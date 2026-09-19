//! SSE 语义边界识别（§13.4）。
//!
//! 故障切换的分水岭不是"收到了响应"，而是"下游已经看到了有语义的内容"。在
//! 只收到 HTTP 头、空白行、注释、ping 或协议开始标记时，上游还没有产生任何
//! 成本，明确的错误仍然可以换个目标重试；一旦发出文本、思考、工具参数或引用
//! 增量，就禁止拼接第二个上游——那会让用户看到两段互相矛盾的回答。
//!
//! 这里只做**浅解析**：逐帧看事件名和少数几个字段，绝不重建整个 JSON（§19.4）。

use axum::body::Bytes;
use futures::{Stream, StreamExt as _};

use crate::domain::Protocol;

/// 在放弃嗅探前最多缓冲多少字节。
///
/// 正常上游几百字节内就会给出第一个语义事件；缓冲上限存在只是为了防止某个
/// 代理送来无穷无尽的注释行把内存吃光。触顶后直接放行，宁可失去切换机会也
/// 不能失去响应。
const MAX_SNIFF_BYTES: usize = 64 * 1024;

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
    pub fn take_buffer(&mut self) -> Bytes {
        Bytes::from(std::mem::take(&mut self.buffer))
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
fn error_message(event: Option<&str>, payload: Option<&serde_json::Value>) -> Option<String> {
    let is_error_event = matches!(event, Some("error"))
        || payload
            .and_then(|value| value.get("type"))
            .and_then(serde_json::Value::as_str)
            == Some("error");
    let error = payload.and_then(|value| value.get("error"));
    if !is_error_event && error.is_none() {
        return None;
    }
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("上游返回了错误事件");
    Some(message.chars().take(200).collect())
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
    let Some(usage) = body.get("usage") else {
        return (None, None);
    };
    let field = |name: &str| usage.get(name).and_then(serde_json::Value::as_u64);
    let input = field("prompt_tokens").or_else(|| {
        field("input_tokens").map(|input| {
            let cache = field("cache_creation_input_tokens").unwrap_or(0)
                + field("cache_read_input_tokens").unwrap_or(0);
            input.saturating_add(cache)
        })
    });
    let output = field("completion_tokens").or_else(|| field("output_tokens"));
    (input, output)
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
    // 缓存与思考的字段名：Anthropic 用 cache_*_input_tokens，OpenAI 用
    // prompt_tokens_details.cached_tokens / completion_tokens_details.reasoning_tokens。
    let details = |parent: &str, name: &str| {
        usage
            .get(parent)
            .and_then(|value| value.get(name))
            .and_then(serde_json::Value::as_u64)
    };
    UsageBreakdown {
        input: field("prompt_tokens").or_else(|| field("input_tokens")),
        output: field("completion_tokens").or_else(|| field("output_tokens")),
        cache_read: field("cache_read_input_tokens")
            .or_else(|| details("prompt_tokens_details", "cached_tokens")),
        cache_write: field("cache_creation_input_tokens"),
        reasoning: field("reasoning_tokens")
            .or_else(|| details("completion_tokens_details", "reasoning_tokens")),
    }
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
        let interesting = event
            .is_some_and(|name| name.starts_with("response.") || name == "error")
            || has(raw, b"usage")
            || has(raw, b"\"error\"")
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
                    self.absorb_anthropic_usage(usage);
                }
            }
            Protocol::OpenAiResponses => {
                let response = value.get("response").unwrap_or(&value);
                if matches!(
                    kind,
                    Some("response.completed" | "response.incomplete" | "response.failed")
                ) {
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
        // 缓存与思考在 OpenAI 侧藏在 details 子对象里（§11.6）。
        let details = |parent: &str, name: &str| {
            usage
                .get(parent)
                .and_then(|value| value.get(name))
                .and_then(serde_json::Value::as_u64)
        };
        if let Some(cached) = details("prompt_tokens_details", "cached_tokens") {
            self.usage.cache_read = Some(cached);
        }
        if let Some(reasoning) = details("completion_tokens_details", "reasoning_tokens") {
            self.usage.reasoning = Some(reasoning);
        }
    }

    /// Anthropic 的 `usage`：缓存读写单独上报，必须并入输入侧。
    fn absorb_anthropic_usage(&mut self, usage: &serde_json::Value) {
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
        if let Some(output) = field("output_tokens") {
            self.usage.output = Some(output);
        }
        if let Some(total) = field("total_tokens") {
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
}
