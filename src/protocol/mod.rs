//! 协议层：完整中间格式与六个方向的跨协议转换（§14）。
//!
//! 结构是**星形**而不是六条独立通路：每个协议只实现"解析成中间格式"与"从中
//! 间格式发射"两件事，六个方向由 `解析 + 发射` 组合而成。加第四个协议时新增
//! 的是一个模块，不是六条通路。
//!
//! ```text
//!   Chat ──┐            ┌── Chat
//!  Messages┼── canonical┼── Messages
//! Responses┘            └── Responses
//! ```
//!
//! 同协议路径**永远不经过这里**：透传只改鉴权头、Base URL 和模型名，未知字段
//! 因此天然保留（§14.1）。中间��式只在跨协议时使用。

pub mod anthropic;
pub mod canonical;
pub mod degrade;
pub mod openai_chat;
pub mod openai_responses;
pub mod sse;
pub mod translate;

use serde_json::Value;

use crate::domain::Protocol;
use canonical::{Event, Request, Response};
use degrade::{Emitted, Fidelity, Unsupported};

/// 把下游请求体解析成中间格式。
pub fn parse_request(protocol: Protocol, body: &Value) -> Result<Request, Unsupported> {
    match protocol {
        Protocol::OpenAiChat => openai_chat::parse_request(body),
        Protocol::OpenAiResponses => openai_responses::parse_request(body),
        Protocol::AnthropicMessages => anthropic::parse_request(body),
    }
}

/// 把中间格式发射成上游请求体。
pub fn emit_request(protocol: Protocol, request: &Request) -> Result<Emitted, Unsupported> {
    match protocol {
        Protocol::OpenAiChat => openai_chat::emit_request(request),
        Protocol::OpenAiResponses => openai_responses::emit_request(request),
        Protocol::AnthropicMessages => anthropic::emit_request(request),
    }
}

/// 解析上游的非流式响应体。
pub fn parse_response(protocol: Protocol, body: &Value) -> Result<Response, Unsupported> {
    match protocol {
        Protocol::OpenAiChat => openai_chat::parse_response(body),
        Protocol::OpenAiResponses => openai_responses::parse_response(body),
        Protocol::AnthropicMessages => anthropic::parse_response(body),
    }
}

/// 发射给下游的非流式响应体。
pub fn emit_response(protocol: Protocol, response: &Response) -> Result<Value, Unsupported> {
    match protocol {
        Protocol::OpenAiChat => openai_chat::emit_response(response),
        Protocol::OpenAiResponses => openai_responses::emit_response(response),
        Protocol::AnthropicMessages => anthropic::emit_response(response),
    }
}

/// 一次跨协议转换的结果：目标协议的请求体与保真度。
#[derive(Debug, Clone)]
pub struct Converted {
    pub body: Value,
    pub fidelity: Fidelity,
}

/// 把下游请求体转换到上游协议（§14.3 的第 3–5 步）。
///
/// 同协议直接返回原体：透传路径不做任何重写，未知字段原样保留。
pub fn convert_request(
    from: Protocol,
    to: Protocol,
    body: &Value,
) -> Result<Converted, Unsupported> {
    if from == to {
        return Ok(Converted {
            body: body.clone(),
            fidelity: Fidelity::Lossless,
        });
    }
    let request = parse_request(from, body)?;
    let emitted = emit_request(to, &request)?;
    Ok(Converted {
        fidelity: if emitted.degraded.is_empty() {
            Fidelity::Lossless
        } else {
            Fidelity::Degraded(emitted.degraded.clone())
        },
        body: emitted.body,
    })
}

/// 试算一次转换的保真度，不产生请求体。
///
/// 首次选择时用它给"只能降级表达"的目标降权；故障切换时用它判断白名单外的
/// 能力是否已经越界（§14.8）。
pub fn probe_fidelity(from: Protocol, to: Protocol, body: &Value) -> Result<Fidelity, Unsupported> {
    convert_request(from, to, body).map(|converted| converted.fidelity)
}

/// 把上游响应体转换回下游协议。
pub fn convert_response(from: Protocol, to: Protocol, body: &Value) -> Result<Value, Unsupported> {
    if from == to {
        return Ok(body.clone());
    }
    let response = parse_response(from, body)?;
    emit_response(to, &response)
}

/// 上游 SSE 帧 → 中间事件。
///
/// Chat 需要跨帧状态（工具调用下标、内容项序号），所以解析器是有状态的；
/// 另外两个协议的帧自带足够信息，但为了调用方统一，一律走这个类型。
///
/// 这一层还负责一个**跨协议的不变量**：`Done` 之前必定先有 `Finish`。上游漏
/// 发 stop reason 是兼容站点的常见毛病，而下游客户端（尤其 Chat）拿不到
/// `finish_reason` 就会把响应当成截断，或者一直等下去（§14.5）。
#[derive(Debug)]
pub struct StreamParser {
    inner: Inner,
    /// 是否已经见过 `Finish`。
    finished: bool,
    /// 是否已经发出 `Done`。之后的任何事件都不再放行。
    done: bool,
}

#[derive(Debug)]
enum Inner {
    Chat(openai_chat::StreamParser),
    Responses,
    Anthropic,
}

impl StreamParser {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            inner: match protocol {
                Protocol::OpenAiChat => Inner::Chat(openai_chat::StreamParser::new()),
                Protocol::OpenAiResponses => Inner::Responses,
                Protocol::AnthropicMessages => Inner::Anthropic,
            },
            finished: false,
            done: false,
        }
    }

    /// 解析一帧。返回零到多个中间事件。
    pub fn push(&mut self, frame: &sse::Frame) -> Vec<Event> {
        if frame.is_comment() {
            return Vec::new();
        }
        if frame.is_done_marker() {
            return self.finish();
        }
        let Ok(data) = serde_json::from_str::<Value>(&frame.data) else {
            return Vec::new();
        };
        let events = match &mut self.inner {
            Inner::Chat(parser) => parser.push(&data),
            Inner::Responses => openai_responses::parse_event(frame.event.as_deref(), &data),
            Inner::Anthropic => anthropic::parse_event(frame.event.as_deref(), &data),
        };
        self.guard(events)
    }

    /// 流结束。Chat 需要补出 `Finish` 与 `Done`，另外两个协议自带收尾事件。
    pub fn finish(&mut self) -> Vec<Event> {
        let events = match &mut self.inner {
            Inner::Chat(parser) => parser.finish(),
            _ => Vec::new(),
        };
        self.guard(events)
    }

    /// 保证 `Done` 之前一定有 `Finish`，且 `Done` 只出现一次。
    ///
    /// 后者不是洁癖：上游的 `[DONE]` 帧与"连接正常关闭"都会走到收尾逻辑，少
    /// 了这道闸，下游就会收到两个 `message_stop`，客户端的状态机随即报错。
    fn guard(&mut self, events: Vec<Event>) -> Vec<Event> {
        if self.done {
            return Vec::new();
        }
        let mut guarded = Vec::with_capacity(events.len() + 1);
        for event in events {
            match event {
                Event::Finish { .. } => {
                    self.finished = true;
                    guarded.push(event);
                }
                Event::Done => {
                    if !self.finished {
                        self.finished = true;
                        // 上游没说为什么停，就按最普通的"这一轮说完了"处理，
                        // 绝不猜一个更具体的原因。
                        guarded.push(Event::Finish {
                            stop: canonical::StopReason::EndTurn,
                            stop_sequence: None,
                        });
                    }
                    self.done = true;
                    guarded.push(Event::Done);
                    break;
                }
                _ => guarded.push(event),
            }
        }
        guarded
    }
}

/// 中间事件 → 下游 SSE 帧。
#[derive(Debug)]
pub enum StreamEmitter {
    Chat(openai_chat::StreamEmitter),
    Responses(openai_responses::StreamEmitter),
    Anthropic(anthropic::StreamEmitter),
}

impl StreamEmitter {
    pub fn new(protocol: Protocol, include_usage: bool, responses_id: Option<String>) -> Self {
        match protocol {
            Protocol::OpenAiChat => Self::Chat(openai_chat::StreamEmitter::new(include_usage)),
            Protocol::OpenAiResponses => {
                Self::Responses(openai_responses::StreamEmitter::new(responses_id))
            }
            Protocol::AnthropicMessages => Self::Anthropic(anthropic::StreamEmitter::new()),
        }
    }

    /// 发射一个中间事件，返回可直接写入响应体的字节。
    pub fn push(&mut self, event: &Event) -> Vec<axum::body::Bytes> {
        match self {
            Self::Chat(emitter) => emitter
                .push(event)
                .into_iter()
                .map(|chunk| sse::format_frame(None, &chunk.to_string()))
                .collect(),
            Self::Responses(emitter) => emitter
                .push(event)
                .into_iter()
                .map(|(name, data)| sse::format_frame(Some(&name), &data.to_string()))
                .collect(),
            Self::Anthropic(emitter) => emitter
                .push(event)
                .into_iter()
                .map(|(name, data)| sse::format_frame(Some(&name), &data.to_string()))
                .collect(),
        }
    }

    /// 流正常结束时的收尾字节。只有 Chat 需要 `[DONE]`。
    pub fn done(&self) -> Vec<axum::body::Bytes> {
        match self {
            Self::Chat(_) => vec![axum::body::Bytes::from_static(b"data: [DONE]\n\n")],
            _ => Vec::new(),
        }
    }

    /// 流中途出错时按下游协议发送的终止错误事件（§13.4、§18.2）。
    pub fn error(&self, message: &str) -> axum::body::Bytes {
        stream_error(self.protocol(), message)
    }

    fn protocol(&self) -> Protocol {
        match self {
            Self::Chat(_) => Protocol::OpenAiChat,
            Self::Responses(_) => Protocol::OpenAiResponses,
            Self::Anthropic(_) => Protocol::AnthropicMessages,
        }
    }
}

/// 一帧是否是上游的错误事件；是则给出截断后的安全消息。
///
/// 三个协议的错误帧形状不同，但都能由这三个信号之一识别：`event: error`、
/// `data.type == "error"`、或 `data.error` 存在。
pub fn frame_error(frame: &sse::Frame) -> Option<String> {
    let payload: Option<Value> = serde_json::from_str(&frame.data).ok();
    let is_error_event = frame.event.as_deref() == Some("error")
        || payload
            .as_ref()
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            == Some("error");
    let error = payload.as_ref().and_then(|value| value.get("error"));
    if !is_error_event && error.is_none() {
        return None;
    }
    let message = error
        .and_then(|error| error.get("message"))
        .or_else(|| payload.as_ref()?.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("上游返回了错误事件");
    Some(message.chars().take(200).collect())
}

/// 按协议构造一个可直接写进流的错误帧（§18.2）。
pub fn stream_error(protocol: Protocol, message: &str) -> axum::body::Bytes {
    match protocol {
        Protocol::OpenAiChat => {
            sse::format_frame(None, &openai_chat::error_event(message).to_string())
        }
        Protocol::OpenAiResponses => {
            let (name, data) = openai_responses::error_event(message);
            sse::format_frame(Some(&name), &data.to_string())
        }
        Protocol::AnthropicMessages => {
            let (name, data) = anthropic::error_event(message);
            sse::format_frame(Some(&name), &data.to_string())
        }
    }
}

/// 某个采样参数在目标协议里的字段名；`None` 表示表达不了，按白名单降级。
fn sampling_key(field: &str, target: Protocol) -> Option<&'static str> {
    match (field, target) {
        // 三个协议共有的参数在各自模块里直接处理，这里只管协议独有的。
        ("top_k", Protocol::AnthropicMessages) => Some("top_k"),
        ("frequency_penalty", Protocol::OpenAiChat) => Some("frequency_penalty"),
        ("presence_penalty", Protocol::OpenAiChat) => Some("presence_penalty"),
        ("seed", Protocol::OpenAiChat) => Some("seed"),
        ("logit_bias", Protocol::OpenAiChat) => Some("logit_bias"),
        ("logprobs", Protocol::OpenAiChat) => Some("logprobs"),
        ("top_logprobs", Protocol::OpenAiChat | Protocol::OpenAiResponses) => Some("top_logprobs"),
        ("truncation", Protocol::OpenAiResponses) => Some("truncation"),
        _ => None,
    }
}

fn known(fields: &[&str], name: &str) -> bool {
    fields.contains(&name)
}

/// `stop` / `stop_sequences` 既可能是字符串，也可能是字符串数组。
fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALL: [Protocol; 3] = [
        Protocol::OpenAiChat,
        Protocol::OpenAiResponses,
        Protocol::AnthropicMessages,
    ];

    /// 一个包含文本、图片、工具定义、工具调用与工具结果的请求，按协议给出。
    fn rich_request(protocol: Protocol) -> Value {
        match protocol {
            Protocol::OpenAiChat => json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "你是助手"},
                    {"role": "user", "content": [
                        {"type": "text", "text": "看图"},
                        {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                    ]},
                    {"role": "assistant", "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": {"name": "weather", "arguments": "{\"city\":\"北京\"}"}
                    }]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "晴"}
                ],
                "tools": [{"type": "function", "function": {
                    "name": "weather", "description": "查天气",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }}],
                "max_tokens": 1024
            }),
            Protocol::OpenAiResponses => json!({
                "model": "m",
                "instructions": "你是助手",
                "input": [
                    {"type": "message", "role": "user", "content": [
                        {"type": "input_text", "text": "看图"},
                        {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
                    ]},
                    {"type": "function_call", "call_id": "call_1", "name": "weather",
                     "arguments": "{\"city\":\"北京\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "晴"}
                ],
                "tools": [{"type": "function", "name": "weather", "description": "查天气",
                           "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}],
                "max_output_tokens": 1024
            }),
            Protocol::AnthropicMessages => json!({
                "model": "m",
                "max_tokens": 1024,
                "system": "你是助手",
                "messages": [
                    {"role": "user", "content": [
                        {"type": "text", "text": "看图"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "call_1", "name": "weather", "input": {"city": "北京"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "call_1", "content": "晴"}
                    ]}
                ],
                "tools": [{"name": "weather", "description": "查天气",
                           "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}]
            }),
        }
    }

    #[test]
    fn all_six_directions_preserve_tools_images_and_call_ids() {
        for from in ALL {
            for to in ALL {
                if from == to {
                    continue;
                }
                let converted = convert_request(from, to, &rich_request(from))
                    .unwrap_or_else(|e| panic!("{from:?} → {to:?} 转换失败：{e}"));
                let text = converted.body.to_string();

                assert_eq!(
                    converted.fidelity,
                    Fidelity::Lossless,
                    "{from:?} → {to:?} 不该有任何降级"
                );
                // 白名单外的三件事：工具、图片、角色语义，一件都不能少。
                assert!(text.contains("weather"), "{from:?} → {to:?} 丢了工具定义");
                assert!(text.contains("call_1"), "{from:?} → {to:?} 丢了工具调用 ID");
                assert!(text.contains("北京"), "{from:?} → {to:?} 丢了工具参数");
                assert!(text.contains("AAAA"), "{from:?} → {to:?} 丢了图片");
                assert!(text.contains("你是助手"), "{from:?} → {to:?} 丢了系统提示");
                assert!(text.contains("晴"), "{from:?} → {to:?} 丢了工具结果");

                // 转换结果必须能被目标协议自己解析回来（往返一致）。
                let reparsed = parse_request(to, &converted.body)
                    .unwrap_or_else(|e| panic!("{from:?} → {to:?} 的结果无法回读：{e}"));
                assert_eq!(reparsed.tools.len(), 1);
                assert_eq!(reparsed.tools[0].name, "weather");
            }
        }
    }

    #[test]
    fn same_protocol_requests_are_passed_through_untouched() {
        for protocol in ALL {
            let body = json!({"model": "m", "厂商私有字段": {"保留": true}, "messages": []});
            let converted = convert_request(protocol, protocol, &body).unwrap();
            assert_eq!(converted.body, body, "同协议不做任何重写");
            assert_eq!(converted.fidelity, Fidelity::Lossless);
        }
    }

    #[test]
    fn unknown_fields_block_cross_protocol_but_not_passthrough() {
        let body = json!({
            "model": "m", "messages": [], "max_tokens": 8,
            "厂商私有字段": {"x": 1}
        });
        // 同协议：原样透传。
        assert!(
            convert_request(
                Protocol::AnthropicMessages,
                Protocol::AnthropicMessages,
                &body
            )
            .is_ok()
        );
        // 跨协议：明确拒绝，绝不静默删除（§14.1、§14.6）。
        let error =
            convert_request(Protocol::AnthropicMessages, Protocol::OpenAiChat, &body).unwrap_err();
        assert!(error.to_string().contains("厂商私有字段"));
    }

    #[test]
    fn thinking_is_the_only_capability_that_degrades() {
        // Anthropic 的带签名思考历史转到 Chat：签名无处安放，按白名单降级。
        let body = json!({
            "model": "m", "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "问题"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "推理", "signature": "sig"},
                    {"type": "text", "text": "答案"}
                ]}
            ],
            "thinking": {"type": "enabled", "budget_tokens": 10000}
        });
        let converted =
            convert_request(Protocol::AnthropicMessages, Protocol::OpenAiChat, &body).unwrap();
        assert_eq!(
            converted.fidelity,
            Fidelity::Degraded(vec!["thinking".into()])
        );
        // 降级只丢思考，答案与"要思考"的意图都还在。
        assert!(converted.body.to_string().contains("答案"));
        assert_eq!(converted.body["reasoning_effort"], "high");
    }

    #[test]
    fn structured_output_refuses_rather_than_degrading() {
        // Schema 丢了客户端会解析崩溃，属于白名单外（§14.8）。
        let body = json!({
            "model": "m", "messages": [],
            "response_format": {"type": "json_schema",
                "json_schema": {"name": "o", "schema": {"type": "object"}}}
        });
        assert!(convert_request(Protocol::OpenAiChat, Protocol::AnthropicMessages, &body).is_err());
        // 但 Chat → Responses 有等价表达，必须无损。
        let converted =
            convert_request(Protocol::OpenAiChat, Protocol::OpenAiResponses, &body).unwrap();
        assert_eq!(converted.fidelity, Fidelity::Lossless);
    }

    #[test]
    fn responses_round_trip_across_all_six_directions() {
        let bodies = [
            (
                Protocol::OpenAiChat,
                json!({
                    "id": "1", "model": "m",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "你好"},
                                 "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 2}
                }),
            ),
            (
                Protocol::OpenAiResponses,
                json!({
                    "id": "1", "model": "m", "status": "completed",
                    "output": [{"type": "message", "role": "assistant",
                                "content": [{"type": "output_text", "text": "你好"}]}],
                    "usage": {"input_tokens": 10, "output_tokens": 2}
                }),
            ),
            (
                Protocol::AnthropicMessages,
                json!({
                    "id": "1", "model": "m", "type": "message", "role": "assistant",
                    "content": [{"type": "text", "text": "你好"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 10, "output_tokens": 2}
                }),
            ),
        ];

        for (from, body) in &bodies {
            for to in ALL {
                let converted = convert_response(*from, to, body)
                    .unwrap_or_else(|e| panic!("{from:?} → {to:?} 响应转换失败：{e}"));
                assert!(
                    converted.to_string().contains("你好"),
                    "{from:?} → {to:?} 丢了响应文本"
                );
                let reparsed = parse_response(to, &converted).unwrap();
                assert_eq!(
                    reparsed.usage.output,
                    Some(2),
                    "{from:?} → {to:?} 丢了 usage"
                );
                assert_eq!(reparsed.usage.input, Some(10));
            }
        }
    }

    /// 把一段 SSE 喂进"解析 + 发射"管道，返回下游看到的完整字节。
    fn pipe(from: Protocol, to: Protocol, raw: &str) -> String {
        let mut parser = StreamParser::new(from);
        let mut emitter = StreamEmitter::new(to, true, None);
        let mut reader = sse::FrameReader::new();
        let mut out = Vec::new();
        for frame in reader.push(raw.as_bytes()) {
            for event in parser.push(&frame) {
                out.extend(emitter.push(&event));
            }
        }
        if let Some(frame) = reader.finish() {
            for event in parser.push(&frame) {
                out.extend(emitter.push(&event));
            }
        }
        for event in parser.finish() {
            out.extend(emitter.push(&event));
        }
        out.extend(emitter.done());
        out.iter()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .collect()
    }

    #[test]
    fn streaming_text_survives_all_six_directions() {
        let streams = [
            (
                Protocol::OpenAiChat,
                "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                 data: [DONE]\n\n",
            ),
            (
                Protocol::OpenAiResponses,
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"1\",\"model\":\"m\"}}\n\n\
                 event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n\
                 event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"你好\"}\n\n\
                 event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n",
            ),
            (
                Protocol::AnthropicMessages,
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"1\",\"model\":\"m\"}}\n\n\
                 event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ),
        ];

        for (from, raw) in &streams {
            for to in ALL {
                let out = pipe(*from, to, raw);
                assert!(out.contains("你好"), "{from:?} → {to:?} 流式丢了文本");
                match to {
                    Protocol::OpenAiChat => {
                        assert!(out.contains("chat.completion.chunk"));
                        assert!(out.trim_end().ends_with("data: [DONE]"));
                        assert!(out.contains("\"finish_reason\":\"stop\""));
                    }
                    Protocol::OpenAiResponses => {
                        assert!(out.contains("event: response.created"));
                        assert!(out.contains("event: response.completed"));
                    }
                    Protocol::AnthropicMessages => {
                        assert!(out.contains("event: message_start"));
                        assert!(out.contains("event: message_stop"));
                        assert!(out.contains("\"stop_reason\":\"end_turn\""));
                    }
                }
            }
        }
    }

    #[test]
    fn streaming_tool_calls_survive_all_six_directions() {
        let streams = [
            (
                Protocol::OpenAiChat,
                "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"weather\",\"arguments\":\"{\\\"city\\\"\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"北京\\\"}\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                 data: [DONE]\n\n",
            ),
            (
                Protocol::AnthropicMessages,
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"1\",\"model\":\"m\"}}\n\n\
                 event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"weather\"}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\"\"}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"北京\\\"}\"}}\n\n\
                 event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ),
            (
                Protocol::OpenAiResponses,
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"1\",\"model\":\"m\"}}\n\n\
                 event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"weather\"}}\n\n\
                 event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"city\\\"\"}\n\n\
                 event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\":\\\"北京\\\"}\"}\n\n\
                 event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0}\n\n\
                 event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"1\",\"output\":[{\"type\":\"function_call\"}]}}\n\n",
            ),
        ];

        for (from, raw) in &streams {
            for to in ALL {
                let out = pipe(*from, to, raw);
                assert!(out.contains("call_1"), "{from:?} → {to:?} 流式丢了调用 ID");
                assert!(out.contains("weather"), "{from:?} → {to:?} 流式丢了工具名");
                assert!(out.contains("北京"), "{from:?} → {to:?} 流式丢了参数片段");
                // 停止原因必须映射成"工具轮次"，否则客户端不会执行工具。
                match to {
                    Protocol::OpenAiChat => assert!(out.contains("tool_calls")),
                    Protocol::AnthropicMessages => assert!(out.contains("tool_use")),
                    Protocol::OpenAiResponses => assert!(out.contains("function_call")),
                }
            }
        }
    }

    #[test]
    fn a_stream_error_uses_the_downstream_protocol_shape() {
        for protocol in ALL {
            let emitter = StreamEmitter::new(protocol, false, None);
            let bytes = emitter.error("上游中断");
            let text = String::from_utf8_lossy(&bytes);
            assert!(text.contains("上游中断"));
            match protocol {
                Protocol::AnthropicMessages | Protocol::OpenAiResponses => {
                    assert!(text.starts_with("event: error\n"))
                }
                Protocol::OpenAiChat => assert!(text.starts_with("data: ")),
            }
        }
    }

    #[test]
    fn a_truncated_upstream_stream_still_terminates_downstream() {
        // Chat 上游断流后必须补出 finish_reason 与 [DONE]，否则客户端会一直等。
        let out = pipe(
            Protocol::OpenAiChat,
            Protocol::AnthropicMessages,
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"半\"}}]}\n\n",
        );
        assert!(out.contains("event: message_stop"));
        assert!(out.contains("content_block_stop"));
    }

    #[test]
    fn the_stream_terminates_exactly_once() {
        // `[DONE]` 帧与"连接正常关闭"都会走收尾逻辑；下游只能看到一次收尾，
        // 否则客户端的状态机会在第二个 message_stop 上报错。
        for (from, raw) in [
            (
                Protocol::OpenAiChat,
                "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                 data: [DONE]\n\n",
            ),
            (
                Protocol::AnthropicMessages,
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"1\",\"model\":\"m\"}}\n\n\
                 event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n\
                 event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ),
        ] {
            let out = pipe(from, Protocol::AnthropicMessages, raw);
            assert_eq!(
                out.matches("event: message_stop").count(),
                1,
                "{from:?} 的流收尾发了不止一次\n{out}"
            );
            let out = pipe(from, Protocol::OpenAiChat, raw);
            assert_eq!(out.matches("[DONE]").count(), 1, "{from:?}\n{out}");
            assert_eq!(
                out.matches("\"finish_reason\":\"stop\"").count(),
                1,
                "{from:?}\n{out}"
            );
        }
    }

    #[test]
    fn an_upstream_that_omits_its_stop_reason_still_yields_one() {
        // 兼容站点常常直接发 message_stop 而不发 message_delta。下游若拿不到
        // finish_reason 会把响应当成截断，所以这里必须补一个（§14.5）。
        let out = pipe(
            Protocol::AnthropicMessages,
            Protocol::OpenAiChat,
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"1\",\"model\":\"m\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        assert!(out.contains("\"finish_reason\":\"stop\""), "{out}");
        assert!(out.trim_end().ends_with("data: [DONE]"), "{out}");

        // 上游自己给了停止原因时，绝不覆盖成通用值。
        let out = pipe(
            Protocol::AnthropicMessages,
            Protocol::OpenAiChat,
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"1\",\"model\":\"m\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"半\"}}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        assert!(out.contains("\"finish_reason\":\"length\""), "{out}");
    }
}
