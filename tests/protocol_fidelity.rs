//! 跨协议转换的保真度回归（§14.6、§14.8）。
//!
//! 核心承诺：文本、图片、文件、工具、结构化输出、流式事件与未知字段绝不静默
//! 丢失；只有思考可以降级，而且必须标记。这里的每个用例对应一次审计发现。

use akhub::domain::Protocol;
use akhub::protocol::canonical::{Event, ItemKind, StopReason};
use akhub::protocol::degrade::{Degradations, Fidelity};
use akhub::protocol::{
    self, anthropic, convert_request, convert_response, openai_chat, openai_responses, sse,
};
use serde_json::{Map, Value, json};

const CHAT: Protocol = Protocol::OpenAiChat;
const RESPONSES: Protocol = Protocol::OpenAiResponses;
const MESSAGES: Protocol = Protocol::AnthropicMessages;

fn degraded(fidelity: &Fidelity) -> Vec<String> {
    match fidelity {
        Fidelity::Lossless => Vec::new(),
        Fidelity::Degraded(list) => list.clone(),
    }
}

fn rejected(from: Protocol, to: Protocol, body: &Value) -> String {
    match convert_request(from, to, body) {
        Ok(converted) => panic!("期望拒绝，却转换成了 {}", converted.body),
        Err(reason) => reason.to_string(),
    }
}

// ------------------------------------------------ 1. output_config 的其他子字段

fn align(model: &str, body: Value) -> (Map<String, Value>, Vec<String>) {
    let mut object = body.as_object().cloned().unwrap();
    let mut degradations = Degradations::default();
    anthropic::align_thinking(&mut object, model, MESSAGES, None, None, &mut degradations);
    (object, degradations.into_list())
}

#[test]
fn effort_and_structured_output_format_survive_thinking_alignment() {
    let format = json!({"type": "json_schema", "schema": {"type": "object"}});
    let body = json!({
        "model": "m", "max_tokens": 1024, "messages": [],
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "high", "format": format},
    });

    // 自适应契约：effort 原样，format 不能被顺手删掉。
    let (aligned, _) = align("claude-opus-5-5", body.clone());
    assert_eq!(aligned["output_config"]["effort"], "high");
    assert_eq!(aligned["output_config"]["format"], format);

    // 手工契约：effort 换成预算，format 依然保留。
    let (aligned, _) = align("claude-sonnet-4-5", body);
    assert_eq!(aligned["thinking"]["type"], "enabled");
    assert!(aligned["output_config"].get("effort").is_none());
    assert_eq!(aligned["output_config"]["format"], format);
}

#[test]
fn output_config_without_other_subfields_is_removed_when_effort_goes_away() {
    let body = json!({
        "model": "m", "max_tokens": 1024, "messages": [],
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "high"},
    });
    let (aligned, _) = align("claude-sonnet-4-5", body);
    assert!(aligned.get("output_config").is_none());
}

// ------------------------------------------------ 2. Chat 流式解析的内容项划分

fn chat_events(chunks: &[Value]) -> Vec<Event> {
    let mut parser = openai_chat::StreamParser::new();
    let mut events: Vec<Event> = chunks.iter().flat_map(|c| parser.push(c)).collect();
    events.extend(parser.finish());
    events
}

fn chunk(delta: Value) -> Value {
    json!({"id": "c1", "model": "m", "choices": [{"index": 0, "delta": delta}]})
}

fn finish(reason: &str) -> Value {
    json!({"id": "c1", "model": "m", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn anthropic_frames(events: &[Event]) -> Vec<Value> {
    let mut emitter = anthropic::StreamEmitter::new();
    events
        .iter()
        .flat_map(|e| emitter.push(e))
        .map(|(_, data)| data)
        .collect()
}

fn responses_frames(events: &[Event]) -> Vec<Value> {
    let mut emitter = openai_responses::StreamEmitter::new(Some("resp_akh_test".into()));
    events
        .iter()
        .flat_map(|e| emitter.push(e))
        .map(|(_, data)| data)
        .collect()
}

/// 每个内容块的 (index, 种类, 收到的 delta 种类)。
fn anthropic_blocks(frames: &[Value]) -> Vec<(u64, String, Vec<String>)> {
    let mut blocks: Vec<(u64, String, Vec<String>)> = Vec::new();
    for frame in frames {
        match frame["type"].as_str() {
            Some("content_block_start") => blocks.push((
                frame["index"].as_u64().unwrap(),
                frame["content_block"]["type"].as_str().unwrap().into(),
                Vec::new(),
            )),
            Some("content_block_delta") => {
                let index = frame["index"].as_u64().unwrap();
                let block = blocks
                    .iter_mut()
                    .find(|(i, _, _)| *i == index)
                    .expect("delta 必须落在已经开始的块上");
                block
                    .2
                    .push(frame["delta"]["type"].as_str().unwrap().into());
            }
            _ => {}
        }
    }
    blocks
}

#[test]
fn chat_reasoning_then_content_becomes_two_separate_items() {
    let events = chat_events(&[
        chunk(json!({"role": "assistant", "reasoning_content": "想一想"})),
        chunk(json!({"reasoning_content": "再想想"})),
        chunk(json!({"content": "答案"})),
        chunk(json!({"content": "完毕"})),
        finish("stop"),
    ]);
    let starts: Vec<(usize, ItemKind)> = events
        .iter()
        .filter_map(|e| match e {
            Event::ItemStart { index, kind } => Some((*index, kind.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![(0, ItemKind::Thinking), (1, ItemKind::Text)]);
    // 思考项必须在正文项开始之前关闭，且每项只关一次。
    let ends: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            Event::ItemEnd { index } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![0, 1]);

    let blocks = anthropic_blocks(&anthropic_frames(&events));
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].1, "thinking");
    assert!(blocks[0].2.iter().all(|d| d == "thinking_delta"));
    assert_eq!(blocks[1].1, "text");
    assert!(blocks[1].2.iter().all(|d| d == "text_delta"));

    let frames = responses_frames(&events);
    let added: Vec<&str> = frames
        .iter()
        .filter(|f| f["type"] == "response.output_item.added")
        .map(|f| f["item"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(added, vec!["reasoning", "message"]);
    let text_deltas: Vec<u64> = frames
        .iter()
        .filter(|f| f["type"] == "response.output_text.delta")
        .map(|f| f["output_index"].as_u64().unwrap())
        .collect();
    assert_eq!(text_deltas, vec![1, 1], "正文只能落在 message 项上");
    let completed = frames.last().unwrap();
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(
        completed["response"]["output"][1]["content"][0]["text"],
        "答案完毕"
    );
}

#[test]
fn chat_content_tool_content_gets_its_own_items() {
    let events = chat_events(&[
        chunk(json!({"role": "assistant", "content": "先查"})),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
            "function": {"name": "weather", "arguments": "{\"city\""}}]}),
        ),
        chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": ":\"北京\"}"}}]})),
        chunk(json!({"content": "查完了"})),
        finish("tool_calls"),
    ]);

    let blocks = anthropic_blocks(&anthropic_frames(&events));
    let kinds: Vec<&str> = blocks.iter().map(|b| b.1.as_str()).collect();
    assert_eq!(kinds, vec!["text", "tool_use", "text"]);
    assert!(blocks[1].2.iter().all(|d| d == "input_json_delta"));
    assert_eq!(blocks[2].2, vec!["text_delta".to_string()]);

    let frames = responses_frames(&events);
    let completed = frames.last().unwrap();
    let output = completed["response"]["output"].as_array().unwrap();
    let types: Vec<&str> = output.iter().map(|i| i["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["message", "function_call", "message"]);
    assert_eq!(output[1]["arguments"], "{\"city\":\"北京\"}");
    assert_eq!(output[2]["content"][0]["text"], "查完了");
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Finish {
            stop: StopReason::ToolUse,
            ..
        }
    )));
}

// ------------------------------------------------ 3. 常见的 OpenAI 提示字段

#[test]
fn codex_hint_fields_do_not_block_cross_protocol_targets() {
    let body = json!({
        "model": "gpt-5", "input": "hi",
        "prompt_cache_key": "session-1",
        "safety_identifier": "user-1",
        "stream_options": {"include_obfuscation": false},
        "max_tool_calls": 4,
        "text": {"verbosity": "low"},
    });

    let chat = convert_request(RESPONSES, CHAT, &body).unwrap();
    assert_eq!(chat.body["prompt_cache_key"], "session-1");
    assert_eq!(chat.body["user"], "user-1");
    assert_eq!(chat.body["verbosity"], "low");
    assert_eq!(degraded(&chat.fidelity), vec!["max_tool_calls".to_string()]);

    let messages = convert_request(RESPONSES, MESSAGES, &body).unwrap();
    assert!(messages.body.get("prompt_cache_key").is_none());
    assert_eq!(messages.body["metadata"]["user_id"], "user-1");
    let mut lost = degraded(&messages.fidelity);
    lost.sort();
    assert_eq!(lost, vec!["max_tool_calls".to_string(), "verbosity".into()]);

    let chat_body = json!({
        "model": "m", "messages": [{"role": "user", "content": "hi"}],
        "prompt_cache_key": "k", "safety_identifier": "s", "verbosity": "high",
    });
    let responses = convert_request(CHAT, RESPONSES, &chat_body).unwrap();
    assert_eq!(responses.body["prompt_cache_key"], "k");
    assert_eq!(responses.body["text"]["verbosity"], "high");
    assert_eq!(responses.fidelity, Fidelity::Lossless);
}

// ------------------------------------------------ 4. 表达不了就拒绝

#[test]
fn anthropic_blocks_without_a_canonical_shape_are_rejected() {
    for block in [
        json!({"type": "image", "source": {"type": "file", "file_id": "file_1"}}),
        json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "x"}}),
        json!({"type": "search_result", "source": "s", "title": "t", "content": []}),
    ] {
        let body = json!({
            "model": "m", "max_tokens": 16,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "看"}, block]}],
        });
        rejected(MESSAGES, CHAT, &body);
        rejected(MESSAGES, RESPONSES, &body);
    }
}

#[test]
fn chat_parts_without_a_canonical_shape_are_rejected() {
    for part in [
        json!({"type": "file", "file": {"file_id": "file_1"}}),
        json!({"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}}),
    ] {
        let body = json!({"model": "m", "messages": [{"role": "user", "content": [part]}]});
        rejected(CHAT, MESSAGES, &body);
        rejected(CHAT, RESPONSES, &body);
    }
}

#[test]
fn responses_items_and_parts_without_a_canonical_shape_are_rejected() {
    let inputs = [
        json!([{"type": "message", "role": "user", "content": [
            {"type": "input_image", "file_id": "file_1"}]}]),
        json!([{"type": "message", "role": "user", "content": [
            {"type": "input_file", "file_id": "file_1"}]}]),
        json!([{"type": "item_reference", "id": "msg_1"}]),
        json!([{"type": "compaction", "encrypted_content": "x"}]),
        json!([{"type": "custom_tool_call", "call_id": "c", "name": "n", "input": "x"}]),
        json!([{"type": "local_shell_call", "call_id": "c", "action": {}}]),
        json!([{"type": "web_search_call", "id": "ws_1", "status": "completed"}]),
    ];
    for input in inputs {
        let body = json!({"model": "m", "input": input});
        rejected(RESPONSES, MESSAGES, &body);
        rejected(RESPONSES, CHAT, &body);
    }
}

#[test]
fn responses_file_url_maps_to_an_anthropic_url_document_but_not_to_chat() {
    let body = json!({"model": "m", "input": [{"type": "message", "role": "user", "content": [
        {"type": "input_file", "file_url": "https://example.com/a.pdf"}]}]});
    let messages = convert_request(RESPONSES, MESSAGES, &body).unwrap();
    assert_eq!(
        messages.body["messages"][0]["content"][0]["source"],
        json!({"type": "url", "url": "https://example.com/a.pdf"})
    );
    rejected(RESPONSES, CHAT, &body);
}

#[test]
fn tool_result_images_move_to_a_user_message_after_the_tool_messages() {
    // Claude Code 把截图放在 tool_result 里。
    let body = json!({
        "model": "m", "max_tokens": 16,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "a", "name": "shot", "input": {}},
                {"type": "tool_use", "id": "b", "name": "shot", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": [
                    {"type": "text", "text": "截图如下"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]},
                {"type": "tool_result", "tool_use_id": "b", "content": "无图"},
                {"type": "text", "text": "继续"}
            ]}
        ]
    });
    let chat = convert_request(MESSAGES, CHAT, &body).unwrap();
    let messages = chat.body["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["assistant", "tool", "tool", "user", "user"]);
    assert_eq!(messages[1]["content"], "截图如下");
    assert_eq!(
        messages[3]["content"][0]["image_url"]["url"],
        "data:image/png;base64,AAAA"
    );
    assert_eq!(messages[4]["content"], "继续");

    // Responses 的 function_call_output 本身就能放图片。
    let responses = convert_request(MESSAGES, RESPONSES, &body).unwrap();
    let output = &responses.body["input"][2]["output"];
    assert_eq!(output[1]["type"], "input_image");
}

// ------------------------------------------------ 5. 不透明思考不跨供应商

#[test]
fn anthropic_redacted_thinking_never_becomes_responses_encrypted_content() {
    let body = json!({
        "model": "m", "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": "anthropic-secret"},
                {"type": "thinking", "thinking": "推理", "signature": "sig"},
                {"type": "text", "text": "答"}
            ]},
            {"role": "user", "content": "再说"}
        ]
    });
    let converted = convert_request(MESSAGES, RESPONSES, &body).unwrap();
    let text = converted.body.to_string();
    assert!(!text.contains("anthropic-secret"));
    assert!(!text.contains("encrypted_content"));
    assert_eq!(degraded(&converted.fidelity), vec!["thinking".to_string()]);
}

#[test]
fn responses_encrypted_reasoning_never_reaches_anthropic() {
    let body = json!({"model": "m", "input": [
        {"type": "message", "role": "user", "content": "hi"},
        {"type": "reasoning", "id": "rs_1", "encrypted_content": "openai-secret",
         "summary": [{"type": "summary_text", "text": "想过了"}]},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "答"}]},
    ]});
    let converted = convert_request(RESPONSES, MESSAGES, &body).unwrap();
    let text = converted.body.to_string();
    assert!(!text.contains("openai-secret"));
    assert!(!text.contains("\"thinking\":\"想过了\""));
    assert_eq!(degraded(&converted.fidelity), vec!["thinking".to_string()]);
}

#[test]
fn round_tripped_unsigned_thinking_is_dropped_not_forwarded() {
    // 跨协议发给 Anthropic 客户端的思考没有签名；客户端原样带回来时，它不能
    // 被当成签名思考转出去。
    let body = json!({
        "model": "m", "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "推理", "signature": ""},
                {"type": "thinking", "thinking": "推理2"},
                {"type": "text", "text": "答"}
            ]},
            {"role": "user", "content": "再说"}
        ]
    });
    let request = protocol::parse_request(MESSAGES, &body).unwrap();
    let emitted = protocol::emit_request(MESSAGES, &request).unwrap();
    let assistant = &emitted.body["messages"][1]["content"];
    assert_eq!(assistant.as_array().unwrap().len(), 1);
    assert_eq!(assistant[0]["type"], "text");
    assert_eq!(emitted.degraded, vec!["thinking".to_string()]);
}

#[test]
fn opaque_thinking_in_upstream_streams_is_recorded_as_degraded() {
    let mut parser = protocol::StreamParser::new(MESSAGES);
    parser.push(&sse::Frame {
        event: Some("content_block_start".into()),
        data: json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "redacted_thinking", "data": "x"}})
        .to_string(),
    });
    assert_eq!(parser.take_degraded(), vec!["thinking"]);

    let mut parser = protocol::StreamParser::new(RESPONSES);
    parser.push(&sse::Frame {
        event: Some("response.output_item.done".into()),
        data: json!({"type": "response.output_item.done", "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "x", "summary": []}})
        .to_string(),
    });
    assert_eq!(parser.take_degraded(), vec!["thinking"]);
}

// ------------------------------------------------ 6. 档位换算

#[test]
fn effort_levels_map_without_silent_folding() {
    let anthropic = |effort: &str| {
        json!({
            "model": "claude-opus-5-5", "max_tokens": 16, "messages": [],
            "thinking": {"type": "adaptive"}, "output_config": {"effort": effort},
        })
    };
    let xhigh = convert_request(MESSAGES, RESPONSES, &anthropic("xhigh")).unwrap();
    assert_eq!(xhigh.body["reasoning"]["effort"], "xhigh");
    assert!(!degraded(&xhigh.fidelity).contains(&"effort".to_string()));

    let max = convert_request(MESSAGES, CHAT, &anthropic("max")).unwrap();
    assert_eq!(max.body["reasoning_effort"], "xhigh");
    assert!(degraded(&max.fidelity).contains(&"effort".to_string()));

    let minimal = json!({"model": "claude-opus-5-5", "reasoning_effort": "minimal",
        "messages": [{"role": "user", "content": "hi"}]});
    let converted = convert_request(CHAT, MESSAGES, &minimal).unwrap();
    assert_eq!(converted.body["output_config"]["effort"], "low");
}

// ------------------------------------------------ 7. 零散问题

#[test]
fn context_window_exhaustion_is_reported_as_truncation() {
    let body = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": "半截"}],
        "stop_reason": "model_context_window_exceeded",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    });
    let chat = convert_response(MESSAGES, CHAT, &body).unwrap();
    assert_eq!(chat["choices"][0]["finish_reason"], "length");
    let responses = convert_response(MESSAGES, RESPONSES, &body).unwrap();
    assert_eq!(responses["status"], "incomplete");
}

#[test]
fn trickled_frames_split_correctly_across_every_byte_boundary() {
    let payload = "x".repeat(10_000);
    let stream = format!("event: a\ndata: {payload}\n\nevent: b\r\ndata: 2\r\n\r\n");
    let mut reader = sse::FrameReader::new();
    let mut frames = Vec::new();
    for byte in stream.as_bytes() {
        frames.extend(reader.push(std::slice::from_ref(byte)));
    }
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].data, payload);
    assert_eq!(frames[1].event.as_deref(), Some("b"));
    assert_eq!(frames[1].data, "2");
}
