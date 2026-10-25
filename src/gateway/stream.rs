//! SSE 语义边界识别（§13.4）。
//!
//! 故障切换的分水岭不是"收到了响应"，而是"下游已经看到了有语义的内容"。在
//! 只收到 HTTP 头、空白行、注释、ping 或协议开始标记时，上游还没有产生任何
//! 成本，明确的错误仍然可以换个目标重试；一旦发出文本、思考、工具参数或引用
//! 增量，就禁止拼接第二个上游——那会让用户看到两段互相矛盾的回答。
//!
//! 这里只做**浅解析**：逐帧看事件名和少数几个字段，绝不重建整个 JSON（§19.4）。

use std::time::Duration;

use axum::body::Bytes;
use futures::{Stream, StreamExt as _};

use crate::domain::Protocol;

/// 在放弃嗅探前最多缓冲多少字节。
///
/// 正常上游几百字节内就会给出第一个语义事件；缓冲上限存在只是为了防止某个
/// 代理送来无穷无尽的注释行把内存吃光。触顶后直接放行，宁可失去切换机会也
/// 不能失去响应。
const MAX_SNIFF_BYTES: usize = 64 * 1024;

/// 首字之前的心跳：为什么必须有，以及为什么它不改变切换边界（§13.4 修订）。
///
/// **为什么必须有心跳。** 网关在嗅探到语义事件之前不构造响应头，这段时间里
/// 下游收不到任何字节。上游的首字延迟动辄几十秒到几分钟（长上下文 + 思考），
/// 中间任何一层 CDN 都会把"零字节"判成源站无响应：
///
/// - Cloudflare 的源站响应超时约 100 秒，超时即回 **524**；
/// - EdgeOne 对响应慢极有耐心，但长挂连接会推高回源建连压力，间接放大 **522**。
///
/// 实测：同一份 6.8 MB 请求体，EdgeOne 侧 76 秒后放行 200，Cloudflare 侧
/// 129.9 秒后返回 524；TCP/TLS 都在 0.2 秒内完成，随后是全程零字节。
///
/// 心跳用 SSE 注释帧（`: keep-alive`）。它是 SSE 规范的一部分，三个协议的
/// 客户端都忽略它，网关自己的嗅探器也把它归入 `Verdict::Pending`
/// （见本文件 `classify` 对注释行的处理），因此**不影响切换边界**：
/// 注释帧到达不改变"下游还没看到语义内容"这一事实。
///
/// 间隔与宽限期都是可配的（`Settings::keep_alive_interval` 与
/// `Settings::first_content_grace`），因为最窄的那层 CDN 超时因部署而异。
///
/// 一帧 SSE 注释心跳。三个协议共用同一种写法。
pub fn keep_alive_frame() -> axum::body::Bytes {
    axum::body::Bytes::from_static(b": keep-alive\n\n")
}

/// 首个**语义**事件真正到达的时刻，由响应体写入、结算时读取。
///
/// 心跳把"客户端收到第一个字节"提前到了语义内容之前，因此不能再拿
/// `Success.first_byte` 当首字延迟：那个数字会变成"第一个注释帧"的时间，
/// 而用户真正等的是回答的第一句话。预算耗尽后由响应体自己记时，
/// 结算优先用它；拿不到就退回原来的口径（§6.6：不知道就是不知道）。
pub type FirstContentClock = std::sync::Arc<std::sync::Mutex<Option<Duration>>>;

/// 新建一个首字时钟。
pub fn first_content_clock() -> FirstContentClock {
    std::sync::Arc::new(std::sync::Mutex::new(None))
}

/// 记下首个语义事件的到达时刻；只记第一次。
pub fn mark_first_content(clock: &FirstContentClock, at: Duration) {
    if let Ok(mut slot) = clock.lock()
        && slot.is_none()
    {
        *slot = Some(at);
    }
}

/// 读出首个语义事件的到达时刻。
pub fn first_content(clock: &FirstContentClock) -> Option<Duration> {
    clock.lock().ok().and_then(|slot| *slot)
}

/// 嗅探结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 还只是协议噪声，继续等下一块。
    Pending,
    /// 已经出现有语义的增量，从此禁止切换。
    Semantic,
    /// 语义内容出现之前的明确错误，仍可切换。
    Error(String),
}

/// 逐块喂入上游字节流，判断"下游是否已经看到有语义的内容"。
pub struct Sniffer {
    protocol: Protocol,
    buffer: Vec<u8>,
    /// 已经解析到哪个字节，避免每来一块就重扫全部缓冲。
    scanned: usize,
}

impl Sniffer {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            buffer: Vec::with_capacity(4096),
            scanned: 0,
        }
    }

    /// 喂入一块字节。
    pub fn push(&mut self, chunk: &[u8]) -> Verdict {
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() >= MAX_SNIFF_BYTES {
            return Verdict::Semantic;
        }

        // SSE 帧以空行分隔。只解析已经完整收到的帧。
        while let Some(end) = find_frame_end(&self.buffer, self.scanned) {
            let frame = String::from_utf8_lossy(&self.buffer[self.scanned..end]).into_owned();
            self.scanned = end;
            match self.classify(&frame) {
                Verdict::Pending => continue,
                verdict => return verdict,
            }
        }
        Verdict::Pending
    }

    /// 取走已缓冲的字节，交给下游作为响应体的前缀。
    ///
    /// 只在**放弃这个嗅探器**时使用：扫描位置不归零，继续喂新块会漏掉帧边界。
    /// 提交之后还要接着嗅探的路径用 [`Self::flush_unemitted`]。
    pub fn take_buffer(&mut self) -> Bytes {
        Bytes::from(std::mem::take(&mut self.buffer))
    }

    /// 取走缓冲区里"还没交给下游"的字节，供"提交后继续嗅探"使用。
    ///
    /// 与 [`Self::take_buffer`] 的关键差别：**缓冲区不清空**，扫描位置也不归零。
    /// 心跳提交（§13.4 修订）会先把已经收到的非语义字节交给下游，然后继续用
    /// 同一个嗅探器判断后面来的块是不是语义内容——而一个语义帧完全可能正好被
    /// 切在两次读取之间。把已发出的字节留在缓冲区里当上下文，那个帧的尾巴才能
    /// 和后面的块拼成完整帧；清空缓冲区会让它永远认不出来，整条流被当成噪声，
    /// 最后还会补一个"上游在产生内容前就结束"的假错误帧。
    ///
    /// `emitted` 是调用方记住的"已经交给下游的字节数"，返回后推进到当前长度。
    /// 缓冲区的增长由 `push` 的上限兜住（触顶即判 `Semantic`）。
    /// 取走缓冲区里**完整帧**的字节，可能被切开的尾帧留在缓冲区里。
    ///
    /// 心跳提交时用它决定"先发什么"：心跳是**独立的 SSE 注释帧**，只有落在
    /// 帧边界上才是合法的——若把一个还没收全的帧的前半段先发出去，紧跟其后的
    /// 心跳会插进这个帧的中间（注释行把 data 行截断），客户端拿到的 JSON
    /// 就是坏的。所以先发到最后一个完整帧为止，尾帧留着等后续字节补齐。
    pub fn flush_complete_frames(&mut self, emitted: &mut usize) -> Bytes {
        let end = last_complete_frame_end(&self.buffer);
        let start = (*emitted).min(end);
        // 游标只前进，绝不倒退：调用方可能在 flush_unemitted 之后又回到这里，
        // 让游标退回 end 会把已经发过的字节再发一遍。
        *emitted = (*emitted).max(end);
        Bytes::copy_from_slice(&self.buffer[start..end])
    }

    /// 取走缓冲区里所有还没交给下游的字节（含不完整的尾帧）。
    ///
    /// 只在**确认要提交**时使用：那一刻起不再发心跳，字节可以原样往下送，
    /// 不完整的尾帧会和后续块在客户端侧自然拼回。
    pub fn flush_unemitted(&mut self, emitted: &mut usize) -> Bytes {
        let start = (*emitted).min(self.buffer.len());
        let pending = Bytes::copy_from_slice(&self.buffer[start..]);
        *emitted = self.buffer.len();
        pending
    }

    fn classify(&self, frame: &str) -> Verdict {
        let mut event = None;
        let mut data = Vec::new();
        for line in frame.lines() {
            let line = line.trim_end_matches('\r');
            // 注释行（以冒号开头）是心跳，不是内容。
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(value) = line.strip_prefix("event:") {
                event = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("data:") {
                data.push(value.trim().to_string());
            }
        }
        let data = data.join("\n");
        if data == "[DONE]" {
            // 完整走完却一个字都没产出：这属于损坏响应，交由调用方切换。
            return Verdict::Pending;
        }
        let payload: Option<serde_json::Value> = if data.is_empty() {
            None
        } else {
            serde_json::from_str(&data).ok()
        };

        if let Some(message) = error_message(event.as_deref(), payload.as_ref()) {
            return Verdict::Error(message);
        }
        match self.protocol {
            Protocol::AnthropicMessages => anthropic(event.as_deref(), payload.as_ref()),
            Protocol::OpenAiChat => openai_chat(payload.as_ref()),
            Protocol::OpenAiResponses => openai_responses(event.as_deref(), payload.as_ref()),
        }
    }
}

/// 缓冲区里最后一个**完整** SSE 帧的结束位置（一个都没有时是 0）。
///
/// 心跳只能落在帧边界上：注释帧插进一个没收全的帧里会把它的 data 行截断。
fn last_complete_frame_end(buffer: &[u8]) -> usize {
    let mut end = 0;
    while let Some(next) = find_frame_end(buffer, end) {
        end = next;
    }
    end
}

/// 找到从 `from` 开始的第一个完整 SSE 帧的结束位置。
fn find_frame_end(buffer: &[u8], from: usize) -> Option<usize> {
    let mut index = from;
    while index + 1 < buffer.len() {
        if buffer[index] == b'\n' && buffer[index + 1] == b'\n' {
            return Some(index + 2);
        }
        if index + 3 < buffer.len() && &buffer[index..index + 4] == b"\r\n\r\n" {
            return Some(index + 4);
        }
        index += 1;
    }
    None
}

/// 三个协议共用的错误事件识别。
///
/// 除了通用的 `event: error` 形状，还必须认 **Responses 的失败收尾**
///（§13.4 修订）。`response.failed` 是上游"这次回答失败了"的正式表达，
/// 形状与 `event: error` 完全不同：错误嵌在 `response.error` 里，事件名也不是
/// `error`。只认前者会让一条失败的流被判成"已经出现语义内容"——于是既不换号、
/// 也不计失败，坏答案被原样端给下游（现场：可达鸭的 gpt-6.1-sol 流）。
fn error_message(event: Option<&str>, payload: Option<&serde_json::Value>) -> Option<String> {
    let is_error_event = matches!(event, Some("error"))
        || payload
            .and_then(|value| value.get("type"))
            .and_then(serde_json::Value::as_str)
            == Some("error");
    let error = payload.and_then(|value| value.get("error"));
    if is_error_event || error.is_some() {
        let message = error
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("上游返回了错误事件");
        return Some(message.chars().take(200).collect());
    }
    responses_failure(event_kind(event, payload), payload)
}

/// Responses 的失败收尾：返回可以安全转达的原因文本，不是失败时返回 `None`。
///
/// 认三种形状，都是上游真的把这次回答判成失败：
/// `event: response.failed` / `response.error`、负载里同名的 `type`，以及终止
/// 对象里的 `response.status == "failed"`。
fn responses_failure(kind: Option<&str>, payload: Option<&serde_json::Value>) -> Option<String> {
    if !responses_failed(kind, payload) {
        return None;
    }
    Some(responses_failure_text(payload))
}

/// 事件名或负载形状是不是 Responses 的失败收尾。
fn responses_failed(kind: Option<&str>, payload: Option<&serde_json::Value>) -> bool {
    matches!(kind, Some("response.failed" | "response.error"))
        || payload
            .and_then(|value| value.get("response"))
            .and_then(|response| response.get("status"))
            .and_then(serde_json::Value::as_str)
            == Some("failed")
}

/// 失败收尾里能拿到的原因：优先 `response.error.message`，再退到顶层 `message`。
///
/// 拿不到具体文案时给固定兜底——绝不把整帧 JSON 塞进记录或错误文本。
fn responses_failure_text(payload: Option<&serde_json::Value>) -> String {
    payload
        .and_then(|value| value.get("response"))
        .and_then(|response| response.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            payload
                .and_then(|value| value.get("message"))
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("上游返回了失败响应")
        .chars()
        .take(200)
        .collect()
}

/// Anthropic：`message_start` 与 `ping` 是协议开始标记，不是内容。
///
/// 除这两种之外的任何事件都按语义内容处理——包括不认识的事件。把合法流误判
/// 为"还没开始"的代价是整条流被当成损坏响应报 502；把噪声误判为内容的代价只
/// 是少一次切换机会。两相比较，必须偏向后者。
fn anthropic(event: Option<&str>, payload: Option<&serde_json::Value>) -> Verdict {
    match event_kind(event, payload) {
        Some("message_start") | Some("ping") => Verdict::Pending,
        Some(_) => Verdict::Semantic,
        None if payload.is_some() => Verdict::Semantic,
        None => Verdict::Pending,
    }
}

/// OpenAI Chat：只有 role 或空字段的首块是协议开始标记，带内容的增量才算语义。
fn openai_chat(payload: Option<&serde_json::Value>) -> Verdict {
    let Some(payload) = payload else {
        return Verdict::Pending;
    };
    let Some(choices) = payload.get("choices").and_then(serde_json::Value::as_array) else {
        // 不认识的形状（例如兼容站点自定义的事件）：宁可提前放行。
        return Verdict::Semantic;
    };
    // `choices: []` 是部分实现的心跳块，或者是 `include_usage` 的收尾块。
    let Some(first) = choices.first() else {
        return Verdict::Pending;
    };
    if first.get("finish_reason").is_some_and(|r| !r.is_null()) {
        return Verdict::Semantic;
    }
    match first.get("delta").and_then(serde_json::Value::as_object) {
        // OpenAI 的首块通常是 `{"role":"assistant","content":""}`：role 与空
        // 字符串都不是内容。
        Some(delta) => {
            let substantive = delta
                .iter()
                .any(|(field, value)| field != "role" && !is_blank(value));
            if substantive {
                Verdict::Semantic
            } else {
                Verdict::Pending
            }
        }
        // 没有 delta 的 choice（completions 风格的 `text`）直接放行。
        None => Verdict::Semantic,
    }
}

/// Responses：`response.created` / `in_progress` / `queued` 是开始标记。
fn openai_responses(event: Option<&str>, payload: Option<&serde_json::Value>) -> Verdict {
    match event_kind(event, payload) {
        Some("response.created" | "response.in_progress" | "response.queued") => Verdict::Pending,
        Some(_) => Verdict::Semantic,
        None if payload.is_some() => Verdict::Semantic,
        None => Verdict::Pending,
    }
}

/// 事件类型：优先取 `event:` 行，退回到 JSON 里的 `type` 字段。
fn event_kind<'a>(
    event: Option<&'a str>,
    payload: Option<&'a serde_json::Value>,
) -> Option<&'a str> {
    event
        .filter(|e| !e.is_empty())
        .or_else(|| payload?.get("type")?.as_str())
}

fn is_blank(value: &serde_json::Value) -> bool {
    value.is_null()
        || value.as_str().is_some_and(str::is_empty)
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
}

/// 把已缓冲的前缀接在剩余流之前，让下游拿到完整的字节序列。
pub fn prepend<S>(prefix: Bytes, rest: S) -> impl Stream<Item = std::io::Result<Bytes>>
where
    S: Stream<Item = std::io::Result<Bytes>>,
{
    futures::stream::once(async move { Ok(prefix) }).chain(rest)
}

/// 从非流式响应体里取出输出 Token 数，用于 TPM 归还与输出速度统计。
///
/// 三个协议的字段名不同，但都在 `usage` 下：OpenAI 用 `completion_tokens` 或
/// `output_tokens`，Anthropic 用 `output_tokens`。
pub fn output_tokens(body: &serde_json::Value) -> Option<u64> {
    let usage = body.get("usage")?;
    ["completion_tokens", "output_tokens"]
        .iter()
        .find_map(|field| usage.get(*field).and_then(serde_json::Value::as_u64))
}

/// TPM 结算使用完整 usage，而性能统计仍只使用输出 Token。
pub fn usage_tokens(body: &serde_json::Value) -> Option<u64> {
    let usage = body.get("usage")?;
    let input = ["prompt_tokens", "input_tokens"]
        .iter()
        .find_map(|field| usage.get(*field).and_then(serde_json::Value::as_u64));
    let output = ["completion_tokens", "output_tokens"]
        .iter()
        .find_map(|field| usage.get(*field).and_then(serde_json::Value::as_u64));
    usage
        .get("total_tokens")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            input
                .zip(output)
                .map(|(input, output)| input.saturating_add(output))
        })
}

/// 非流式响应里的（输入 Token、输出 Token），用于请求记录与成本页（§6.6、§6.8）。
///
/// Anthropic 把缓存读写单独上报，必须并入输入侧；上游没给字段就返回 `None`，
/// 绝不估算。
pub fn usage_parts(body: &serde_json::Value) -> (Option<u64>, Option<u64>) {
    // 走同一份解析：`input` 的「含不含缓存」在三个协议间本来就不一致，
    // 两处各写一遍必然分叉——记录里的 input_tokens 与成本页会开始互相矛盾。
    let breakdown = usage_breakdown(body);
    (breakdown.input, breakdown.output)
}

/// 从一块 Responses SSE 字节里取出上游声明的响应 ID（§15.1）。
///
/// `response.created` 帧的 `response.id` 是整条流唯一确定的身份；后面的帧
/// 不会改变它。找不到时返回 `None`——调用方在更多字节到达后重试。
pub fn first_response_id(chunk: &[u8]) -> Option<String> {
    let mut reader = crate::protocol::sse::FrameReader::new();
    let frames = reader.push(chunk);
    for frame in frames.into_iter().chain(reader.finish()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame.data) else {
            continue;
        };
        let kind = frame
            .event
            .as_deref()
            .or_else(|| value.get("type")?.as_str());
        if kind.is_some_and(|event| event.starts_with("response."))
            && let Some(id) = value
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
        {
            return Some(id.to_string());
        }
    }
    None
}

/// 这一帧是不是该协议的"回答写完了"终止信号（§9.3 修订）。
///
/// 三个协议的终止形状不同，但都只在真正收尾时出现：
/// * **OpenAI Chat** —— 某个 choice 的 `finish_reason` 非空。
/// * **Anthropic** —— `message_delta.stop_reason`，或 `message_stop` 事件。
/// * **OpenAI Responses** —— `response.completed` / `incomplete` / `failed`。
///
/// 不能用"帧里有没有 usage"代替：Anthropic 开头就报用量（见
/// [`StreamAccounting::answer_completed`]）。
fn answers_stop_signal(protocol: Protocol, kind: Option<&str>, value: &serde_json::Value) -> bool {
    match protocol {
        Protocol::OpenAiChat => value
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice
                        .get("finish_reason")
                        .is_some_and(|reason| !reason.is_null())
                })
            }),
        Protocol::AnthropicMessages => {
            kind == Some("message_stop")
                || value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .is_some_and(|reason| !reason.is_null())
        }
        Protocol::OpenAiResponses => matches!(
            kind,
            Some("response.completed" | "response.incomplete" | "response.failed")
        ),
    }
}

/// 流式响应的完成态与用量收集器（§26.3）。
///
/// 同协议透传不重编码字节，但结算仍然需要知道"这条流最终成功了没有、实际
/// 用了多少 Token、Responses 的最终响应对象是什么"。这里只对**完整 SSE 帧**
/// 做浅解析，并且只在帧里出现 usage、错误或结束事件时才真正解析 JSON；任何
/// 解析失败都直接忽略——统计绝不能影响转发本身。
pub struct StreamAccounting {
    protocol: Protocol,
    reader: crate::protocol::sse::FrameReader,
    error: Option<String>,
    usage: UsageBreakdown,
    total_tokens: Option<u64>,
    finished: Option<serde_json::Value>,
    /// 上游有没有发出"这一轮回答写完了"的终止信号（§9.3 修订）。
    ///
    /// 三个协议的终止信号形状不同，但语义一致：Chat 的 `finish_reason`、
    /// Anthropic 的 `stop_reason`、Responses 的 `response.completed`。
    stop_signalled: bool,
}

/// 一次请求的 Token 细分（§11.6）。
///
/// 每一项都是"上游报了才有"，缺失留 None。绝不估算、绝不用 0 冒充已知。
/// 三个协议字段名不同，统一在这里归一。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageBreakdown {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

/// 从上游响应体里读出 Token 细分（§11.6）。
pub fn usage_breakdown(body: &serde_json::Value) -> UsageBreakdown {
    let Some(usage) = body.get("usage") else {
        return UsageBreakdown::default();
    };
    let field = |name: &str| usage.get(name).and_then(serde_json::Value::as_u64);
    // 缓存与思考的字段名按协议各不相同：
    // - Anthropic：cache_read_input_tokens / cache_creation_input_tokens；
    // - Chat：prompt_tokens_details.cached_tokens / completion_tokens_details.reasoning_tokens；
    // - Responses：input_tokens_details.cached_tokens / output_tokens_details.reasoning_tokens。
    //
    // **两种 OpenAI 形状都必须认。** 只认 Chat 的形状会让整个 Responses 协议族的
    // 缓存读写与思考 Token 在记录里恒为空——现场 2264 条 Responses 请求里只有 50 条
    // 有缓存读、思考 Token 是 0 条，而同期 Chat 的缓存读上报率是 61.9%（§11.6）。
    let cache_read =
        field("cache_read_input_tokens").or_else(|| crate::protocol::cache_read_tokens(usage));
    let cache_write = field("cache_creation_input_tokens");
    // `input` 一律是**全部**输入 Token（含缓存读写），与 `Usage::input` 的
    // 定义一致（见 protocol::canonical）。
    //
    // 麻烦在于字段名重名而语义相反：**OpenAI Responses 和 Anthropic 都用
    // `input_tokens`**，但 Responses 的已含缓存、Anthropic 的不含
    // （Chat 则用 `prompt_tokens`，同样已含）。所以不能按字段名判断，
    // 要看协议自己的标记：缓存读写放在**顶层** `cache_*_input_tokens` 的
    // 只有 Anthropic，OpenAI 两种协议都藏在 details 子对象里。
    //
    // 漏了这一步，Anthropic 非流式的分母会偏小、命中率被 clamp 成 1.0，
    // 而流式侧的 `absorb_anthropic_usage` 是加了的——两条路径给出两个答案。
    let anthropic_shape = field("cache_read_input_tokens").is_some()
        || field("cache_creation_input_tokens").is_some();
    let input = match field("prompt_tokens") {
        Some(prompt) => Some(prompt),
        None => field("input_tokens").map(|input| {
            if anthropic_shape {
                input.saturating_add(cache_read.unwrap_or(0) + cache_write.unwrap_or(0))
            } else {
                input
            }
        }),
    };
    UsageBreakdown {
        input,
        output: field("completion_tokens").or_else(|| field("output_tokens")),
        cache_read,
        cache_write,
        reasoning: crate::protocol::reasoning_tokens(usage),
    }
}

/// 这一次请求的缓存命中率：缓存读 Token ÷ 全部输入 Token（0-1）。
///
/// 两个前提都必须成立才给值：上游报了缓存读字段，且报了输入总量。缺任何一项
/// 都返回 `None` —— 拿「没上报」当「没命中」会让不做缓存的站点无故背 0 分。
pub fn cache_hit_rate(usage: UsageBreakdown) -> Option<f64> {
    let read = usage.cache_read?;
    let input = usage.input?;
    (input > 0).then(|| (read as f64 / input as f64).clamp(0.0, 1.0))
}

impl StreamAccounting {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            reader: crate::protocol::sse::FrameReader::new(),
            error: None,
            usage: UsageBreakdown::default(),
            total_tokens: None,
            finished: None,
            stop_signalled: false,
        }
    }

    /// 喂入一块**下游**字节；必须在同一块交给客户端之前调用。
    pub fn push(&mut self, chunk: &[u8]) {
        for frame in self.reader.push(chunk) {
            self.observe(&frame);
        }
        // 这里的字节是我们自己发出去的，正常不会触顶。触顶说明发射器写出了一个
        // 超大的坏帧——用量会从这一刻起统计不到，必须记下来而不是让结算静默
        // 按"没有 usage"处理（§19.4）。
        if let Some(reason) = self.reader.overflow()
            && self.error.is_none()
        {
            self.error = Some(format!("下游流出现超限帧，用量统计已中断：{reason}"));
        }
    }

    /// 流结束时处理残留的半帧。
    pub fn finish(&mut self) {
        if let Some(frame) = self.reader.finish() {
            self.observe(&frame);
        }
    }

    /// 流内错误事件：这条流在语义上已经失败，即使 HTTP 头早就发出去了。
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// TPM 结算用的完整用量；上游没有给足字段时返回 `None`，绝不估算。
    pub fn usage_tokens(&self) -> Option<u64> {
        self.total_tokens.or_else(|| {
            self.usage
                .input
                .zip(self.usage.output)
                .map(|(input, output)| input.saturating_add(output))
        })
    }

    /// 输出 Token，供吞吐评分使用。
    /// 完整的 Token 细分，写进请求记录（§11.6）。
    pub fn usage_breakdown(&self) -> UsageBreakdown {
        self.usage
    }

    pub fn output_tokens(&self) -> Option<u64> {
        self.usage.output
    }

    /// 输入 Token（Anthropic 已并入缓存读写）；拿不到就是 `None`。
    pub fn input_tokens(&self) -> Option<u64> {
        self.usage.input
    }

    /// 上游有没有把这次回答**写完**（§9.3 修订）。
    ///
    /// 判据是协议自己的**终止信号**，不是"有没有出现过 usage"。这一点必须说清楚，
    /// 因为"看用量"两个方向都会骗人：
    ///
    /// - **输入侧用量出现得很早。** Anthropic 的 `message_start.message.usage` 就带
    ///   `input_tokens` 与缓存字段，那时回答一个字都没写。
    /// - **输出侧用量也不可靠。** 真实 Anthropic 的 `message_start` 里
    ///   `output_tokens` 是 `1`（占位值），此时同样什么都没写完。
    ///
    /// 所以只认终止信号：Chat 的 `finish_reason`、Anthropic 的 `stop_reason`、
    /// Responses 的 `response.completed` / `incomplete` / `failed`。三者都只在
    /// 上游真的收尾时才出现，跨协议语义一致。
    pub fn answer_completed(&self) -> bool {
        self.stop_signalled
    }

    /// Responses：`response.completed` / `incomplete` / `failed` 里的最终对象。
    pub fn finished_response(&self) -> Option<&serde_json::Value> {
        self.finished.as_ref()
    }

    fn observe(&mut self, frame: &crate::protocol::sse::Frame) {
        if frame.is_done_marker() || frame.data.is_empty() {
            return;
        }
        let raw = frame.data.as_bytes();
        let event = frame.event.as_deref();
        // 终止信号也必须算"有意思"：Chat 的 finish_reason 帧、Anthropic 的
        // message_stop 帧都不带 usage，漏掉它们就判定不了"上游写完了没有"
        // （§9.3 修订）。这里只做廉价的字节探测，解析失败仍然直接忽略。
        let interesting = event.is_some_and(|name| {
            name.starts_with("response.") || name == "error" || name == "message_stop"
        }) || has(raw, b"usage")
            || has(raw, b"\"error\"")
            || has(raw, b"finish_reason")
            || has(raw, b"stop_reason")
            || has(raw, b"response.completed")
            || has(raw, b"response.incomplete")
            || has(raw, b"response.failed");
        if !interesting {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame.data) else {
            return;
        };
        let kind = event.or_else(|| value.get("type").and_then(serde_json::Value::as_str));
        if kind == Some("error") || value.get("error").is_some() {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("上游返回了错误事件");
            self.error = Some(message.chars().take(200).collect());
            return;
        }
        // Responses 的失败收尾（§9.3 修订）：`response.failed` 是一次失败，不是
        // "上游把这次回答写完了"。字节早已发给下游、换不了号，但这条流必须记成
        // 失败：结算侧据此把 Completed 改判为 Failed（不计成功、写 error_code）。
        let failure = responses_failure(kind, Some(&value));
        if let Some(message) = failure.as_ref() {
            self.error = Some(message.clone());
        }
        // 先认终止信号，再吸收用量：收尾事件往往同时带上用量。
        if failure.is_none() && answers_stop_signal(self.protocol, kind, &value) {
            self.stop_signalled = true;
        }
        match self.protocol {
            Protocol::OpenAiChat => self.absorb_usage(&value),
            Protocol::AnthropicMessages => {
                // Anthropic 的输入用量在 `message_start.message.usage`，输出用量
                // 在收尾的 `message_delta.usage`。
                let usage = match kind {
                    Some("message_start") => value
                        .get("message")
                        .and_then(|message| message.get("usage"))
                        .or_else(|| value.get("usage")),
                    _ => value.get("usage"),
                };
                if let Some(usage) = usage {
                    self.absorb_anthropic_usage(usage, kind != Some("message_start"));
                }
            }
            Protocol::OpenAiResponses => {
                let response = value.get("response").unwrap_or(&value);
                if failure.is_none()
                    && matches!(
                        kind,
                        Some("response.completed" | "response.incomplete" | "response.failed")
                    )
                {
                    self.finished = Some(response.clone());
                }
                if let Some(usage) = response.get("usage") {
                    self.absorb_usage(usage);
                }
            }
        }
    }

    /// OpenAI 形状的 usage（Chat 收尾块与 Responses 共用字段名）。
    fn absorb_usage(&mut self, container: &serde_json::Value) {
        let usage = container.get("usage").unwrap_or(container);
        if let Some(total) = usage
            .get("total_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            self.total_tokens = Some(total);
        }
        if let Some(input) = usage
            .get("prompt_tokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(serde_json::Value::as_u64)
        {
            self.usage.input = Some(input);
        }
        if let Some(output) = usage
            .get("completion_tokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(serde_json::Value::as_u64)
        {
            self.usage.output = Some(output);
        }
        // 缓存与思考在 OpenAI 侧藏在 details 子对象里（§11.6）。Chat 与
        // Responses 用的父字段名不同，两种都要认，否则流式记录里会整片丢失
        // （见 protocol::cache_read_tokens 的说明）。
        if let Some(cached) = crate::protocol::cache_read_tokens(usage) {
            self.usage.cache_read = Some(cached);
        }
        if let Some(reasoning) = crate::protocol::reasoning_tokens(usage) {
            self.usage.reasoning = Some(reasoning);
        }
    }

    /// Anthropic 的 `usage`：缓存读写单独上报，必须并入输入侧。
    fn absorb_anthropic_usage(&mut self, usage: &serde_json::Value, final_output: bool) {
        let field = |name: &str| usage.get(name).and_then(serde_json::Value::as_u64);
        if let Some(input) = field("input_tokens") {
            let cache = field("cache_creation_input_tokens").unwrap_or(0)
                + field("cache_read_input_tokens").unwrap_or(0);
            // Anthropic 的 input_tokens 不含缓存部分；记录里要的是总数，
            // 同时缓存读写各自单独留一列（§11.6）。
            self.usage.input = Some(input.saturating_add(cache));
            self.usage.cache_read = field("cache_read_input_tokens");
            self.usage.cache_write = field("cache_creation_input_tokens");
        }
        // message_start 的 0/1 是开场占位，不是本轮的输出用量。
        // 中途断开仍保留真实输入，但输出未知，不能退款或计算虚假的速度。
        if final_output && let Some(output) = field("output_tokens") {
            self.usage.output = Some(output);
        }
        if final_output && let Some(total) = field("total_tokens") {
            self.total_tokens = Some(total);
        }
    }
}

/// 不分配字符串的字节子串查找。
fn has(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(protocol: Protocol, frames: &[&str]) -> Verdict {
        let mut sniffer = Sniffer::new(protocol);
        let mut last = Verdict::Pending;
        for frame in frames {
            last = sniffer.push(frame.as_bytes());
            if last != Verdict::Pending {
                return last;
            }
        }
        last
    }

    #[test]
    fn protocol_start_markers_still_allow_switching() {
        // §13.4：只收到头、空白、注释、ping 或开始标记时仍可切换。
        assert_eq!(
            feed(
                Protocol::AnthropicMessages,
                &[
                    ": ping\n\n",
                    "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                    "event: ping\ndata: {\"type\":\"ping\"}\n\n",
                ]
            ),
            Verdict::Pending
        );
    }

    #[test]
    fn the_first_text_delta_closes_the_switching_window() {
        assert_eq!(
            feed(
                Protocol::AnthropicMessages,
                &[
                    "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"你\"}}\n\n",
                ]
            ),
            Verdict::Semantic
        );
    }

    #[test]
    fn an_error_before_any_content_is_switchable() {
        let verdict = feed(
            Protocol::AnthropicMessages,
            &[
                "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n",
            ],
        );
        assert_eq!(verdict, Verdict::Error("overloaded".into()));
    }

    #[test]
    fn an_openai_role_only_first_chunk_is_not_semantic() {
        // 只有 role 的首块是协议开始标记，还没有任何内容。
        assert_eq!(
            feed(
                Protocol::OpenAiChat,
                &["data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n"]
            ),
            Verdict::Pending
        );
        // OpenAI 官方的首块带一个空字符串 content，同样不算内容。
        assert_eq!(
            feed(
                Protocol::OpenAiChat,
                &[
                    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n"
                ]
            ),
            Verdict::Pending
        );
        assert_eq!(
            feed(
                Protocol::OpenAiChat,
                &[
                    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n",
                ]
            ),
            Verdict::Semantic
        );
    }

    #[test]
    fn unrecognised_shapes_commit_rather_than_being_declared_broken() {
        // 把合法流误判为"还没开始"的代价是整条流被报成 502；把噪声误判为内容
        // 的代价只是少一次切换机会。不认识的形状必须偏向放行。
        assert_eq!(
            feed(
                Protocol::OpenAiChat,
                &["data: {\"object\":\"proxy.custom\",\"text\":\"hi\"}\n\n"]
            ),
            Verdict::Semantic
        );
        assert_eq!(
            feed(
                Protocol::AnthropicMessages,
                &["event: vendor_extension\ndata: {\"type\":\"vendor_extension\"}\n\n"]
            ),
            Verdict::Semantic
        );
        assert_eq!(
            feed(
                Protocol::OpenAiResponses,
                &["event: response.reasoning_summary_text.delta\ndata: {\"delta\":\"…\"}\n\n"]
            ),
            Verdict::Semantic
        );
    }

    #[test]
    fn empty_choice_lists_are_heartbeats() {
        assert_eq!(
            feed(Protocol::OpenAiChat, &["data: {\"choices\":[]}\n\n"]),
            Verdict::Pending
        );
    }

    #[test]
    fn tool_call_arguments_count_as_semantic_content() {
        // 工具参数增量同样不可回滚：客户端可能已经开始渲染函数调用。
        assert_eq!(
            feed(
                Protocol::OpenAiChat,
                &["data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}\n\n"]
            ),
            Verdict::Semantic
        );
    }

    #[test]
    fn responses_start_events_are_not_semantic_but_deltas_are() {
        assert_eq!(
            feed(
                Protocol::OpenAiResponses,
                &["event: response.created\ndata: {\"type\":\"response.created\"}\n\n"]
            ),
            Verdict::Pending
        );
        assert_eq!(
            feed(
                Protocol::OpenAiResponses,
                &[
                    "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"嗨\"}\n\n",
                ]
            ),
            Verdict::Semantic
        );
    }

    #[test]
    fn first_response_id_supports_crlf_frames() {
        assert_eq!(
            first_response_id(
                b"event: response.created\r\ndata: {\"response\":{\"id\":\"resp_upstream\"}}\r\n\r\n"
            ),
            Some("resp_upstream".into())
        );
    }

    #[test]
    fn a_frame_split_across_chunks_is_still_parsed() {
        // TCP 不保证帧边界：半个事件也必须能正确拼起来。
        let mut sniffer = Sniffer::new(Protocol::AnthropicMessages);
        assert_eq!(sniffer.push(b"event: content_block"), Verdict::Pending);
        assert_eq!(sniffer.push(b"_delta\ndata: {\"delta\":"), Verdict::Pending);
        assert_eq!(sniffer.push(b"{\"text\":\"x\"}}\n\n"), Verdict::Semantic);
    }

    #[test]
    fn crlf_framing_is_handled() {
        assert_eq!(
            feed(
                Protocol::AnthropicMessages,
                &["event: content_block_delta\r\ndata: {\"delta\":{\"text\":\"x\"}}\r\n\r\n"]
            ),
            Verdict::Semantic
        );
    }

    #[test]
    fn the_buffered_prefix_is_handed_to_the_client_intact() {
        let mut sniffer = Sniffer::new(Protocol::AnthropicMessages);
        let start = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
        let delta = "event: content_block_delta\ndata: {\"delta\":{\"text\":\"x\"}}\n\n";
        sniffer.push(start.as_bytes());
        sniffer.push(delta.as_bytes());

        // 嗅探期间缓冲的字节一个都不能丢，否则客户端会收到残缺的流。
        let buffered = sniffer.take_buffer();
        assert_eq!(buffered, Bytes::from(format!("{start}{delta}")));
    }

    #[test]
    fn a_flood_of_comments_eventually_commits_instead_of_growing_forever() {
        let mut sniffer = Sniffer::new(Protocol::OpenAiChat);
        let chunk = b": keep-alive\n\n";
        let mut verdict = Verdict::Pending;
        for _ in 0..(MAX_SNIFF_BYTES / chunk.len() + 2) {
            verdict = sniffer.push(chunk);
            if verdict != Verdict::Pending {
                break;
            }
        }
        assert_eq!(verdict, Verdict::Semantic, "宁可失去切换机会也不能吃光内存");
        assert!(sniffer.buffer.len() <= MAX_SNIFF_BYTES + chunk.len());
    }

    #[test]
    fn keep_alive_frames_are_comments_so_they_never_commit() {
        // 心跳必须落在"还没语义内容"这一侧：否则网关会把自己的注释帧
        // 当成下游已看到内容，切换边界被自己堵死。
        let frame = keep_alive_frame();
        assert_eq!(frame.as_ref(), b": keep-alive\n\n");
        for protocol in [
            Protocol::AnthropicMessages,
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
        ] {
            let mut sniffer = Sniffer::new(protocol);
            assert_eq!(sniffer.push(&frame), Verdict::Pending);
        }
    }

    #[test]
    fn complete_frames_are_released_while_a_partial_tail_stays_buffered() {
        // 心跳提交时的分工：收全的帧立刻交给下游，没收全的尾帧留在缓冲区里
        // 等后半段——心跳只能落在帧边界上，插进半个帧里会截断它的 data 行。
        let mut sniffer = Sniffer::new(Protocol::AnthropicMessages);
        assert_eq!(
            sniffer.push(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n"),
            Verdict::Pending
        );
        // 第二个帧只来一半：它还没结束，不能发。
        assert_eq!(
            sniffer.push(b"event: content_block_delta\ndata: {\"delta\":{\"text\":\"x\""),
            Verdict::Pending
        );
        let mut emitted = 0;
        let ready = sniffer.flush_complete_frames(&mut emitted);
        assert!(
            ready.starts_with(b"event: message_start"),
            "完整帧要发出去：{ready:?}"
        );
        assert!(
            !ready.windows(9).any(|w| w == b"content_b"),
            "没收全的尾帧不能发：{ready:?}"
        );
        // 后半段到齐：这个帧终于完整了，嗅探器这才认出它是语义内容。
        assert_eq!(
            sniffer.push(b"}}\n\n"),
            Verdict::Semantic,
            "补上后半段之后，这个帧必须被认成语义内容"
        );
    }

    #[test]
    fn the_emitted_cursor_only_moves_forward() {
        // flush_unemitted（提交）之后又回到 flush_complete_frames（心跳）时，
        // 游标不能倒退，否则同一段字节会被发第二遍。
        let mut sniffer = Sniffer::new(Protocol::AnthropicMessages);
        sniffer.push(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n");
        let mut emitted = 0;
        let _ = sniffer.flush_complete_frames(&mut emitted);
        assert_eq!(emitted, sniffer.buffer.len());
        let _ = sniffer.flush_unemitted(&mut emitted);
        assert_eq!(emitted, sniffer.buffer.len());
        let again = sniffer.flush_complete_frames(&mut emitted);
        assert!(again.is_empty(), "已经发过的字节不能再发一遍：{again:?}");
        assert_eq!(emitted, sniffer.buffer.len(), "游标不能倒退");
    }

    #[test]
    fn the_first_content_clock_keeps_only_the_first_mark() {
        let clock = first_content_clock();
        assert_eq!(first_content(&clock), None);
        mark_first_content(&clock, Duration::from_secs(30));
        mark_first_content(&clock, Duration::from_secs(99));
        assert_eq!(first_content(&clock), Some(Duration::from_secs(30)));
    }

    #[test]
    fn usage_is_read_from_either_protocol() {
        assert_eq!(
            output_tokens(&serde_json::json!({"usage": {"completion_tokens": 128}})),
            Some(128)
        );
        assert_eq!(
            output_tokens(&serde_json::json!({"usage": {"output_tokens": 64}})),
            Some(64)
        );
        assert_eq!(output_tokens(&serde_json::json!({})), None);
    }

    /// 回归：Responses 把缓存读放在 `input_tokens_details`，不是 Chat 的
    /// `prompt_tokens_details`。只认后者会让整个 Responses 协议族的缓存读
    /// 与思考 Token 在记录里恒为空。
    #[test]
    fn responses_detail_fields_are_read_not_just_chat_ones() {
        let breakdown = usage_breakdown(&serde_json::json!({
            "usage": {
                "input_tokens": 1000,
                "output_tokens": 50,
                "input_tokens_details": {"cached_tokens": 768},
                "output_tokens_details": {"reasoning_tokens": 32},
            }
        }));
        assert_eq!(breakdown.input, Some(1000));
        assert_eq!(breakdown.output, Some(50));
        assert_eq!(
            breakdown.cache_read,
            Some(768),
            "Responses 的缓存读必须被认出来"
        );
        assert_eq!(
            breakdown.reasoning,
            Some(32),
            "Responses 的思考 Token 必须被认出来"
        );

        // Responses 的 input_tokens **已经包含**缓存读，不能再加一次。
        // 它的字段名和 Anthropic 一样叫 input_tokens，但语义相反——
        // 按字段名判断会让 Responses 的命中率分母虚高、永远算不出高分。
        assert_eq!(
            cache_hit_rate(breakdown),
            Some(0.768),
            "768/1000，分母就是上游给的 input_tokens，不再补缓存"
        );

        // Chat 的形状继续照常工作，两种协议共用一个解析器。
        let chat = usage_breakdown(&serde_json::json!({
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 50,
                "prompt_tokens_details": {"cached_tokens": 640},
                "completion_tokens_details": {"reasoning_tokens": 16},
            }
        }));
        assert_eq!(chat.cache_read, Some(640));
        assert_eq!(chat.reasoning, Some(16));
        assert_eq!(cache_hit_rate(chat), Some(0.64), "640/1000");
    }

    /// 缓存命中率的**分母口径必须与分子一致**（§9.4 第五维）。
    ///
    /// 三个协议对 `input` 的定义本来就不一样：
    /// - OpenAI 的 `prompt_tokens` / `input_tokens` 已经含缓存读；
    /// - Anthropic 的 `input_tokens` **不含**缓存读写，流式侧
    ///   `absorb_anthropic_usage` 会把它加回来。
    ///
    /// 所以非流式的 `usage_breakdown` 也必须加，否则非流式 Anthropic 的
    /// 命中率会是 `cache_read / 不含缓存的输入`——分母偏小，命中率被 clamp
    /// 到 1.0，一个只命中三成的账号看起来像全命中。
    #[test]
    fn anthropic_cache_hit_uses_the_same_denominator_on_both_paths() {
        // 真实 Anthropic 形状：输入 100（不含缓存），缓存读 300、写 600。
        let body = serde_json::json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_read_input_tokens": 300,
                "cache_creation_input_tokens": 600,
            }
        });
        let breakdown = usage_breakdown(&body);
        assert_eq!(
            breakdown.input,
            Some(1000),
            "全部输入 = 100 + 300 + 600；漏加缓存会让命中率超过 30% 的实际值"
        );
        assert_eq!(
            cache_hit_rate(breakdown),
            Some(0.3),
            "300/1000，不是 300/100"
        );

        // Anthropic 完全不上报缓存时不补任何东西：input_tokens 就是全部输入。
        // 这条挡住「把所有带 input_tokens 的都当成 Anthropic」的过度修正。
        let plain = usage_breakdown(&serde_json::json!({
            "usage": {"input_tokens": 100, "output_tokens": 5}
        }));
        assert_eq!(plain.input, Some(100));
        assert_eq!(cache_hit_rate(plain), None, "没报缓存读就没有命中率可言");

        // 流式路径：同一份 usage 走 StreamAccounting，必须得到同一个命中率。
        let mut accounting = StreamAccounting::new(Protocol::AnthropicMessages);
        feed_all(
            &mut accounting,
            &[
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":1,\"cache_read_input_tokens\":300,\"cache_creation_input_tokens\":600}}}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":50}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ],
        );
        assert_eq!(
            cache_hit_rate(accounting.usage_breakdown()),
            Some(0.3),
            "流式与非流式必须是同一个口径"
        );
    }

    fn feed_all(accounting: &mut StreamAccounting, chunks: &[&str]) {
        for chunk in chunks {
            accounting.push(chunk.as_bytes());
        }
        accounting.finish();
    }

    #[test]
    fn chat_usage_is_settled_from_the_final_chunk() {
        let mut accounting = StreamAccounting::new(Protocol::OpenAiChat);
        feed_all(
            &mut accounting,
            &[
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":900,\"completion_tokens\":10,\"total_tokens\":910}}\n\n",
                "data: [DONE]\n\n",
            ],
        );
        assert_eq!(accounting.usage_tokens(), Some(910));
        assert_eq!(accounting.output_tokens(), Some(10));
        assert_eq!(accounting.error(), None);
    }

    /// `answer_completed` 只认协议的**终止信号**（§9.3 修订）。
    ///
    /// 这条钉的是"看用量"这个想当然的判据有多危险：真实 Anthropic 的
    /// `message_start` 里 `output_tokens` 是 `1`（占位值），输入用量也在那一刻
    /// 就出现。无论看输入侧还是输出侧，都会把"回答还没写"误判成"上游跑完了"。
    #[test]
    fn answer_completed_requires_a_stop_signal_not_just_usage() {
        // 真实的 Anthropic 开头帧：输入用量 + output_tokens 占位值 1。
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":1,\"cache_read_input_tokens\":20}}}\n\n";
        let mut early = StreamAccounting::new(Protocol::AnthropicMessages);
        feed_all(&mut early, &[start]);
        assert_eq!(early.input_tokens(), Some(120));
        assert_eq!(early.output_tokens(), None);
        assert_eq!(early.usage_tokens(), None, "不能按开场占位退还 TPM 预留");
        assert!(
            !early.answer_completed(),
            "开头帧带 output_tokens 占位值也不算写完：上游还没吐正文"
        );

        // 收到 stop_reason 才算写完。
        feed_all(
            &mut early,
            &[
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
            ],
        );
        assert!(early.answer_completed(), "收到 stop_reason 才算写完");

        // message_stop 同样算。
        let mut stopped = StreamAccounting::new(Protocol::AnthropicMessages);
        feed_all(
            &mut stopped,
            &["event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"],
        );
        assert!(stopped.answer_completed(), "message_stop 是终止信号");

        // Chat：finish_reason 才算，单纯收到 usage 不算。
        let mut chat = StreamAccounting::new(Protocol::OpenAiChat);
        feed_all(
            &mut chat,
            &[
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":900,\"completion_tokens\":10}}\n\n",
            ],
        );
        assert!(
            !chat.answer_completed(),
            "只有 usage 没有 finish_reason 不算写完"
        );
        feed_all(
            &mut chat,
            &["data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"],
        );
        assert!(chat.answer_completed(), "finish_reason 是终止信号");

        // Responses：response.completed 才算。
        let mut resp = StreamAccounting::new(Protocol::OpenAiResponses);
        feed_all(
            &mut resp,
            &[
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
            ],
        );
        assert!(!resp.answer_completed(), "创建事件不算写完");
        feed_all(
            &mut resp,
            &[
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"output_tokens\":2}}}\n\n",
            ],
        );
        assert!(resp.answer_completed(), "response.completed 是终止信号");
    }

    #[test]
    fn anthropic_usage_merges_start_and_delta_frames() {
        let mut accounting = StreamAccounting::new(Protocol::AnthropicMessages);
        feed_all(
            &mut accounting,
            &[
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":20}}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"x\"}}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
            ],
        );
        assert_eq!(accounting.usage_tokens(), Some(127));
        assert_eq!(accounting.output_tokens(), Some(7));
    }

    /// 回归：流式 Responses 的收尾帧同样带 `input_tokens_details`，结算路径
    /// 必须把缓存读与思考 Token 落到请求记录里（§11.6）。
    #[test]
    fn responses_stream_settles_cache_and_reasoning_details() {
        let mut accounting = StreamAccounting::new(Protocol::OpenAiResponses);
        feed_all(
            &mut accounting,
            &[
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1000,\"output_tokens\":50,\"total_tokens\":1050,\"input_tokens_details\":{\"cached_tokens\":768},\"output_tokens_details\":{\"reasoning_tokens\":32}}}}\n\n",
            ],
        );
        let usage = accounting.usage_breakdown();
        assert_eq!(usage.input, Some(1000));
        assert_eq!(usage.output, Some(50));
        assert_eq!(
            usage.cache_read,
            Some(768),
            "流式 Responses 的缓存读必须被结算"
        );
        assert_eq!(
            usage.reasoning,
            Some(32),
            "流式 Responses 的思考 Token 必须被结算"
        );
    }

    #[test]
    fn responses_completion_keeps_the_final_object_and_usage() {
        let mut accounting = StreamAccounting::new(Protocol::OpenAiResponses);
        feed_all(
            &mut accounting,
            &[
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\"}],\"usage\":{\"input_tokens\":5,\"output_tokens\":6,\"total_tokens\":11}}}\n\n",
            ],
        );
        assert_eq!(accounting.usage_tokens(), Some(11));
        assert_eq!(accounting.output_tokens(), Some(6));
        let finished = accounting.finished_response().expect("最终响应对象");
        assert_eq!(finished["status"], "completed");
        assert_eq!(finished["output"][0]["type"], "message");
    }

    #[test]
    fn frames_split_across_network_chunks_are_still_settled() {
        let mut accounting = StreamAccounting::new(Protocol::OpenAiChat);
        accounting.push(b"data: {\"choices\":[],\"usage\":{\"prompt_tok");
        accounting.push(b"ens\":3,\"completion_tokens\":4,\"total_tokens\":7}}\n\n");
        accounting.finish();
        assert_eq!(accounting.usage_tokens(), Some(7));
    }

    #[test]
    fn a_stream_error_event_marks_the_stream_failed() {
        let mut accounting = StreamAccounting::new(Protocol::OpenAiResponses);
        feed_all(
            &mut accounting,
            &["event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"boom\"}}\n\n"],
        );
        assert_eq!(accounting.error(), Some("boom"));
        assert_eq!(accounting.usage_tokens(), None, "失败流不编造用量");
    }

    /// 现场回归（可达鸭的 gpt-6.1-sol）：上游等到最后才回 `response.failed`，
    /// 此前一个语义块都没有。旧逻辑把它判成"已经有语义内容"，于是既不换号、
    /// 也不计失败，坏答案被原样端给下游。
    #[test]
    fn a_failed_response_before_any_content_is_switchable() {
        let verdict = feed(
            Protocol::OpenAiResponses,
            &[
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n",
                "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"上游超时\"}}}\n\n",
            ],
        );
        assert_eq!(verdict, Verdict::Error("上游超时".into()));
    }

    /// 已经在写正文之后才失败：换不了号，但这条流必须记成失败，否则这笔会被
    /// 记成一次成功、账号永远扣不到分。
    #[test]
    fn a_failed_response_after_content_is_recorded_as_a_fault() {
        let mut accounting = StreamAccounting::new(Protocol::OpenAiResponses);
        feed_all(
            &mut accounting,
            &[
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
                "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"status\":\"failed\",\"error\":{\"message\":\"boom\"}}}\n\n",
            ],
        );
        assert_eq!(accounting.error(), Some("boom"));
        assert!(
            !accounting.answer_completed(),
            "失败的回答不算写完，否则这笔会被记成成功"
        );
        assert_eq!(accounting.finished_response(), None);
    }

    /// 没有具体文案时用固定兜底，绝不把整帧 JSON 端出去。
    #[test]
    fn a_bare_failed_response_still_yields_a_readable_reason() {
        let mut accounting = StreamAccounting::new(Protocol::OpenAiResponses);
        feed_all(
            &mut accounting,
            &[
                "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\n\n",
            ],
        );
        assert_eq!(accounting.error(), Some("上游返回了失败响应"));
    }

    /// 回归：正常的 `response.completed` 不能被这次修订影响。
    #[test]
    fn a_completed_response_still_counts_as_completed() {
        let mut accounting = StreamAccounting::new(Protocol::OpenAiResponses);
        feed_all(
            &mut accounting,
            &[
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"total_tokens\":7}}}\n\n",
            ],
        );
        assert_eq!(accounting.error(), None);
        assert!(accounting.answer_completed());
        assert!(accounting.finished_response().is_some());
        assert_eq!(accounting.usage_tokens(), Some(7));
    }

    /// `response.incomplete` 是"没写完就收尾"，不是失败：上游确实把这次回答
    /// 结束了（例如打到 max_output_tokens），不能据此熔断账号。
    #[test]
    fn an_incomplete_response_is_not_a_failure() {
        let mut accounting = StreamAccounting::new(Protocol::OpenAiResponses);
        feed_all(
            &mut accounting,
            &[
                "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
            ],
        );
        assert_eq!(accounting.error(), None);
        assert!(accounting.answer_completed());
    }
}
