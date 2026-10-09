//! Anthropic Messages 的解析与发射（§14.6）。
//!
//! Anthropic 的形状与另外两个协议差别最大的三处：
//! 1. `system` 是顶层字段，不是消息数组里的一条。
//! 2. 工具结果是**用户轮次里的一个内容块**，不是独立的 `tool` 角色消息。
//! 3. 工具参数是解析好的 JSON 对象，不是字符串。
//!
//! 三处都必须在这里吸收掉，中间格式才不会被某一个协议的形状污染。

use serde_json::{Map, Value, json};

use crate::domain::Protocol;
use crate::protocol::canonical::{
    Effort, Event, ItemKind, MediaSource, Message, OutputFormat, Part, Request, Response, Role,
    StopReason, ThinkingBlock, ThinkingConfig, ThinkingDisplay, Tool, ToolChoice, Usage,
};
use crate::protocol::degrade::{Degradations, Emitted, Unsupported};
use crate::protocol::{known, sampling_key, string_list};

/// Messages 请求体里所有被显式处理的顶层字段。
const KNOWN_FIELDS: &[&str] = &[
    "model",
    "messages",
    "system",
    "stream",
    "tools",
    "tool_choice",
    "max_tokens",
    "temperature",
    "top_p",
    "stop_sequences",
    "thinking",
    "output_config",
    "metadata",
    "service_tier",
];

/// 可以安全丢弃的 Anthropic 专有采样参数（§14.8 白名单）。
const SAMPLING_FIELDS: &[&str] = &["top_k"];

/// 无法在其他协议表达的 Anthropic 专有能力。
const VENDOR_TOOL_TYPES: &[&str] = &[
    "computer",
    "bash",
    "text_editor",
    "web_search",
    "code_execution",
    "mcp",
];

// ---------------------------------------------------------------- 解析

/// 把 Anthropic Messages 请求体解析成中间格式。
pub fn parse_request(body: &Value) -> Result<Request, Unsupported> {
    let object = body
        .as_object()
        .ok_or_else(|| Unsupported::new("请求体不是 JSON 对象"))?;

    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| Unsupported::new("请求体缺少 model"))?;
    let mut request = Request::new(Protocol::AnthropicMessages, model);

    request.stream = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    request.include_usage = true;
    request.max_tokens = object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    request.temperature = object.get("temperature").and_then(Value::as_f64);
    request.top_p = object.get("top_p").and_then(Value::as_f64);
    request.stop = string_list(object.get("stop_sequences"));
    request.user = object
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        .map(str::to_string);

    if let Some(system) = object.get("system") {
        let parts = parse_content(system);
        if !parts.is_empty() {
            request.messages.push(Message {
                role: Role::System,
                parts,
            });
        }
    }

    for message in object
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = match message.get("role").and_then(Value::as_str) {
            Some("assistant") => Role::Assistant,
            _ => Role::User,
        };
        let parts = parse_content(message.get("content").unwrap_or(&Value::Null));
        request.messages.push(Message { role, parts });
    }

    for tool in object
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match parse_tool(tool) {
            Ok(tool) => request.tools.push(tool),
            Err(vendor) => request.inexpressible.push(vendor),
        }
    }
    request.tool_choice = object.get("tool_choice").and_then(parse_tool_choice);
    // Anthropic 用「禁用并行」表达，中间格式统一成「是否允许并行」。
    if let Some(disabled) = object
        .get("tool_choice")
        .and_then(|c| c.get("disable_parallel_tool_use"))
        .and_then(Value::as_bool)
    {
        request.parallel_tool_calls = Some(!disabled);
    }

    if let Some(thinking) = object.get("thinking") {
        // `enabled` 与 `adaptive` 都是"要思考"，只有 `disabled` 是显式关闭。
        let enabled = thinking.get("type").and_then(Value::as_str) != Some("disabled");
        request.thinking = Some(ThinkingConfig {
            enabled,
            budget_tokens: thinking
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
            effort: None,
            display: thinking
                .get("display")
                .and_then(Value::as_str)
                .and_then(ThinkingDisplay::parse),
        });
    }

    // 4.6 起档位搬到顶层 `output_config.effort`，`thinking` 只剩开关与展示方式。
    if let Some(config) = object.get("output_config") {
        if let Some(effort) = config
            .get("effort")
            .and_then(Value::as_str)
            .and_then(Effort::parse)
        {
            request
                .thinking
                .get_or_insert(ThinkingConfig {
                    enabled: true,
                    budget_tokens: None,
                    effort: None,
                    display: None,
                })
                .effort = Some(effort);
        }
        // `output_config` 的其他子字段（结构化输出的 `format`……）这一层还表达
        // 不了：按"不能表达就报错"处理，而不是静默丢给上游。
        for field in config
            .as_object()
            .into_iter()
            .flat_map(|object| object.keys())
        {
            if field.as_str() != "effort" {
                request.unknown.push(format!("output_config.{field}"));
            }
        }
    }

    for field in SAMPLING_FIELDS {
        if let Some(value) = object.get(*field) {
            request.sampling.insert((*field).to_string(), value.clone());
        }
    }
    for (field, _) in object {
        if !known(KNOWN_FIELDS, field) && !known(SAMPLING_FIELDS, field) {
            request.unknown.push(field.clone());
        }
    }
    Ok(request)
}

/// 内容既可能是纯字符串，也可能是内容块数组。
fn parse_content(content: &Value) -> Vec<Part> {
    match content {
        Value::String(text) if !text.is_empty() => vec![Part::text(text.clone())],
        Value::Array(blocks) => blocks.iter().filter_map(parse_block).collect(),
        _ => Vec::new(),
    }
}

fn parse_block(block: &Value) -> Option<Part> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(Part::text(
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )),
        "image" => Some(Part::Image {
            source: parse_source(block.get("source")?)?,
            detail: None,
        }),
        "document" => Some(Part::Document {
            source: parse_source(block.get("source")?)?,
            name: block
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "tool_use" => Some(Part::ToolCall {
            id: block.get("id").and_then(Value::as_str)?.to_string(),
            name: block.get("name").and_then(Value::as_str)?.to_string(),
            // 中间格式统一保存 JSON 文本：另外两个协议的原生形状就是文本。
            arguments: block
                .get("input")
                .map(ToString::to_string)
                .unwrap_or_else(|| "{}".into()),
        }),
        "tool_result" => Some(Part::ToolResult {
            call_id: block
                .get("tool_use_id")
                .and_then(Value::as_str)?
                .to_string(),
            content: parse_content(block.get("content").unwrap_or(&Value::Null)),
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "thinking" => Some(Part::Thinking(ThinkingBlock {
            text: block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            signature: block
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..ThinkingBlock::default()
        })),
        "redacted_thinking" => Some(Part::Thinking(ThinkingBlock {
            encrypted: block
                .get("data")
                .and_then(Value::as_str)
                .map(str::to_string),
            redacted: true,
            ..ThinkingBlock::default()
        })),
        _ => None,
    }
}

fn parse_source(source: &Value) -> Option<MediaSource> {
    match source.get("type").and_then(Value::as_str)? {
        "base64" => Some(MediaSource::Base64 {
            media_type: source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_string(),
            data: source.get("data").and_then(Value::as_str)?.to_string(),
        }),
        "url" => Some(MediaSource::Url(
            source.get("url").and_then(Value::as_str)?.to_string(),
        )),
        _ => None,
    }
}

/// 标准函数工具解析成 [`Tool`]；供应商内置工具返回它的类型名。
fn parse_tool(tool: &Value) -> Result<Tool, String> {
    if let Some(kind) = tool.get("type").and_then(Value::as_str)
        && VENDOR_TOOL_TYPES.iter().any(|v| kind.starts_with(v))
    {
        return Err(format!("Anthropic 内置工具 {kind}"));
    }
    let Some(name) = tool.get("name").and_then(Value::as_str) else {
        return Err("缺少名称的工具定义".into());
    };
    Ok(Tool {
        name: name.to_string(),
        description: tool
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        parameters: tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
        strict: None,
    })
}

fn parse_tool_choice(choice: &Value) -> Option<ToolChoice> {
    match choice.get("type").and_then(Value::as_str)? {
        "auto" => Some(ToolChoice::Auto),
        "any" => Some(ToolChoice::Required),
        "none" => Some(ToolChoice::None),
        "tool" => Some(ToolChoice::Named(
            choice.get("name").and_then(Value::as_str)?.to_string(),
        )),
        _ => None,
    }
}

/// 解析非流式响应体。
pub fn parse_response(body: &Value) -> Result<Response, Unsupported> {
    let parts = parse_content(body.get("content").unwrap_or(&Value::Null));
    Ok(Response {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        stop: body
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(parse_stop)
            .unwrap_or(StopReason::EndTurn),
        stop_sequence: body
            .get("stop_sequence")
            .and_then(Value::as_str)
            .map(str::to_string),
        usage: parse_usage(body.get("usage")),
        parts,
    })
}

fn parse_stop(raw: &str) -> StopReason {
    match raw {
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" | "pause_turn" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

/// Anthropic 的 `input_tokens` **不含**缓存部分，中间格式的 `input` 含，
/// 所以这里要加回去（§14.6 "不能伪造数字"）。
fn parse_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    let cache_read = field("cache_read_input_tokens");
    let cache_write = field("cache_creation_input_tokens");
    let input =
        field("input_tokens").map(|base| base + cache_read.unwrap_or(0) + cache_write.unwrap_or(0));
    Usage {
        input,
        output: field("output_tokens"),
        cache_read,
        cache_write,
        reasoning: None,
    }
}

// ---------------------------------------------------------------- 发射

/// 把中间格式发射成 Anthropic Messages 请求体。
pub fn emit_request(request: &Request) -> Result<Emitted, Unsupported> {
    request.reject_inexpressible()?;
    let mut degraded = Degradations::default();

    // 结构化输出没有等价能力：Anthropic 只有工具，用工具伪装 Schema 会改变
    // stop_reason 与响应形状，属于"不是语义等价"（§14.6）。
    if request.output_format.is_some() {
        return Err(Unsupported::new(
            "Anthropic Messages 无法表达结构化输出 Schema",
        ));
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    // Anthropic 要求 max_tokens 必填。缺省时给一个足够大的值，而不是猜一个小的。
    body.insert(
        "max_tokens".into(),
        json!(request.max_tokens.unwrap_or(32_000)),
    );
    if request.stream {
        body.insert("stream".into(), json!(true));
    }

    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        match message.role {
            // developer 与 system 在 Anthropic 只有一个落点，合并进 system。
            Role::System | Role::Developer => system.extend(emit_blocks(
                &message.parts,
                &mut degraded,
                /* keep_thinking */ false,
            )?),
            Role::User | Role::Assistant => {
                let keep_thinking = message.role == Role::Assistant;
                let blocks = emit_blocks(&message.parts, &mut degraded, keep_thinking)?;
                if blocks.is_empty() {
                    continue;
                }
                let role = if message.role == Role::Assistant {
                    "assistant"
                } else {
                    "user"
                };
                push_message(&mut messages, role, blocks);
            }
        }
    }
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    body.insert("messages".into(), Value::Array(messages));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = &request.tool_choice {
        let mut value = match choice {
            ToolChoice::Auto => json!({"type": "auto"}),
            ToolChoice::None => json!({"type": "none"}),
            ToolChoice::Required => json!({"type": "any"}),
            ToolChoice::Named(name) => json!({"type": "tool", "name": name}),
        };
        if request.parallel_tool_calls == Some(false)
            && let Some(object) = value.as_object_mut()
        {
            object.insert("disable_parallel_tool_use".into(), json!(true));
        }
        body.insert("tool_choice".into(), value);
    }

    let contract = ThinkingContract::of(&request.model);
    if let Some(thinking) = request.thinking {
        if thinking.enabled {
            body.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": thinking.budget()}),
            );
            // 思考预算必须小于 max_tokens，否则 Anthropic 直接 400。自适应契约
            // 不发预算，也就没有这条约束，不该为它把 max_tokens 抬上去。
            let max = body.get("max_tokens").and_then(Value::as_u64).unwrap_or(0) as u32;
            if !contract.adaptive && max <= thinking.budget() {
                body.insert("max_tokens".into(), json!(thinking.budget() + 4_096));
            }
        } else {
            body.insert("thinking".into(), json!({"type": "disabled"}));
        }
    }

    insert_common(&mut body, request, &mut degraded);
    // 上面发的是手工形状；认不认它由**上游模型**决定，不由下游协议决定。
    align_thinking(
        &mut body,
        &request.model,
        request.origin,
        request.thinking.as_ref(),
        &mut degraded,
    );
    Ok(Emitted {
        body: Value::Object(body),
        degraded: degraded.into_list(),
        structured_tool: None,
    })
}

/// 相邻的同角色消息必须合并：Anthropic 拒绝连续两条 user 或 assistant。
fn push_message(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.extend(blocks);
        return;
    }
    messages.push(json!({"role": role, "content": blocks}));
}

fn insert_common(body: &mut Map<String, Value>, request: &Request, degraded: &mut Degradations) {
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    if !request.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(request.stop));
    }
    if let Some(user) = &request.user {
        body.insert("metadata".into(), json!({"user_id": user}));
    }
    for (field, value) in &request.sampling {
        match sampling_key(field, Protocol::AnthropicMessages) {
            Some(target) => {
                body.insert(target.into(), value.clone());
            }
            None => degraded.drop(field),
        }
    }
}

fn emit_blocks(
    parts: &[Part],
    degraded: &mut Degradations,
    keep_thinking: bool,
) -> Result<Vec<Value>, Unsupported> {
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            Part::Text(text) if text.is_empty() => {}
            Part::Text(text) => blocks.push(json!({"type": "text", "text": text})),
            Part::Refusal(text) => blocks.push(json!({"type": "text", "text": text})),
            Part::Image { source, .. } => {
                blocks.push(json!({"type": "image", "source": emit_source(source)}))
            }
            Part::Document { source, name } => {
                let mut block = json!({"type": "document", "source": emit_source(source)});
                if let Some(name) = name
                    && let Some(object) = block.as_object_mut()
                {
                    object.insert("title".into(), json!(name));
                }
                blocks.push(block);
            }
            Part::ToolCall {
                id,
                name,
                arguments,
            } => blocks.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                // Anthropic 要的是对象；参数不是合法 JSON 时不能猜，直接报错。
                "input": parse_arguments(arguments)?,
            })),
            Part::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": emit_blocks(content, degraded, false)?,
                });
                if *is_error && let Some(object) = block.as_object_mut() {
                    object.insert("is_error".into(), json!(true));
                }
                blocks.push(block);
            }
            // 没有签名的思考块 Anthropic 会拒收；来自其他协议的思考只能丢弃。
            Part::Thinking(thinking) => match (keep_thinking, &thinking.signature) {
                (true, Some(signature)) => blocks.push(json!({
                    "type": "thinking",
                    "thinking": thinking.text,
                    "signature": signature,
                })),
                (true, None) if thinking.redacted && thinking.encrypted.is_some() => {
                    blocks.push(json!({
                        "type": "redacted_thinking",
                        "data": thinking.encrypted,
                    }))
                }
                _ => degraded.drop("thinking"),
            },
        }
    }
    Ok(blocks)
}

fn parse_arguments(arguments: &str) -> Result<Value, Unsupported> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(arguments)
        .map_err(|_| Unsupported::new("工具调用参数不是合法 JSON，无法转换为 Anthropic 形状"))
}

fn emit_source(source: &MediaSource) -> Value {
    match source {
        MediaSource::Base64 { media_type, data } => json!({
            "type": "base64",
            "media_type": media_type,
            "data": data,
        }),
        MediaSource::Url(url) => json!({"type": "url", "url": url}),
    }
}

/// 把中间格式的响应发射成 Anthropic 响应体。
pub fn emit_response(response: &Response) -> Result<Value, Unsupported> {
    let mut degraded = Degradations::default();
    let content = emit_blocks(&response.parts, &mut degraded, true)?;
    Ok(json!({
        "id": response.id,
        "type": "message",
        "role": "assistant",
        "model": response.model,
        "content": content,
        "stop_reason": emit_stop(response.stop),
        "stop_sequence": response.stop_sequence,
        "usage": emit_usage(&response.usage),
    }))
}

fn emit_stop(stop: StopReason) -> &'static str {
    match stop {
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::Refusal => "refusal",
        // 内容过滤在 Anthropic 侧没有对应取值；end_turn 是最接近且稳定的选择。
        StopReason::EndTurn | StopReason::ContentFilter => "end_turn",
    }
}

fn emit_usage(usage: &Usage) -> Value {
    let mut object = Map::new();
    // 缺失的项不写 0：伪造数字比留空更糟（§14.6）。
    if let Some(input) = usage.uncached_input() {
        object.insert("input_tokens".into(), json!(input));
    }
    if let Some(output) = usage.output {
        object.insert("output_tokens".into(), json!(output));
    }
    if let Some(read) = usage.cache_read {
        object.insert("cache_read_input_tokens".into(), json!(read));
    }
    if let Some(write) = usage.cache_write {
        object.insert("cache_creation_input_tokens".into(), json!(write));
    }
    Value::Object(object)
}

// ------------------------------------------------------------ 流式解析

/// 把一帧 Anthropic SSE 解析成中间事件。识别不了的帧返回空列表。
pub fn parse_event(event: Option<&str>, data: &Value) -> Vec<Event> {
    let kind = event
        .or_else(|| data.get("type").and_then(Value::as_str))
        .unwrap_or_default();
    let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;

    match kind {
        "message_start" => {
            let message = data.get("message");
            let mut events = vec![Event::Start {
                id: field(message, "id"),
                model: field(message, "model"),
            }];
            let mut usage = parse_usage(message.and_then(|m| m.get("usage")));
            // 开场的输出占位不能经跨协议转换变成最终的真实用量。
            usage.output = None;
            usage.reasoning = None;
            if !usage.is_empty() {
                events.push(Event::Usage(usage));
            }
            events
        }
        "content_block_start" => {
            let block = data.get("content_block");
            let kind = match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                Some("tool_use") => ItemKind::ToolCall {
                    id: field(block, "id"),
                    name: field(block, "name"),
                },
                Some("thinking") | Some("redacted_thinking") => ItemKind::Thinking,
                _ => ItemKind::Text,
            };
            vec![Event::ItemStart { index, kind }]
        }
        "content_block_delta" => {
            let delta = data.get("delta");
            let kind = delta.and_then(|d| d.get("type")).and_then(Value::as_str);
            match kind {
                Some("input_json_delta") => vec![Event::ToolArgsDelta {
                    index,
                    fragment: field(delta, "partial_json"),
                }],
                Some("thinking_delta") => vec![Event::ThinkingDelta {
                    index,
                    text: field(delta, "thinking"),
                }],
                // signature_delta 只对 Anthropic 自己有意义，跨协议无处安放。
                Some("signature_delta") => Vec::new(),
                _ => vec![Event::TextDelta {
                    index,
                    text: field(delta, "text"),
                }],
            }
        }
        "content_block_stop" => vec![Event::ItemEnd { index }],
        "message_delta" => {
            let mut events = Vec::new();
            let usage = parse_usage(data.get("usage"));
            if !usage.is_empty() {
                events.push(Event::Usage(usage));
            }
            let delta = data.get("delta");
            if let Some(stop) = delta
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
            {
                events.push(Event::Finish {
                    stop: parse_stop(stop),
                    stop_sequence: delta
                        .and_then(|d| d.get("stop_sequence"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            events
        }
        "message_stop" => vec![Event::Done],
        _ => Vec::new(),
    }
}

fn field(value: Option<&Value>, name: &str) -> String {
    value
        .and_then(|v| v.get(name))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

// ------------------------------------------------------------ 流式发射

/// 把中间事件发射成 Anthropic SSE 帧。
///
/// Anthropic 的流有严格的嵌套结构：`message_start` → 若干
/// `content_block_start/delta/stop` → `message_delta` → `message_stop`。
/// 这个状态机负责补齐中间格式里没有、但 Anthropic 必需的帧。
#[derive(Debug, Default)]
pub struct StreamEmitter {
    started: bool,
    open_block: Option<usize>,
    model: String,
    usage: Usage,
}

impl StreamEmitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把一个中间事件翻译成零到多帧 `(event, data)`。
    pub fn push(&mut self, event: &Event) -> Vec<(String, Value)> {
        match event {
            Event::Start { id, model } => {
                self.started = true;
                self.model = model.clone();
                vec![(
                    "message_start".into(),
                    json!({
                        "type": "message_start",
                        "message": {
                            "id": id,
                            "type": "message",
                            "role": "assistant",
                            "model": model,
                            "content": [],
                            "stop_reason": Value::Null,
                            "usage": {"input_tokens": 0, "output_tokens": 0},
                        }
                    }),
                )]
            }
            Event::ItemStart { index, kind } => {
                let mut frames = self.close_open_block();
                self.open_block = Some(*index);
                let block = match kind {
                    ItemKind::Text | ItemKind::Refusal => json!({"type": "text", "text": ""}),
                    ItemKind::Thinking => json!({"type": "thinking", "thinking": ""}),
                    ItemKind::ToolCall { id, name } => json!({
                        "type": "tool_use", "id": id, "name": name, "input": {}
                    }),
                };
                frames.push((
                    "content_block_start".into(),
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": block,
                    }),
                ));
                frames
            }
            Event::TextDelta { index, text } => vec![(
                "content_block_delta".into(),
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "text_delta", "text": text},
                }),
            )],
            Event::ThinkingDelta { index, text } => vec![(
                "content_block_delta".into(),
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "thinking_delta", "thinking": text},
                }),
            )],
            Event::ToolArgsDelta { index, fragment } => vec![(
                "content_block_delta".into(),
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": fragment},
                }),
            )],
            Event::ItemEnd { index } => {
                self.open_block = None;
                vec![(
                    "content_block_stop".into(),
                    json!({"type": "content_block_stop", "index": index}),
                )]
            }
            // usage 先记下来，等 Finish 时随 message_delta 一起发。
            Event::Usage(usage) => {
                self.usage.merge(*usage);
                Vec::new()
            }
            Event::Finish {
                stop,
                stop_sequence,
            } => {
                let mut frames = self.close_open_block();
                frames.push((
                    "message_delta".into(),
                    json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": emit_stop(*stop),
                            "stop_sequence": stop_sequence,
                        },
                        "usage": emit_usage(&self.usage),
                    }),
                ));
                frames
            }
            Event::Done => vec![("message_stop".into(), json!({"type": "message_stop"}))],
        }
    }

    /// 上游漏发 `content_block_stop` 时补一帧，保证客户端的状态机不悬空。
    fn close_open_block(&mut self) -> Vec<(String, Value)> {
        match self.open_block.take() {
            Some(index) => vec![(
                "content_block_stop".into(),
                json!({"type": "content_block_stop", "index": index}),
            )],
            None => Vec::new(),
        }
    }
}

/// 流中途出错时的错误帧（§18.2）。
pub fn error_event(
    code: crate::gateway::error::ErrorCode,
    message: &str,
    request_id: Option<&str>,
) -> (String, Value) {
    (
        "error".into(),
        json!({
            "type": "error",
            "error": {
                "type": code.anthropic_type_for_stream(),
                "message": message,
                "code": code.as_str(),
            },
            "request_id": request_id,
        }),
    )
}

/// `thinking` 配置在 Anthropic 与 OpenAI 之间的档位换算入口，供测试断言。
pub fn effort_of(config: &ThinkingConfig) -> Effort {
    config.effort()
}

/// Anthropic 没有结构化输出，这里保留常量让调用方的错误信息统一。
pub const OUTPUT_FORMAT_UNSUPPORTED: &str = "Anthropic Messages 无法表达结构化输出 Schema";

/// 供跨协议模块判断"这个中间请求要求的输出格式是否可表达"。
pub fn supports_output_format(format: Option<&OutputFormat>) -> bool {
    format.is_none()
}

// ------------------------------------------------------------ 思考契约（§14.6）

/// 只认手工预算（`enabled`/`disabled` + `budget_tokens`）的老家族。
///
/// 这张表是**回落名单**：新的 Claude 版本不需要往这里加东西，只有想把某个
/// 家族从自适应契约里排除出去时才需要动它。
const LEGACY_THINKING_FAMILIES: &[&str] = &[
    "claude-3", // 3、3.5、3.7
    "claude-opus-4-0",
    "claude-opus-4.0",
    "claude-opus-4-1",
    "claude-opus-4.1",
    "claude-opus-4-5",
    "claude-opus-4.5",
    "claude-sonnet-4-0",
    "claude-sonnet-4.0",
    "claude-sonnet-4-5",
    "claude-sonnet-4.5",
    "claude-haiku-4-5",
    "claude-haiku-4.5",
    "claude-opus-4-2025", // 带日期的 4.0 命名
    "claude-sonnet-4-2025",
];

/// 自适应但还没有 `display`、也还收采样参数的一代。
const ADAPTIVE_WITHOUT_DISPLAY: &[&str] = &[
    "claude-opus-4-6",
    "claude-opus-4.6",
    "claude-sonnet-4-6",
    "claude-sonnet-4.6",
];

/// 上游模型对思考参数的接受契约。
///
/// 4.6 把手工预算换成了自适应思考（`thinking.type = "adaptive"` +
/// `output_config.effort`），4.7 起连旧形状一起拒掉，还收走
/// `temperature`/`top_p`/`top_k`。这些差异只能看**模型**：同一个
/// `/v1/messages` 上游既可能是 4.5，也可能是 5.x。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingContract {
    /// 用自适应形状表达思考，而不是 `enabled` + `budget_tokens`。
    pub adaptive: bool,
    /// 接受 `thinking.display`（4.7 起思考文本默认不回）。
    pub display: bool,
    /// 接受 `xhigh` 档位（4.7 起才有；4.6 只认 low/medium/high/max）。
    pub xhigh: bool,
    /// 不接受 `temperature` / `top_p` / `top_k`。
    pub no_sampling: bool,
}

impl ThinkingContract {
    /// 手工预算：老 Claude 家族与非 Claude 的 Anthropic 兼容端点。
    pub const MANUAL: Self = Self {
        adaptive: false,
        display: false,
        xhigh: false,
        no_sampling: false,
    };

    /// 按要发出去的那个模型名判断契约。
    ///
    /// 白名单式的版本号匹配会在下一个模型发布时失效——`claude-opus-5-5` 里
    /// 没有任何 "4.x" 子串，按老路走下去就是把新模型 400 掉。所以这里反过来：
    /// 只有**已知的老家族**回落到手工契约，其余 Claude（含还没见过的下一代）
    /// 一律按现代契约走。
    pub fn of(model: &str) -> Self {
        let name = model.to_ascii_lowercase();
        // Kimi / Moonshot 的 Anthropic 兼容端点实现了自适应契约（含 display）。
        if name.contains("kimi") || name.contains("moonshot") {
            return Self {
                adaptive: true,
                display: true,
                xhigh: true,
                no_sampling: false,
            };
        }
        if !name.contains("claude") {
            // GLM、qwen、minimax 这些 Anthropic 兼容端点只实现了老形状。
            return Self::MANUAL;
        }
        if LEGACY_THINKING_FAMILIES
            .iter()
            .any(|family| name.contains(family))
        {
            return Self::MANUAL;
        }
        if ADAPTIVE_WITHOUT_DISPLAY
            .iter()
            .any(|family| name.contains(family))
        {
            return Self {
                adaptive: true,
                display: false,
                xhigh: false,
                no_sampling: false,
            };
        }
        // 真实型号名里一定有版本数字（3、4.5、5.5、fable-5……）。没有数字的名字
        // 多半是别名或自定义端点名，这时按手工契约走：两条路猜错的代价都是一个
        // 400，但"老形状"至少是历史最长、兼容面最广的那一种。
        if !name.chars().any(|c| c.is_ascii_digit()) {
            return Self::MANUAL;
        }
        Self {
            adaptive: true,
            display: true,
            xhigh: true,
            no_sampling: true,
        }
    }
}

/// 客户端想要的思考形态，与上游模型无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThinkingIntent {
    enabled: bool,
    /// 客户端**显式**表达的档位；`None` 表示"没说要多少"。
    effort: Option<Effort>,
    display: Option<ThinkingDisplay>,
}

impl ThinkingIntent {
    fn of(config: &ThinkingConfig) -> Self {
        Self {
            enabled: config.enabled,
            effort: config.explicit_effort(),
            display: config.display,
        }
    }

    /// 只有请求体、没有中间格式时的回读（按上游模型名二次对齐用）。
    fn from_body(body: &Map<String, Value>) -> Option<Self> {
        let thinking = body.get("thinking");
        let effort = body
            .get("output_config")
            .and_then(|config| config.get("effort"))
            .and_then(Value::as_str)
            .and_then(Effort::parse)
            .or_else(|| {
                thinking
                    .and_then(|thinking| thinking.get("budget_tokens"))
                    .and_then(Value::as_u64)
                    .map(|budget| Effort::from_budget(budget as u32))
            });
        if thinking.is_none() && effort.is_none() {
            return None;
        }
        Some(Self {
            enabled: thinking
                .and_then(|thinking| thinking.get("type"))
                .and_then(Value::as_str)
                != Some("disabled"),
            effort,
            display: thinking
                .and_then(|thinking| thinking.get("display"))
                .and_then(Value::as_str)
                .and_then(ThinkingDisplay::parse),
        })
    }
}

/// 把已经发射好的 Messages 请求体对齐到**真正要发出去的那个模型**的契约。
///
/// 跨协议路径上 `emit_request` 只看得见下游的逻辑模型名，而逻辑模型常常只是
/// 一个别名（`claude-opus-5-5[1M]`、分组名……）。调用方因此可以在知道上游
/// 模型之后再对齐一次。两次调用是同一条规则，可以重复执行：形状不对就改成
/// 对的，已经对的不动。
pub fn align_thinking(
    body: &mut Map<String, Value>,
    model: &str,
    origin: Protocol,
    thinking: Option<&ThinkingConfig>,
    degraded: &mut Degradations,
) {
    let contract = ThinkingContract::of(model);
    if contract.no_sampling {
        // 4.7 起这三个参数一律不被接受：留着只会换回一个 400，按白名单降级。
        for field in ["temperature", "top_p", "top_k"] {
            if body.remove(field).is_some() {
                degraded.drop(field);
            }
        }
    }

    let intent = match thinking {
        Some(config) => Some(ThinkingIntent::of(config)),
        None => ThinkingIntent::from_body(body),
    };
    let Some(intent) = intent else {
        return;
    };

    if contract.adaptive {
        body.remove("thinking");
        body.remove("output_config");
        if !intent.enabled {
            // 自适应模型默认就思考，`{"type": "disabled"}` 会被上游 400 拒掉：
            // 只能省掉这个字段，并把"客户端明确要求不思考"记成一次降级。
            degraded.drop("thinking");
            return;
        }
        let mut block = json!({"type": "adaptive"});
        let display = match intent.display {
            Some(display) => Some(display),
            // 跨协议时思考文本要去填另一个协议的推理通道，而 4.7 起默认
            // `omitted`——不显式要就一个字都拿不到。同协议交给客户端自己决定。
            None if contract.display && origin != Protocol::AnthropicMessages => {
                Some(ThinkingDisplay::Summarized)
            }
            None => None,
        };
        if let Some(display) = display {
            block["display"] = json!(display.as_str());
        }
        body.insert("thinking".into(), block);
        // 客户端没表态就不替它选档位：自适应模型的默认档位比中间格式猜的高。
        if let Some(effort) = intent.effort {
            // 4.6 只认 low/medium/high/max：把 xhigh 原样发过去只会换回一个
            // "This model does not support effort level 'xhigh'" 的 400，
            // 折到它接受的最强档位。
            let effort = if effort == Effort::XHigh && !contract.xhigh {
                Effort::Max
            } else {
                effort
            };
            body.insert("output_config".into(), json!({"effort": effort.as_str()}));
        }
        return;
    }

    // 手工契约不认识 `adaptive` 与 `output_config`。发射出来的通常已经是老
    // 形状，只有"逻辑模型名像新模型、真上游是老模型"时才需要改回来。
    let adaptive_form = body
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("adaptive");
    if !adaptive_form {
        return;
    }
    body.remove("output_config");
    body.remove("thinking");
    if !intent.enabled {
        body.insert("thinking".into(), json!({"type": "disabled"}));
        return;
    }
    let budget = intent.effort.unwrap_or(Effort::Medium).budget_tokens();
    body.insert(
        "thinking".into(),
        json!({"type": "enabled", "budget_tokens": budget}),
    );
    let max = body.get("max_tokens").and_then(Value::as_u64).unwrap_or(0) as u32;
    if max <= budget {
        body.insert("max_tokens".into(), json!(budget + 4_096));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_and_tool_results_round_trip() {
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "system": "你是助手",
            "messages": [
                {"role": "user", "content": "查一下天气"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "weather", "input": {"city": "北京"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "晴"}
                ]}
            ],
            "tools": [{"name": "weather", "input_schema": {"type": "object"}}]
        });
        let request = parse_request(&body).unwrap();

        assert_eq!(request.messages[0].role, Role::System);
        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.tools.len(), 1);
        // 工具参数在中间格式里统一是 JSON 文本。
        match &request.messages[2].parts[0] {
            Part::ToolCall { arguments, id, .. } => {
                assert_eq!(id, "toolu_1");
                assert!(arguments.contains("北京"));
            }
            other => panic!("期望工具调用，得到 {other:?}"),
        }

        let emitted = emit_request(&request).unwrap();
        assert!(emitted.is_lossless());
        assert_eq!(emitted.body["system"][0]["text"], "你是助手");
        assert_eq!(
            emitted.body["messages"][1]["content"][0]["input"]["city"],
            "北京"
        );
    }

    #[test]
    fn consecutive_same_role_messages_are_merged() {
        // Chat 的多条 tool 消息转过来会变成连续的 user 轮次，Anthropic 会拒收。
        let mut request = Request::new(Protocol::OpenAiChat, "m");
        for id in ["a", "b"] {
            request.messages.push(Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    call_id: id.into(),
                    content: vec![Part::text("ok")],
                    is_error: false,
                }],
            });
        }
        let emitted = emit_request(&request).unwrap();
        let messages = emitted.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "连续同角色必须合并");
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn unsigned_thinking_is_dropped_and_reported() {
        let mut request = Request::new(Protocol::OpenAiResponses, "m");
        request.messages.push(Message {
            role: Role::Assistant,
            parts: vec![
                Part::Thinking(ThinkingBlock {
                    text: "推理".into(),
                    ..ThinkingBlock::default()
                }),
                Part::text("答案"),
            ],
        });
        let emitted = emit_request(&request).unwrap();
        assert_eq!(emitted.degraded, vec!["thinking".to_string()]);
        let blocks = emitted.body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "没有签名的思考块必须丢弃");

        // 带签名的原生思考块必须原样保留。
        let mut native = Request::new(Protocol::AnthropicMessages, "m");
        native.messages.push(Message {
            role: Role::Assistant,
            parts: vec![Part::Thinking(ThinkingBlock {
                text: "推理".into(),
                signature: Some("sig".into()),
                ..ThinkingBlock::default()
            })],
        });
        let emitted = emit_request(&native).unwrap();
        assert!(emitted.is_lossless());
        assert_eq!(
            emitted.body["messages"][0]["content"][0]["signature"],
            "sig"
        );
    }

    #[test]
    fn structured_output_is_refused_rather_than_faked() {
        // 用提示词或工具伪装严格 Schema 是明确禁止的（§14.6）。
        let mut request = Request::new(Protocol::OpenAiChat, "m");
        request.output_format = Some(OutputFormat::JsonSchema {
            name: "s".into(),
            description: None,
            schema: json!({"type": "object"}),
            strict: true,
        });
        assert!(emit_request(&request).is_err());
    }

    #[test]
    fn thinking_budget_never_exceeds_max_tokens() {
        let mut request = Request::new(Protocol::OpenAiResponses, "m");
        request.max_tokens = Some(1_000);
        request.thinking = Some(ThinkingConfig {
            enabled: true,
            budget_tokens: None,
            effort: Some(Effort::High),
            display: None,
        });
        let emitted = emit_request(&request).unwrap();
        let budget = emitted.body["thinking"]["budget_tokens"].as_u64().unwrap();
        let max = emitted.body["max_tokens"].as_u64().unwrap();
        assert!(max > budget, "预算必须小于 max_tokens，否则上游直接 400");
    }

    #[test]
    fn usage_conversion_keeps_the_anthropic_cache_convention() {
        let usage = parse_usage(Some(&json!({
            "input_tokens": 60,
            "output_tokens": 7,
            "cache_read_input_tokens": 40,
        })));
        // 中间格式的 input 含缓存部分。
        assert_eq!(usage.input, Some(100));
        // 发射回去时再减掉，数字必须回到原样。
        assert_eq!(emit_usage(&usage)["input_tokens"], 60);
        assert_eq!(emit_usage(&usage)["cache_read_input_tokens"], 40);
    }

    #[test]
    fn vendor_builtin_tools_make_the_request_inexpressible() {
        let body = json!({
            "model": "m",
            "max_tokens": 16,
            "messages": [],
            "tools": [{"type": "computer_20250124", "name": "computer"}]
        });
        let request = parse_request(&body).unwrap();
        assert_eq!(request.inexpressible.len(), 1);
        assert!(request.reject_inexpressible().is_err());
    }

    #[test]
    fn unknown_top_level_fields_block_cross_protocol_conversion() {
        let body = json!({
            "model": "m", "max_tokens": 16, "messages": [],
            "厂商私有": {"x": 1}
        });
        let request = parse_request(&body).unwrap();
        assert_eq!(request.unknown, vec!["厂商私有".to_string()]);
        assert!(request.reject_inexpressible().is_err());
    }

    #[test]
    fn top_k_is_degraded_only_when_the_target_cannot_express_it() {
        let body = json!({"model": "m", "max_tokens": 16, "messages": [], "top_k": 40});
        let request = parse_request(&body).unwrap();
        assert!(request.unknown.is_empty(), "top_k 是已知的白名单参数");
        // 回到 Anthropic 自己：无损。
        let emitted = emit_request(&request).unwrap();
        assert_eq!(emitted.body["top_k"], 40);
        assert!(emitted.is_lossless());
    }

    #[test]
    fn stream_emitter_closes_dangling_blocks() {
        let mut emitter = StreamEmitter::new();
        emitter.push(&Event::Start {
            id: "msg_1".into(),
            model: "m".into(),
        });
        emitter.push(&Event::ItemStart {
            index: 0,
            kind: ItemKind::Text,
        });
        // 上游没发 ItemEnd 就直接 Finish：必须补一个 content_block_stop。
        let frames = emitter.push(&Event::Finish {
            stop: StopReason::EndTurn,
            stop_sequence: None,
        });
        assert_eq!(frames[0].0, "content_block_stop");
        assert_eq!(frames[1].0, "message_delta");
        assert_eq!(frames[1].1["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn stream_events_parse_into_canonical_form() {
        let events = parse_event(
            Some("content_block_delta"),
            &json!({"index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"a\""}}),
        );
        assert_eq!(
            events,
            vec![Event::ToolArgsDelta {
                index: 2,
                fragment: "{\"a\"".into()
            }]
        );
        // 签名增量只对 Anthropic 有意义，中间格式里没有位置。
        assert!(
            parse_event(
                Some("content_block_delta"),
                &json!({"delta": {"type": "signature_delta", "signature": "x"}})
            )
            .is_empty()
        );
    }

    // -------------------------------------------------------- 思考契约（§14.6）

    /// 5.x 的命名里没有任何 "4.x" 子串：契约必须由"是不是已知的老家族"决定，
    /// 而不是由"认不认识这个版本号"决定——后者会在下一个模型发布时失效。
    #[test]
    fn thinking_contract_follows_the_model_family_not_the_version_number() {
        assert!(!ThinkingContract::of("claude-opus-4-5").adaptive);
        assert!(!ThinkingContract::of("us.anthropic.claude-sonnet-4-5-20250929-v1:0").adaptive);
        assert!(ThinkingContract::of("claude-opus-4-6").adaptive);
        assert!(!ThinkingContract::of("claude-opus-4-6").display);
        assert!(ThinkingContract::of("claude-opus-5-5[1M]").adaptive);
        assert!(ThinkingContract::of("claude-fable-5").no_sampling);
        assert!(ThinkingContract::of("kimi-k2").adaptive);
        assert!(ThinkingContract::of("claude-mythos-6").adaptive);
        // 非 Claude 的 Anthropic 兼容端点还停在老形状上。
        assert!(!ThinkingContract::of("glm-4.6").adaptive);
        assert!(!ThinkingContract::of("gpt-6-astra-cc-format").adaptive);
        // 没有版本数字的 Claude 名字认不出来，按兼容面最广的老形状走。
        assert!(!ThinkingContract::of("claude-up").adaptive);
        assert!(!ThinkingContract::of("Claude-Relay").adaptive);
    }

    #[test]
    fn adaptive_models_get_adaptive_thinking_instead_of_a_budget() {
        let mut request = Request::new(Protocol::OpenAiResponses, "claude-opus-5-5");
        request.thinking = Some(ThinkingConfig {
            enabled: true,
            budget_tokens: None,
            effort: Some(Effort::XHigh),
            display: None,
        });
        let emitted = emit_request(&request).unwrap();
        assert_eq!(emitted.body["thinking"]["type"], "adaptive");
        assert!(
            emitted.body["thinking"].get("budget_tokens").is_none(),
            "新契约里预算字段会被 400 拒掉"
        );
        assert_eq!(emitted.body["output_config"]["effort"], "xhigh");
        // 跨协议时替客户端要摘要，否则 4.7 起思考文本一个字都拿不到。
        assert_eq!(emitted.body["thinking"]["display"], "summarized");
        assert!(emitted.is_lossless(), "换成新形状不是降级");
    }

    #[test]
    fn an_absent_level_is_not_replaced_by_a_guess() {
        let mut request = Request::new(Protocol::OpenAiResponses, "claude-opus-5-5");
        request.thinking = Some(ThinkingConfig {
            enabled: true,
            budget_tokens: None,
            effort: None,
            display: None,
        });
        let emitted = emit_request(&request).unwrap();
        assert_eq!(emitted.body["thinking"]["type"], "adaptive");
        // 客户端没表态就用模型自己的默认档位，不替它挑一个 medium。
        assert!(emitted.body.get("output_config").is_none());
    }

    #[test]
    fn xhigh_is_folded_to_max_on_models_that_do_not_accept_it() {
        // 4.6 只认 low/medium/high/max：xhigh 会被上游 400 拒掉。
        let mut request = Request::new(Protocol::OpenAiResponses, "claude-opus-4-6");
        request.thinking = Some(ThinkingConfig {
            enabled: true,
            budget_tokens: None,
            effort: Some(Effort::XHigh),
            display: None,
        });
        let emitted = emit_request(&request).unwrap();
        assert_eq!(emitted.body["thinking"]["type"], "adaptive");
        assert_eq!(emitted.body["output_config"]["effort"], "max");

        // 4.7 起 xhigh 是独立档位，原样保留。
        let mut modern = Request::new(Protocol::OpenAiResponses, "claude-opus-5-5");
        modern.thinking = request.thinking;
        let emitted = emit_request(&modern).unwrap();
        assert_eq!(emitted.body["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn adaptive_models_never_receive_a_disabled_thinking_block() {
        let mut request = Request::new(Protocol::OpenAiChat, "claude-sonnet-5");
        request.thinking = Some(ThinkingConfig {
            enabled: false,
            budget_tokens: None,
            effort: None,
            display: None,
        });
        let emitted = emit_request(&request).unwrap();
        assert!(
            emitted.body.get("thinking").is_none(),
            "省略才是新契约下关掉思考的写法"
        );
        assert_eq!(emitted.degraded, vec!["thinking".to_string()]);
    }

    #[test]
    fn legacy_models_keep_the_manual_budget_and_its_max_tokens_guard() {
        let mut request = Request::new(Protocol::OpenAiResponses, "claude-sonnet-4-5-20250929");
        request.max_tokens = Some(4_096);
        request.thinking = Some(ThinkingConfig {
            enabled: true,
            budget_tokens: None,
            effort: Some(Effort::High),
            display: None,
        });
        let emitted = emit_request(&request).unwrap();
        assert_eq!(emitted.body["thinking"]["type"], "enabled");
        assert_eq!(emitted.body["thinking"]["budget_tokens"], 16_384);
        assert!(emitted.body.get("output_config").is_none());
        let max = emitted.body["max_tokens"].as_u64().unwrap();
        assert!(max > 16_384, "预算必须小于 max_tokens，否则上游直接 400");
    }

    #[test]
    fn strict_models_drop_the_sampling_parameters_they_would_reject() {
        let mut request = Request::new(Protocol::OpenAiChat, "claude-opus-5-5");
        request.temperature = Some(0.3);
        request.top_p = Some(0.9);
        request.sampling.insert("top_k".into(), json!(40));
        request.thinking = Some(ThinkingConfig {
            enabled: true,
            budget_tokens: None,
            effort: Some(Effort::High),
            display: None,
        });
        let emitted = emit_request(&request).unwrap();
        assert!(emitted.body.get("temperature").is_none());
        assert!(emitted.body.get("top_p").is_none());
        assert!(emitted.body.get("top_k").is_none());
        assert_eq!(
            emitted.degraded,
            vec!["temperature".to_string(), "top_p".into(), "top_k".into()]
        );

        // 4.5 系还收这些参数，不能顺手丢。
        let mut legacy = Request::new(Protocol::OpenAiChat, "claude-opus-4-5");
        legacy.temperature = Some(0.3);
        legacy.sampling.insert("top_k".into(), json!(40));
        let legacy = emit_request(&legacy).unwrap();
        assert_eq!(legacy.body["temperature"], 0.3);
        assert_eq!(legacy.body["top_k"], 40);
        assert!(legacy.is_lossless());
    }

    #[test]
    fn adaptive_thinking_and_effort_survive_parsing() {
        let body = json!({
            "model": "claude-opus-5-5",
            "max_tokens": 4_096,
            "messages": [],
            "thinking": {"type": "adaptive", "display": "omitted"},
            "output_config": {"effort": "max"}
        });
        let request = parse_request(&body).unwrap();
        let thinking = request.thinking.unwrap();
        assert!(thinking.enabled);
        assert_eq!(thinking.effort, Some(Effort::Max));
        assert_eq!(thinking.display, Some(ThinkingDisplay::Omitted));
        assert!(
            request.unknown.is_empty(),
            "output_config 是已知字段，不该把整条请求判成不可转换"
        );
        // 转给 Chat：Anthropic 专有的 max 折到 high，别的选择原样保留。
        let emitted = crate::protocol::openai_chat::emit_request(&request).unwrap();
        assert_eq!(emitted.body["reasoning_effort"], "high");
    }

    #[test]
    fn unexpressible_output_config_subfields_block_cross_protocol() {
        let body = json!({
            "model": "m", "max_tokens": 16, "messages": [],
            "output_config": {"format": {"type": "json_schema"}}
        });
        let request = parse_request(&body).unwrap();
        assert_eq!(request.unknown, vec!["output_config.format".to_string()]);
        assert!(request.reject_inexpressible().is_err());
    }

    /// 逻辑模型名只是别名，真上游是老模型：按上游名再对齐一次要能改回来，
    /// 而且重复执行不能改出新花样。
    #[test]
    fn alignment_follows_the_real_upstream_model_and_is_idempotent() {
        let mut request = Request::new(Protocol::OpenAiChat, "claude-opus-5-5");
        request.max_tokens = Some(4_096);
        request.thinking = Some(ThinkingConfig {
            enabled: true,
            budget_tokens: None,
            effort: Some(Effort::Max),
            display: None,
        });
        let emitted = emit_request(&request).unwrap();
        assert_eq!(emitted.body["thinking"]["type"], "adaptive");

        let mut body = emitted.body.as_object().unwrap().clone();
        let mut degraded = Degradations::default();
        align_thinking(
            &mut body,
            "claude-opus-4-5",
            Protocol::OpenAiChat,
            request.thinking.as_ref(),
            &mut degraded,
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(
            body.get("output_config").is_none(),
            "老模型不认识 output_config"
        );
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
        assert!(body["max_tokens"].as_u64().unwrap() > budget);

        let once = body.clone();
        align_thinking(
            &mut body,
            "claude-opus-4-5",
            Protocol::OpenAiChat,
            request.thinking.as_ref(),
            &mut degraded,
        );
        assert_eq!(body, once);
        assert!(degraded.into_list().is_empty());
    }

    #[test]
    fn adaptive_alignment_reads_the_intent_back_from_a_body_it_did_not_emit() {
        let mut body = Map::new();
        body.insert("model".into(), json!("claude-opus-5-5"));
        body.insert("max_tokens".into(), json!(4_096));
        body.insert(
            "thinking".into(),
            json!({"type": "enabled", "budget_tokens": 10_000}),
        );
        let mut degraded = Degradations::default();
        align_thinking(
            &mut body,
            "claude-opus-5-5",
            Protocol::AnthropicMessages,
            None,
            &mut degraded,
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        // 同协议：客户端自己管 display，不替它加。
        assert!(body["thinking"].get("display").is_none());
        assert!(degraded.into_list().is_empty());
    }
}
