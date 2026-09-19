//! 跨协议的响应转换与流式转发（§14.5、§13.4）。
//!
//! 同协议路径不经过这里：那条路上字节原样转发，一次都不重新编码。只有上游
//! 端点与下游协议不同时，才需要把上游的事件流解析成中间事件、再按下游协议
//! 重新发射。
//!
//! **切换边界的判据换成了中间事件**：`message_start`、`response.created`、
//! 只有 role 的首块都不是语义内容，此时上游还没产生成本，明确的错误仍可换
//! 目标；一旦出现文本、思考或工具参数增量就禁止拼接第二个上游。

use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use futures::StreamExt as _;

use crate::domain::Protocol;
use crate::gateway::error::ErrorCode;
use crate::protocol::{self, sse};

/// 放弃嗅探前最多缓冲多少字节。
///
/// 触顶后直接提交：宁可失去切换机会，也不能让一个只发注释的代理吃光内存。
const MAX_BUFFER_BYTES: usize = 64 * 1024;

/// 完整 Responses 事件可能包含最终输出，但未结束的帧不能无限占用内存。
const MAX_RESPONSE_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// 一次已提交的流式响应。
pub struct Committed {
    pub body: Body,
    /// 首个语义事件的延迟，用于评分。
    pub first_token: Duration,
}

/// 转换失败的原因，由调用方决定是否切换目标。
#[derive(Debug)]
pub struct Failure {
    pub code: ErrorCode,
    pub message: String,
}

impl Failure {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 读取上游流，转换成下游协议后提交。
///
/// 在第一个语义事件之前返回 `Err` 表示"还没花钱，可以换目标"；一旦提交，
/// 后续错误只能按下游协议发一个终止错误事件（§13.4）。
pub async fn commit_stream(request: StreamRequest<'_>) -> Result<Committed, Failure> {
    let StreamRequest {
        upstream,
        downstream,
        include_usage,
        account,
        mut response,
        responses_id,
        request_id,
        degraded,
    } = request;
    let started = Instant::now();
    let mut reader = sse::FrameReader::new();
    let mut parser = protocol::StreamParser::new(upstream);
    let mut emitter = protocol::StreamEmitter::new(downstream, include_usage, responses_id);
    let mut prefix: Vec<Bytes> = Vec::new();
    let mut buffered = 0usize;

    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            // 一个语义事件都没有就结束：这是损坏响应，允许换目标（§13.2）。
            Ok(None) => {
                return Err(Failure::new(
                    ErrorCode::UpstreamProtocolError,
                    format!("账号「{account}」的流式响应在产生内容前就结束"),
                ));
            }
            Err(error) => {
                return Err(Failure::new(
                    if error.is_timeout() {
                        ErrorCode::UpstreamTimeout
                    } else {
                        ErrorCode::UpstreamExhausted
                    },
                    format!("账号「{account}」的流式响应中断"),
                ));
            }
        };

        buffered += chunk.len();
        // 单帧超限说明上游不是在发 SSE。提交之前发现就还能换目标（§13.2）。
        if let Some(reason) = reader.overflow() {
            return Err(Failure::new(ErrorCode::UpstreamProtocolError, reason));
        }
        let mut frames = reader.push(&chunk).into_iter();
        while let Some(frame) = frames.next() {
            if let Some(message) = protocol::frame_error(&frame) {
                return Err(Failure::new(
                    ErrorCode::UpstreamProtocolError,
                    format!("账号「{account}」的流式响应报错：{message}"),
                ));
            }
            let mut committed = false;
            for event in parser.push(&frame) {
                committed |= event.is_semantic();
                prefix.extend(emitter.push(&event));
            }
            // 解析阶段丢掉的东西要立刻收进 sink，不能等流结束（§14.8）。
            drain_degraded(&mut parser, &degraded);
            if committed {
                // 同一块里还没处理的帧必须一起交出去，否则它们连同上游已经
                // 生成的内容一起消失。
                let pending: Vec<sse::Frame> = frames.collect();
                return Ok(Committed {
                    first_token: started.elapsed(),
                    body: continue_stream(StreamPlumbing {
                        prefix,
                        degraded,
                        pending,
                        reader,
                        parser,
                        emitter,
                        response,
                        account,
                        request_id,
                    }),
                });
            }
        }

        if buffered >= MAX_BUFFER_BYTES {
            return Ok(Committed {
                first_token: started.elapsed(),
                body: continue_stream(StreamPlumbing {
                    prefix,
                    degraded,
                    pending: Vec::new(),
                    reader,
                    parser,
                    emitter,
                    response,
                    account,
                    request_id,
                }),
            });
        }
    }
}

/// 继续消费上游所需要的全部管道对象。
///
/// 打包成一个结构体而不是八个参数：这些字段总是一起传递，散开之后既容易
/// 传错位置，也超出函数参数个数的合理范围。
struct StreamPlumbing<'a> {
    prefix: Vec<Bytes>,
    degraded: DegradationSink,
    /// 提交那一刻还留在同一块字节里、尚未处理的帧。
    pending: Vec<sse::Frame>,
    reader: sse::FrameReader,
    parser: protocol::StreamParser,
    emitter: protocol::StreamEmitter,
    response: reqwest::Response,
    account: &'a str,
    request_id: &'a str,
}

/// 一次跨协议流式转换的全部输入。
///
/// 打包成结构体而不是八个参数：这些字段总是一起传递，散开之后既容易传错位置，
/// 也超出函数参数个数的合理范围。
pub struct StreamRequest<'a> {
    /// 上游协议：决定怎么解析帧。
    pub upstream: Protocol,
    /// 下游协议：决定怎么发射帧。
    pub downstream: Protocol,
    /// 下游是否要求 usage 事件。
    pub include_usage: bool,
    /// 账号名，只用于日志与错误文案。
    pub account: &'a str,
    /// 上游响应体。
    pub response: reqwest::Response,
    /// Responses 入口的网关 ID；决定要不要重写响应 ID。
    pub responses_id: Option<String>,
    /// 网关请求 ID，写进流内错误帧（§18.2）。
    pub request_id: &'a str,
    /// 解析阶段的能力降级收集器（§14.8）。
    pub degraded: DegradationSink,
}

/// 共享的"这次流丢了什么能力"收集器（§14.8）。
///
/// 解析器在流的中途才会发现丢东西（例如 Anthropic 的签名增量出现在思考块
/// 末尾），而请求记录要到流结束才写，两边必须共享同一个位置。
pub type DegradationSink = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// 新建一个收集器。
pub fn degradation_sink() -> DegradationSink {
    std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// 把解析器刚记下的降级搬进共享收集器。
fn drain_degraded(parser: &mut protocol::StreamParser, sink: &DegradationSink) {
    let taken = parser.take_degraded();
    if taken.is_empty() {
        return;
    }
    if let Ok(mut sink) = sink.lock() {
        for capability in taken {
            let name = capability.to_string();
            if !sink.contains(&name) {
                sink.push(name);
            }
        }
    }
}

/// 提交之后继续消费上游，把剩余事件转换给下游。
fn continue_stream(plumbing: StreamPlumbing<'_>) -> Body {
    let StreamPlumbing {
        prefix,
        degraded,
        pending,
        mut reader,
        mut parser,
        mut emitter,
        response,
        account,
        request_id,
    } = plumbing;
    let account = account.to_string();
    let request_id = request_id.to_string();
    let stream = async_stream::stream! {
        for bytes in prefix {
            yield Ok::<Bytes, std::io::Error>(bytes);
        }
        for frame in pending {
            if let Some(message) = protocol::frame_error(&frame) {
                yield Ok(emitter.error(
                    ErrorCode::UpstreamProtocolError,
                    &message,
                    Some(&request_id),
                ));
                return;
            }
            for event in parser.push(&frame) {
                for bytes in emitter.push(&event) {
                    yield Ok(bytes);
                }
            }
            drain_degraded(&mut parser, &degraded);
        }

        let mut upstream = response.bytes_stream();
        let mut broken = false;
        while let Some(item) = upstream.next().await {
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(_) => {
                    broken = true;
                    break;
                }
            };
            for frame in reader.push(&chunk) {
                if let Some(message) = protocol::frame_error(&frame) {
                    // 已经提交，只能在流内报错（§18.2）；绝不伪造正常完成。
                    yield Ok(emitter.error(
                        ErrorCode::UpstreamProtocolError,
                        &message,
                        Some(&request_id),
                    ));
                    return;
                }
                for event in parser.push(&frame) {
                    for bytes in emitter.push(&event) {
                        yield Ok(bytes);
                    }
                }
                drain_degraded(&mut parser, &degraded);
            }
            // 单帧超限：已经提交，同样只能在流内报错。不报的话下游会收到一个
            // 突然断掉的流，无从判断是正常结束还是上游坏了（§14.8、§19.4）。
            if let Some(reason) = reader.overflow() {
                yield Ok(emitter.error(
                    ErrorCode::UpstreamProtocolError,
                    reason,
                    Some(&request_id),
                ));
                return;
            }
        }

        if broken {
            yield Ok(emitter.error(
                ErrorCode::UpstreamExhausted,
                &format!("账号「{account}」的上游流式响应中断"),
                Some(&request_id),
            ));
            return;
        }
        // 上游正常结束：补齐下游协议要求的收尾事件。
        if let Some(frame) = reader.finish() {
            for event in parser.push(&frame) {
                for bytes in emitter.push(&event) {
                    yield Ok(bytes);
                }
            }
            drain_degraded(&mut parser, &degraded);
        }
        for event in parser.finish() {
            for bytes in emitter.push(&event) {
                yield Ok(bytes);
            }
        }
        for bytes in emitter.done() {
            yield Ok(bytes);
        }
    };
    Body::from_stream(stream)
}

/// 同协议路径的流式转发：字节原样送出，但中断时补一个下游协议的错误事件。
///
/// 不伪造正常完成事件掩盖上游中断（§13.4）。
pub fn passthrough_stream(
    prefix: Bytes,
    response: reqwest::Response,
    downstream: Protocol,
    request_id: &str,
) -> Body {
    let request_id = request_id.to_string();
    let stream = async_stream::stream! {
        yield Ok::<Bytes, std::io::Error>(prefix);
        let mut upstream = response.bytes_stream();
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => yield Ok(chunk),
                Err(_) => {
                    // 流已经开始，稳定错误码与请求 ID 只能写进事件体（§18.2）。
                    yield Ok(protocol::stream_error(
                        downstream,
                        ErrorCode::UpstreamExhausted,
                        "上游流式响应中断",
                        Some(&request_id),
                    ));
                    return;
                }
            }
        }
    };
    Body::from_stream(stream)
}

/// 同协议 Responses 流式转发：把上游响应 ID 替换为网关 ID。
///
/// 只重写 `response.created` / `response.in_progress` / `response.completed` /
/// `response.incomplete` 事件里 `response.id` 字段；其余字节原样转发。ID 的
/// 绝不出现两次：客户端引用的地址从第一个事件起就固定是网关 ID（§15.1）。
pub fn passthrough_responses_stream(
    prefix: Bytes,
    response: reqwest::Response,
    gateway_id: &str,
    request_id: &str,
) -> Body {
    let gateway_id = gateway_id.to_string();
    let request_id = request_id.to_string();
    let stream = async_stream::stream! {
        let mut rewriter = ResponseIdRewriter::new(&gateway_id);
        match rewriter.push(&prefix) {
            Ok(Some(bytes)) => yield Ok::<Bytes, std::io::Error>(bytes),
            Ok(None) => {},
            Err(message) => {
                yield Ok(protocol::stream_error(
                    Protocol::OpenAiResponses,
                    ErrorCode::UpstreamProtocolError,
                    message,
                    Some(&request_id),
                ));
                return;
            }
        }
        let mut upstream = response.bytes_stream();
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    match rewriter.push(&chunk) {
                        Ok(Some(bytes)) => yield Ok(bytes),
                        Ok(None) => {},
                        Err(message) => {
                            yield Ok(protocol::stream_error(
                                Protocol::OpenAiResponses,
                                ErrorCode::UpstreamProtocolError,
                                message,
                                Some(&request_id),
                            ));
                            return;
                        }
                    }
                }
                Err(_) => {
                    yield Ok(protocol::stream_error(
                        Protocol::OpenAiResponses,
                        ErrorCode::UpstreamExhausted,
                        "上游流式响应中断",
                        Some(&request_id),
                    ));
                    return;
                }
            }
        }
        if let Some(bytes) = rewriter.finish() {
            yield Ok(bytes);
        }
    };
    Body::from_stream(stream)
}

/// 跨网络分块重组 SSE 帧，再替换 Responses 事件中的 `response.id`。
///
/// reqwest 的 chunk 边界不等于 SSE 帧边界。必须把未完成的帧留到下一块，
/// 否则上游 ID 可能在恰好被分块的位置泄漏给客户端。
struct ResponseIdRewriter {
    buffer: Vec<u8>,
    gateway_id: String,
}

impl ResponseIdRewriter {
    fn new(gateway_id: &str) -> Self {
        Self {
            buffer: Vec::with_capacity(4096),
            gateway_id: gateway_id.to_string(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Option<Bytes>, &'static str> {
        let mut output = Vec::new();
        for segment in chunk.split_inclusive(|byte| *byte == b'\n') {
            if self.buffer.len().saturating_add(segment.len()) > MAX_RESPONSE_FRAME_BYTES {
                self.buffer.clear();
                return Err("上游 Responses 事件超过 8 MiB 上限");
            }
            self.buffer.extend_from_slice(segment);
            // 每个分段最多追加一行，只需检查末尾，避免碎片输入反复扫描整帧。
            let from = self.buffer.len().saturating_sub(4);
            if sse::find_frame_end(&self.buffer, from).is_some() {
                output.extend_from_slice(&rewrite_frame(&self.buffer, &self.gateway_id));
                self.buffer.clear();
            }
        }
        Ok((!output.is_empty()).then(|| Bytes::from(output)))
    }

    fn finish(&mut self) -> Option<Bytes> {
        let rest = std::mem::take(&mut self.buffer);
        if rest.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
        Some(rewrite_frame(&rest, &self.gateway_id))
    }
}

fn rewrite_frame(raw: &[u8], gateway_id: &str) -> Bytes {
    let Ok(text) = std::str::from_utf8(raw) else {
        return Bytes::copy_from_slice(raw);
    };
    let frame = sse::parse_frame(text);
    let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(&frame.data) else {
        return Bytes::copy_from_slice(raw);
    };
    let kind = frame
        .event
        .as_deref()
        .or_else(|| payload.get("type")?.as_str());
    if !kind.is_some_and(|event| event.starts_with("response.")) {
        return Bytes::copy_from_slice(raw);
    }
    let Some(object) = payload
        .get_mut("response")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Bytes::copy_from_slice(raw);
    };
    if !object.contains_key("id") {
        return Bytes::copy_from_slice(raw);
    }
    object.insert("id".into(), serde_json::json!(gateway_id));
    let mut out = String::with_capacity(text.len() + 16);
    let mut wrote_data = false;
    for line in text.lines().filter(|line| !line.is_empty()) {
        if line.starts_with("data:") {
            if !wrote_data {
                out.push_str("data: ");
                out.push_str(&payload.to_string());
                out.push('\n');
                wrote_data = true;
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// 起一台只吐固定 SSE 的假上游，返回它的 `reqwest::Response`。
    async fn upstream(frames: &'static str) -> reqwest::Response {
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || async move {
                ([("content-type", "text/event-stream")], frames).into_response()
            }),
        );
        use axum::response::IntoResponse as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        reqwest::get(format!("http://{addr}/")).await.unwrap()
    }

    async fn text_of(body: Body) -> String {
        String::from_utf8(to_bytes(body, 1 << 20).await.unwrap().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_chat_stream_becomes_a_messages_stream() {
        let response = upstream(
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;

        let committed = commit_stream(StreamRequest {
            upstream: Protocol::OpenAiChat,
            downstream: Protocol::AnthropicMessages,
            include_usage: true,
            account: "账号A",
            response,
            responses_id: None,
            request_id: "req_test",
            degraded: degradation_sink(),
        })
        .await
        .unwrap();
        let text = text_of(committed.body).await;

        assert!(text.contains("event: message_start"));
        assert!(text.contains("你好"));
        assert!(text.contains("event: message_stop"));
    }

    #[tokio::test]
    async fn an_error_before_any_content_is_switchable() {
        let response = upstream(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"1\"}}\n\n\
             event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n",
        )
        .await;

        let failure = commit_stream(StreamRequest {
            upstream: Protocol::AnthropicMessages,
            downstream: Protocol::OpenAiChat,
            include_usage: false,
            account: "账号A",
            response,
            responses_id: None,
            request_id: "req_test",
            degraded: degradation_sink(),
        })
        .await
        .err()
        .expect("语义内容之前的错误必须可切换");
        assert_eq!(failure.code, ErrorCode::UpstreamProtocolError);
        assert!(failure.message.contains("overloaded"));
    }

    #[tokio::test]
    async fn a_stream_that_produces_nothing_is_a_broken_response() {
        let response =
            upstream("event: message_start\ndata: {\"type\":\"message_start\"}\n\n").await;
        let failure = commit_stream(StreamRequest {
            upstream: Protocol::AnthropicMessages,
            downstream: Protocol::OpenAiChat,
            include_usage: false,
            account: "账号A",
            response,
            responses_id: None,
            request_id: "req_test",
            degraded: degradation_sink(),
        })
        .await
        .err()
        .expect("一个字都没产出属于损坏响应");
        assert_eq!(failure.code, ErrorCode::UpstreamProtocolError);
    }

    #[tokio::test]
    async fn an_error_after_content_terminates_the_stream_without_faking_completion() {
        let response = upstream(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"1\"}}\n\n\
             event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"半\"}}\n\n\
             event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"boom\"}}\n\n",
        )
        .await;

        let committed = commit_stream(StreamRequest {
            upstream: Protocol::AnthropicMessages,
            downstream: Protocol::OpenAiChat,
            include_usage: false,
            account: "账号A",
            response,
            responses_id: None,
            request_id: "req_test",
            degraded: degradation_sink(),
        })
        .await
        .unwrap();
        let text = text_of(committed.body).await;
        assert!(text.contains("半"), "已经发出的内容必须送达");
        assert!(text.contains("boom"), "提交后只能在流内报错");
        assert!(!text.contains("[DONE]"), "不得伪造正常完成");
    }

    #[test]
    fn responses_ids_are_rewritten_across_chunk_and_crlf_boundaries() {
        let mut rewriter = ResponseIdRewriter::new("resp_akh_gateway");
        let chunks = [
            b"event: response.created\r\ndata: {\"response\":{\"id\":\"resp_".as_slice(),
            b"upstream\"}}\r\n\r\n",
            b"event: response.completed\ndata: {\"response\":{\"id\":\"resp_upstream\"}}\n\n",
        ];
        let mut output = String::new();
        for chunk in chunks {
            if let Some(bytes) = rewriter.push(chunk).unwrap() {
                output.push_str(std::str::from_utf8(&bytes).unwrap());
            }
        }
        if let Some(bytes) = rewriter.finish() {
            output.push_str(std::str::from_utf8(&bytes).unwrap());
        }

        assert_eq!(output.matches("resp_akh_gateway").count(), 2);
        assert!(!output.contains("resp_upstream"));
    }

    #[test]
    fn response_id_rewriting_preserves_utf8_for_every_byte_split() {
        let frame = "event: response.completed\r\ndata: {\"response\":{\"id\":\"resp_upstream\",\"text\":\"中文\"}}\r\n\r\n";
        for split in 0..=frame.len() {
            let mut rewriter = ResponseIdRewriter::new("resp_akh_gateway");
            let mut output = Vec::new();
            for chunk in [&frame.as_bytes()[..split], &frame.as_bytes()[split..]] {
                if let Some(bytes) = rewriter.push(chunk).unwrap() {
                    output.extend_from_slice(&bytes);
                }
            }
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("中文"), "split={split}");
            assert!(text.contains("resp_akh_gateway"), "split={split}");
            assert!(!text.contains("resp_upstream"), "split={split}");
        }
    }

    #[test]
    fn response_id_rewriting_bounds_unterminated_frames() {
        let mut rewriter = ResponseIdRewriter::new("resp_akh_gateway");
        assert!(
            rewriter
                .push(&vec![b'x'; MAX_RESPONSE_FRAME_BYTES])
                .unwrap()
                .is_none()
        );
        assert!(rewriter.push(b"x").is_err());
        assert!(rewriter.buffer.is_empty());
    }

    #[test]
    fn response_id_rewriting_handles_multiline_data_and_final_frames() {
        let mut rewriter = ResponseIdRewriter::new("resp_akh_gateway");
        assert!(
            rewriter
                .push(
                    b"data: {\"type\":\"response.completed\",\n\
            data: \"response\":{\"id\":\"resp_upstream\"}}"
                )
                .unwrap()
                .is_none()
        );
        let bytes = rewriter.finish().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let frame = sse::parse_frame(text);
        let value: serde_json::Value = serde_json::from_str(&frame.data).unwrap();
        assert_eq!(value["response"]["id"], "resp_akh_gateway");
        assert!(!text.contains("resp_upstream"));
    }
}
