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
}
