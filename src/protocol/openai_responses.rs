//! OpenAI Responses 的解析与发射（§14.6、§15）。
//!
//! Responses 的形状特点：
//! 1. `instructions` 是顶层字段，`input` 既可以是字符串也可以是输入项数组。
//! 2. 工具调用与结果是**顶层输入项**，不嵌在消息里，用 `call_id` 关联。
//! 3. 工具定义是扁平的（`{type, name, parameters}`），没有 `function` 包装。
//! 4. 思考是独立的 `reasoning` 项，带 `encrypted_content`。

use serde_json::{Map, Value, json};

use crate::domain::Protocol;
use crate::protocol::canonical::{
    Effort, Event, ItemKind, MediaSource, Message, OutputFormat, Part, Request, Response, Role,
    StopReason, ThinkingBlock, ThinkingConfig, Tool, ToolChoice, Usage,
};
use crate::protocol::degrade::{Degradations, Emitted, Unsupported};
use crate::protocol::{known, sampling_key, string_list};

const KNOWN_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "stream",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "text",
    "max_output_tokens",
    "temperature",
    "top_p",
    "reasoning",
    "user",
    "metadata",
    "store",
    "include",
    "service_tier",
];

const SAMPLING_FIELDS: &[&str] = &["top_logprobs", "truncation"];

/// Responses 专有且无法在别处表达的能力。
const INEXPRESSIBLE_FIELDS: &[&str] = &[
    "previous_response_id",
    "conversation",
    "background",
    "prompt",
];

/// Responses 的内置工具类型，没有等价协议表达（§14.6）。
const VENDOR_TOOL_TYPES: &[&str] = &[
    "web_search",
    "web_search_preview",
    "file_search",
    "computer_use_preview",
    "code_interpreter",
    "image_generation",
    "local_shell",
    "mcp",
];

// ---------------------------------------------------------------- 解析

pub fn parse_request(body: &Value) -> Result<Request, Unsupported> {
    let object = body
        .as_object()
        .ok_or_else(|| Unsupported::new("请求体不是 JSON 对象"))?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| Unsupported::new("请求体缺少 model"))?;
    let mut request = Request::new(Protocol::OpenAiResponses, model);

    request.stream = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    request.include_usage = true;
    request.max_tokens = object
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    request.temperature = object.get("temperature").and_then(Value::as_f64);
    request.top_p = object.get("top_p").and_then(Value::as_f64);
    request.user = object
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_string);
    request.parallel_tool_calls = object.get("parallel_tool_calls").and_then(Value::as_bool);

    if let Some(instructions) = object.get("instructions").and_then(Value::as_str)
        && !instructions.is_empty()
    {
        request.messages.push(Message {
            role: Role::System,
            parts: vec![Part::text(instructions)],
        });
    }
    parse_input(object.get("input"), &mut request.messages);

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
    request.output_format = object
        .get("text")
        .and_then(|text| text.get("format"))
        .and_then(parse_text_format);
    // Responses 用 `text.format` 表达结构化输出；`stop` 不是它的参数。
    request.stop = string_list(object.get("stop"));

    if let Some(reasoning) = object.get("reasoning") {
        let effort = reasoning.get("effort").and_then(Value::as_str);
        request.thinking = Some(match effort {
            Some("none") => ThinkingConfig {
                enabled: false,
                budget_tokens: None,
                effort: None,
            },
            other => ThinkingConfig {
                enabled: true,
                budget_tokens: None,
                effort: other.and_then(Effort::parse),
            },
        });
    }

    for field in SAMPLING_FIELDS {
        if let Some(value) = object.get(*field) {
            request.sampling.insert((*field).to_string(), value.clone());
        }
    }
    for field in INEXPRESSIBLE_FIELDS {
        if object.get(*field).is_some_and(|v| !v.is_null()) {
            request.inexpressible.push(format!("Responses 的 {field}"));
        }
    }
    for (field, _) in object {
        if !known(KNOWN_FIELDS, field)
            && !known(SAMPLING_FIELDS, field)
            && !known(INEXPRESSIBLE_FIELDS, field)
        {
            request.unknown.push(field.clone());
        }
    }
    Ok(request)
}

/// `input` 既可以是一段文本，也可以是输入项数组。
fn parse_input(input: Option<&Value>, messages: &mut Vec<Message>) {
    match input {
        Some(Value::String(text)) if !text.is_empty() => messages.push(Message {
            role: Role::User,
            parts: vec![Part::text(text.clone())],
        }),
        Some(Value::Array(items)) => {
            for item in items {
                parse_item(item, messages);
            }
        }
        _ => {}
    }
}

/// 解析一个输入项。工具调用与结果是顶层项，要归并到相邻的消息里。
fn parse_item(item: &Value, messages: &mut Vec<Message>) {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        // 没有 type 的项按消息处理（官方 SDK 允许省略）。
        .unwrap_or("message");

    match kind {
        "message" => {
            let role = match item.get("role").and_then(Value::as_str) {
                Some("system") => Role::System,
                Some("developer") => Role::Developer,
                Some("assistant") => Role::Assistant,
                _ => Role::User,
            };
            let parts = parse_content(item.get("content").unwrap_or(&Value::Null));
            messages.push(Message { role, parts });
        }
        "function_call" => {
            let part = Part::ToolCall {
                id: item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string(),
            };
            append(messages, Role::Assistant, part);
        }
        "function_call_output" => {
            let part = Part::ToolResult {
                call_id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                content: parse_output_content(item.get("output")),
                is_error: false,
            };
            append(messages, Role::User, part);
        }
        "reasoning" => {
            let text = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let part = Part::Thinking(ThinkingBlock {
                text,
                signature: None,
                encrypted: item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                id: item.get("id").and_then(Value::as_str).map(str::to_string),
                redacted: false,
            });
            append(messages, Role::Assistant, part);
        }
        _ => {}
    }
}

/// 把一个块追加到最后一条同角色消息，必要时新建一条。
fn append(messages: &mut Vec<Message>, role: Role, part: Part) {
    match messages.last_mut() {
        Some(last) if last.role == role => last.parts.push(part),
        _ => messages.push(Message {
            role,
            parts: vec![part],
        }),
    }
}

fn parse_content(content: &Value) -> Vec<Part> {
    match content {
        Value::String(text) if !text.is_empty() => vec![Part::text(text.clone())],
        Value::Array(items) => items.iter().filter_map(parse_content_part).collect(),
        _ => Vec::new(),
    }
}

fn parse_content_part(part: &Value) -> Option<Part> {
    match part.get("type").and_then(Value::as_str)? {
        "input_text" | "output_text" | "text" => Some(Part::text(
            part.get("text").and_then(Value::as_str).unwrap_or_default(),
        )),
        "input_image" => {
            // `file_id` 引用的是上游自己的文件资源，跨协议无法表达。
            let url = part.get("image_url").and_then(Value::as_str)?;
            let source = MediaSource::from_url(url);
            Some(Part::Image {
                source,
                detail: part
                    .get("detail")
                    .and_then(Value::as_str)
                    .filter(|detail| *detail != "auto")
                    .map(str::to_string),
            })
        }
        "input_file" => Some(Part::Document {
            source: MediaSource::from_url(part.get("file_data").and_then(Value::as_str)?),
            name: part
                .get("filename")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "refusal" => Some(Part::Refusal(
            part.get("refusal")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        _ => None,
    }
}

/// `function_call_output.output` 可以是字符串，也可以是内容块数组。
fn parse_output_content(output: Option<&Value>) -> Vec<Part> {
    match output {
        Some(Value::String(text)) => vec![Part::text(text.clone())],
        Some(Value::Array(items)) => items.iter().filter_map(parse_content_part).collect(),
        _ => Vec::new(),
    }
}

fn parse_tool(tool: &Value) -> Result<Tool, String> {
    let kind = tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    if kind != "function" {
        if VENDOR_TOOL_TYPES.iter().any(|v| kind.starts_with(v)) {
            return Err(format!("Responses 内置工具 {kind}"));
        }
        return Err(format!("未知的工具类型 {kind}"));
    }
    // Responses 的函数工具是扁平的，但官方 SDK 也接受 Chat 风格的嵌套。
    let source = tool.get("function").unwrap_or(tool);
    let name = source
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "缺少名称的工具定义".to_string())?;
    Ok(Tool {
        name: name.to_string(),
        description: source
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        parameters: source
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
        strict: source.get("strict").and_then(Value::as_bool),
    })
}

fn parse_tool_choice(choice: &Value) -> Option<ToolChoice> {
    match choice {
        Value::String(text) => match text.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" | "any" => Some(ToolChoice::Required),
            _ => None,
        },
        Value::Object(_) => Some(ToolChoice::Named(
            choice.get("name").and_then(Value::as_str)?.to_string(),
        )),
        _ => None,
    }
}

fn parse_text_format(format: &Value) -> Option<OutputFormat> {
    match format.get("type").and_then(Value::as_str)? {
        "json_object" => Some(OutputFormat::JsonObject),
        "json_schema" => Some(OutputFormat::JsonSchema {
            name: format
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("response")
                .to_string(),
            description: format
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            schema: format.get("schema").cloned().unwrap_or(Value::Null),
            strict: format
                .get("strict")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        _ => None,
    }
}

pub fn parse_response(body: &Value) -> Result<Response, Unsupported> {
    let mut messages: Vec<Message> = Vec::new();
    for item in body
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        parse_item(item, &mut messages);
    }
    let parts: Vec<Part> = messages.into_iter().flat_map(|m| m.parts).collect();

    let incomplete = body
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str);
    let stop = match (body.get("status").and_then(Value::as_str), incomplete) {
        (_, Some("max_output_tokens")) => StopReason::MaxTokens,
        (_, Some("content_filter")) => StopReason::ContentFilter,
        _ if parts.iter().any(|p| matches!(p, Part::ToolCall { .. })) => StopReason::ToolUse,
        _ if parts.iter().any(|p| matches!(p, Part::Refusal(_))) => StopReason::Refusal,
        _ => StopReason::EndTurn,
    };

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
        stop,
        stop_sequence: None,
        usage: parse_usage(body.get("usage")),
        parts,
    })
}

fn parse_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    // 缓存与思考的父字段名在两种 OpenAI 协议下不同，交给共享的
    // CACHE_TOKEN_PARENTS / REASONING_TOKEN_PARENTS 判定，避免这里与
    // 流式结算各写一套而慢慢分叉（§11.6）。
    Usage {
        input: field("input_tokens"),
        output: field("output_tokens"),
        cache_read: super::cache_read_tokens(usage),
        cache_write: None,
        reasoning: super::reasoning_tokens(usage),
    }
}

// ---------------------------------------------------------------- 发射

pub fn emit_request(request: &Request) -> Result<Emitted, Unsupported> {
    request.reject_inexpressible()?;
    let mut degraded = Degradations::default();

    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    if request.stream {
        body.insert("stream".into(), json!(true));
    }

    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();
    for message in &request.messages {
        match message.role {
            // Responses 的 instructions 只接受纯文本；system 消息里的非文本块
            // 保留成 developer 消息，避免丢内容。
            Role::System | Role::Developer => {
                let text = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    instructions.push(text);
                }
                let others: Vec<&Part> = message
                    .parts
                    .iter()
                    .filter(|part| !matches!(part, Part::Text(_)))
                    .collect();
                if !others.is_empty() {
                    let content = emit_content(&others, "developer", &mut degraded)?;
                    input.push(json!({
                        "type": "message", "role": "developer", "content": content
                    }));
                }
            }
            Role::User | Role::Assistant => emit_message(message, &mut input, &mut degraded)?,
        }
    }
    if !instructions.is_empty() {
        body.insert("instructions".into(), json!(instructions.join("\n\n")));
    }
    body.insert("input".into(), Value::Array(input));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                let mut value = json!({
                    "type": "function",
                    "name": tool.name,
                    "parameters": tool.parameters,
                    // Responses 要求显式给出 strict，null 会被拒。
                    "strict": tool.strict.unwrap_or(false),
                });
                if let Some(description) = &tool.description
                    && let Some(object) = value.as_object_mut()
                {
                    object.insert("description".into(), json!(description));
                }
                value
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = &request.tool_choice {
        body.insert(
            "tool_choice".into(),
            match choice {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Named(name) => json!({"type": "function", "name": name}),
            },
        );
    }
    if let Some(parallel) = request.parallel_tool_calls {
        body.insert("parallel_tool_calls".into(), json!(parallel));
    }
    if let Some(format) = &request.output_format {
        body.insert("text".into(), json!({"format": emit_text_format(format)}));
    }
    if let Some(thinking) = request.thinking {
        if thinking.enabled {
            body.insert(
                "reasoning".into(),
                json!({"effort": thinking.effort().as_str(), "summary": "auto"}),
            );
        } else {
            body.insert("reasoning".into(), json!({"effort": "none"}));
        }
    }
    if let Some(max) = request.max_tokens {
        body.insert("max_output_tokens".into(), json!(max));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    // Responses 没有停止序列这个参数，属于白名单外……但它在 §14.8 的表里既不
    // 是工具也不是 Schema，丢掉只影响生成边界，按采样参数处理。
    if !request.stop.is_empty() {
        degraded.drop("stop");
    }
    if let Some(user) = &request.user {
        body.insert("user".into(), json!(user));
    }
    for (field, value) in &request.sampling {
        match sampling_key(field, Protocol::OpenAiResponses) {
            Some(target) => {
                body.insert(target.into(), value.clone());
            }
            None => degraded.drop(field),
        }
    }

    Ok(Emitted {
        body: Value::Object(body),
        degraded: degraded.into_list(),
        structured_tool: None,
    })
}

/// 发射一条消息。工具调用、工具结果与思考都要拆成顶层输入项。
fn emit_message(
    message: &Message,
    input: &mut Vec<Value>,
    degraded: &mut Degradations,
) -> Result<(), Unsupported> {
    let role = if message.role == Role::Assistant {
        "assistant"
    } else {
        "user"
    };
    let mut content: Vec<&Part> = Vec::new();

    for part in &message.parts {
        match part {
            Part::ToolCall {
                id,
                name,
                arguments,
            } => {
                flush(input, role, &mut content, degraded)?;
                input.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
            Part::ToolResult {
                call_id,
                content: result,
                ..
            } => {
                flush(input, role, &mut content, degraded)?;
                let refs: Vec<&Part> = result.iter().collect();
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": emit_content(&refs, "user", degraded)?,
                }));
            }
            Part::Thinking(thinking) => {
                // 只有带 encrypted_content 的原生 reasoning 项能被上游接受；
                // 其他来源的思考文本无处安放，按白名单丢弃（§14.8）。
                match &thinking.encrypted {
                    Some(encrypted) => {
                        flush(input, role, &mut content, degraded)?;
                        let mut item = json!({
                            "type": "reasoning",
                            "encrypted_content": encrypted,
                            "summary": summary_blocks(&thinking.text),
                        });
                        if let Some(id) = &thinking.id
                            && let Some(object) = item.as_object_mut()
                        {
                            object.insert("id".into(), json!(id));
                        }
                        input.push(item);
                    }
                    None => degraded.drop("thinking"),
                }
            }
            other => content.push(other),
        }
    }
    flush(input, role, &mut content, degraded)
}

fn summary_blocks(text: &str) -> Value {
    if text.is_empty() {
        return json!([]);
    }
    json!([{"type": "summary_text", "text": text}])
}

fn flush(
    input: &mut Vec<Value>,
    role: &str,
    content: &mut Vec<&Part>,
    degraded: &mut Degradations,
) -> Result<(), Unsupported> {
    if content.is_empty() {
        return Ok(());
    }
    let blocks = emit_content(content, role, degraded)?;
    content.clear();
    input.push(json!({"type": "message", "role": role, "content": blocks}));
    Ok(())
}

/// 内容块的 `type` 在 Responses 里区分输入与输出：`input_text` 对
/// `output_text`。角色决定用哪一套。
fn emit_content(
    parts: &[&Part],
    role: &str,
    _degraded: &mut Degradations,
) -> Result<Vec<Value>, Unsupported> {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            Part::Text(text) if text.is_empty() => {}
            Part::Text(text) => blocks.push(json!({"type": text_type, "text": text})),
            Part::Refusal(text) => blocks.push(json!({"type": "refusal", "refusal": text})),
            Part::Image { source, detail } => {
                let mut block = json!({
                    "type": "input_image",
                    "image_url": source.to_url(),
                    "detail": detail.clone().unwrap_or_else(|| "auto".into()),
                });
                if let Some(object) = block.as_object_mut()
                    && detail.is_none()
                {
                    object.insert("detail".into(), json!("auto"));
                }
                blocks.push(block);
            }
            Part::Document { source, name } => blocks.push(json!({
                "type": "input_file",
                "filename": name.clone().unwrap_or_else(|| "file".into()),
                "file_data": source.to_url(),
            })),
            // 这三种在调用方已经被拆成顶层项，不该走到这里。
            Part::ToolCall { .. } | Part::ToolResult { .. } | Part::Thinking(_) => {
                return Err(Unsupported::new("工具项必须作为顶层输入项发射"));
            }
        }
    }
    Ok(blocks)
}

fn emit_text_format(format: &OutputFormat) -> Value {
    match format {
        OutputFormat::JsonObject => json!({"type": "json_object"}),
        OutputFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        } => {
            let mut value = json!({
                "type": "json_schema",
                "name": name,
                "schema": schema,
                "strict": strict,
            });
            if let Some(description) = description
                && let Some(object) = value.as_object_mut()
            {
                object.insert("description".into(), json!(description));
            }
            value
        }
    }
}

pub fn emit_response(response: &Response) -> Result<Value, Unsupported> {
    let mut degraded = Degradations::default();
    let mut output: Vec<Value> = Vec::new();
    emit_message(
        &Message {
            role: Role::Assistant,
            parts: response.parts.clone(),
        },
        &mut output,
        &mut degraded,
    )?;
    // Responses 的输出消息项需要 id 与 status。
    for item in &mut output {
        if item.get("type").and_then(Value::as_str) == Some("message")
            && let Some(object) = item.as_object_mut()
        {
            object.insert("id".into(), json!(format!("msg_{}", response.id)));
            object.insert("status".into(), json!("completed"));
        }
    }

    let mut body = json!({
        "id": response.id,
        "object": "response",
        "created_at": 0,
        "model": response.model,
        "status": status_of(response.stop),
        "output": output,
        "usage": emit_usage(&response.usage),
    });
    if let Some(reason) = incomplete_reason(response.stop)
        && let Some(object) = body.as_object_mut()
    {
        object.insert("incomplete_details".into(), json!({"reason": reason}));
    }
    Ok(body)
}

fn status_of(stop: StopReason) -> &'static str {
    match stop {
        StopReason::MaxTokens | StopReason::ContentFilter => "incomplete",
        _ => "completed",
    }
}

fn incomplete_reason(stop: StopReason) -> Option<&'static str> {
    match stop {
        StopReason::MaxTokens => Some("max_output_tokens"),
        StopReason::ContentFilter => Some("content_filter"),
        _ => None,
    }
}

fn emit_usage(usage: &Usage) -> Value {
    let mut object = Map::new();
    if let Some(input) = usage.input {
        object.insert("input_tokens".into(), json!(input));
    }
    if let Some(output) = usage.output {
        object.insert("output_tokens".into(), json!(output));
    }
    if let (Some(input), Some(output)) = (usage.input, usage.output) {
        object.insert("total_tokens".into(), json!(input + output));
    }
    if let Some(read) = usage.cache_read {
        object.insert(
            "input_tokens_details".into(),
            json!({"cached_tokens": read}),
        );
    }
    if let Some(reasoning) = usage.reasoning {
        object.insert(
            "output_tokens_details".into(),
            json!({"reasoning_tokens": reasoning}),
        );
    }
    if let Some(write) = usage.cache_write {
        object.insert("akhub_cache_creation_tokens".into(), json!(write));
    }
    Value::Object(object)
}

// ------------------------------------------------------------ 流式解析

/// Responses 的流式事件已经带 `output_index`，与中间格式的序号一致，所以
/// 解析器只需要记住 `response.id` 与工具调用的 ID。
pub fn parse_event(event: Option<&str>, data: &Value) -> Vec<Event> {
    let kind = event
        .or_else(|| data.get("type").and_then(Value::as_str))
        .unwrap_or_default();
    let index = data
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;

    match kind {
        "response.created" | "response.in_progress" | "response.queued" => {
            let response = data.get("response");
            vec![Event::Start {
                id: field(response, "id"),
                model: field(response, "model"),
            }]
        }
        "response.output_item.added" => {
            let item = data.get("item");
            let kind = match item.and_then(|i| i.get("type")).and_then(Value::as_str) {
                Some("function_call") => ItemKind::ToolCall {
                    id: item
                        .and_then(|i| i.get("call_id").or_else(|| i.get("id")))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: field(item, "name"),
                },
                Some("reasoning") => ItemKind::Thinking,
                _ => ItemKind::Text,
            };
            vec![Event::ItemStart { index, kind }]
        }
        "response.output_text.delta" => vec![Event::TextDelta {
            index,
            text: field(Some(data), "delta"),
        }],
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            vec![Event::ThinkingDelta {
                index,
                text: field(Some(data), "delta"),
            }]
        }
        "response.function_call_arguments.delta" => vec![Event::ToolArgsDelta {
            index,
            fragment: field(Some(data), "delta"),
        }],
        "response.output_item.done" => vec![Event::ItemEnd { index }],
        "response.completed" | "response.incomplete" => {
            let response = data.get("response");
            let mut events = Vec::new();
            let usage = parse_usage(response.and_then(|r| r.get("usage")));
            if !usage.is_empty() {
                events.push(Event::Usage(usage));
            }
            let stop = response
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .map(|reason| match reason {
                    "max_output_tokens" => StopReason::MaxTokens,
                    "content_filter" => StopReason::ContentFilter,
                    _ => StopReason::EndTurn,
                })
                .unwrap_or_else(|| {
                    // 输出里有工具调用就是工具轮次。
                    let has_call = response
                        .and_then(|r| r.get("output"))
                        .and_then(Value::as_array)
                        .is_some_and(|items| {
                            items.iter().any(|item| {
                                item.get("type").and_then(Value::as_str) == Some("function_call")
                            })
                        });
                    if has_call {
                        StopReason::ToolUse
                    } else {
                        StopReason::EndTurn
                    }
                });
            events.push(Event::Finish {
                stop,
                stop_sequence: None,
            });
            events.push(Event::Done);
            events
        }
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

/// 把中间事件发射成 Responses SSE。
///
/// Responses 的事件序列最严格：每个输出项都要有 `added` / `done`，文本项还要
/// 有 `content_part.added` 与 `output_text.done`，最后必须以
/// `response.completed` 收尾并带完整的 `response` 对象。
#[derive(Debug, Default)]
pub struct StreamEmitter {
    id: String,
    model: String,
    sequence: u64,
    usage: Usage,
    /// 当前打开的项：序号、种类、已累积的文本或参数。
    open: Option<OpenItem>,
    output: Vec<Value>,
    stop: StopReason,
}

#[derive(Debug, Clone)]
struct OpenItem {
    index: usize,
    kind: ItemKind,
    text: String,
}

impl StreamEmitter {
    /// `preset_id` 是网关自己的 `resp_akh_*` ID：跨协议进入 Responses 时，
    /// 客户端从第一个事件起引用的必须是网关 ID，而不是临时生成的上游形状 ID。
    pub fn new(preset_id: Option<String>) -> Self {
        Self {
            id: preset_id.unwrap_or_default(),
            ..Self::default()
        }
    }

    pub fn push(&mut self, event: &Event) -> Vec<(String, Value)> {
        match event {
            Event::Start { id, model } => {
                // 预设的网关 ID 永远优先：上游 Chat/Messages 的 id 绝不能
                // 泄漏给 Responses 客户端。
                if self.id.is_empty() {
                    self.id = if id.is_empty() {
                        format!("resp_{}", ulid::Ulid::generate())
                    } else {
                        id.clone()
                    };
                }
                self.model = model.clone();
                vec![
                    self.frame(
                        "response.created",
                        json!({"response": self.envelope("in_progress")}),
                    ),
                    self.frame(
                        "response.in_progress",
                        json!({"response": self.envelope("in_progress")}),
                    ),
                ]
            }
            Event::ItemStart { index, kind } => {
                let mut frames = self.close_open();
                self.open = Some(OpenItem {
                    index: *index,
                    kind: kind.clone(),
                    text: String::new(),
                });
                let item = self.item_shell(*index, kind);
                frames.push(self.frame(
                    "response.output_item.added",
                    json!({"output_index": index, "item": item}),
                ));
                if matches!(kind, ItemKind::Text | ItemKind::Refusal) {
                    frames.push(self.frame(
                        "response.content_part.added",
                        json!({
                            "output_index": index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": ""},
                        }),
                    ));
                }
                frames
            }
            Event::TextDelta { index, text } => {
                if let Some(open) = &mut self.open {
                    open.text.push_str(text);
                }
                vec![self.frame(
                    "response.output_text.delta",
                    json!({"output_index": index, "content_index": 0, "delta": text}),
                )]
            }
            Event::ThinkingDelta { index, text } => {
                if let Some(open) = &mut self.open {
                    open.text.push_str(text);
                }
                vec![self.frame(
                    "response.reasoning_summary_text.delta",
                    json!({"output_index": index, "summary_index": 0, "delta": text}),
                )]
            }
            Event::ToolArgsDelta { index, fragment } => {
                if let Some(open) = &mut self.open {
                    open.text.push_str(fragment);
                }
                vec![self.frame(
                    "response.function_call_arguments.delta",
                    json!({"output_index": index, "delta": fragment}),
                )]
            }
            Event::ItemEnd { .. } => self.close_open(),
            Event::Usage(usage) => {
                self.usage.merge(*usage);
                Vec::new()
            }
            Event::Finish { stop, .. } => {
                self.stop = *stop;
                self.close_open()
            }
            Event::Done => {
                let mut frames = self.close_open();
                let status = status_of(self.stop);
                let mut envelope = self.envelope(status);
                if let Some(reason) = incomplete_reason(self.stop)
                    && let Some(object) = envelope.as_object_mut()
                {
                    object.insert("incomplete_details".into(), json!({"reason": reason}));
                }
                let event = if status == "completed" {
                    "response.completed"
                } else {
                    "response.incomplete"
                };
                frames.push(self.frame(event, json!({"response": envelope})));
                frames
            }
        }
    }

    /// 关闭当前项，补齐 Responses 要求的收尾事件。
    fn close_open(&mut self) -> Vec<(String, Value)> {
        let Some(open) = self.open.take() else {
            return Vec::new();
        };
        let mut frames = Vec::new();
        match &open.kind {
            ItemKind::Text | ItemKind::Refusal => {
                frames.push(self.frame(
                    "response.output_text.done",
                    json!({
                        "output_index": open.index,
                        "content_index": 0,
                        "text": open.text,
                    }),
                ));
                frames.push(self.frame(
                    "response.content_part.done",
                    json!({
                        "output_index": open.index,
                        "content_index": 0,
                        "part": {"type": "output_text", "text": open.text},
                    }),
                ));
            }
            ItemKind::Thinking => frames.push(self.frame(
                "response.reasoning_summary_text.done",
                json!({
                    "output_index": open.index,
                    "summary_index": 0,
                    "text": open.text,
                }),
            )),
            ItemKind::ToolCall { .. } => frames.push(self.frame(
                "response.function_call_arguments.done",
                json!({"output_index": open.index, "arguments": open.text}),
            )),
        }

        let item = self.completed_item(&open);
        self.output.push(item.clone());
        frames.push(self.frame(
            "response.output_item.done",
            json!({"output_index": open.index, "item": item}),
        ));
        frames
    }

    fn item_shell(&self, index: usize, kind: &ItemKind) -> Value {
        match kind {
            ItemKind::ToolCall { id, name } => json!({
                "type": "function_call",
                "id": format!("fc_{}_{index}", self.id),
                "call_id": id,
                "name": name,
                "arguments": "",
                "status": "in_progress",
            }),
            ItemKind::Thinking => json!({
                "type": "reasoning",
                "id": format!("rs_{}_{index}", self.id),
                "summary": [],
            }),
            _ => json!({
                "type": "message",
                "id": format!("msg_{}_{index}", self.id),
                "role": "assistant",
                "content": [],
                "status": "in_progress",
            }),
        }
    }

    fn completed_item(&self, open: &OpenItem) -> Value {
        match &open.kind {
            ItemKind::ToolCall { id, name } => json!({
                "type": "function_call",
                "id": format!("fc_{}_{}", self.id, open.index),
                "call_id": id,
                "name": name,
                "arguments": open.text,
                "status": "completed",
            }),
            ItemKind::Thinking => json!({
                "type": "reasoning",
                "id": format!("rs_{}_{}", self.id, open.index),
                "summary": summary_blocks(&open.text),
            }),
            _ => json!({
                "type": "message",
                "id": format!("msg_{}_{}", self.id, open.index),
                "role": "assistant",
                "content": [{"type": "output_text", "text": open.text, "annotations": []}],
                "status": "completed",
            }),
        }
    }

    fn envelope(&self, status: &str) -> Value {
        let mut value = json!({
            "id": self.id,
            "object": "response",
            "created_at": 0,
            "model": self.model,
            "status": status,
            "output": self.output,
        });
        if !self.usage.is_empty()
            && let Some(object) = value.as_object_mut()
        {
            object.insert("usage".into(), emit_usage(&self.usage));
        }
        value
    }

    fn frame(&mut self, event: &str, mut data: Value) -> (String, Value) {
        if let Some(object) = data.as_object_mut() {
            object.insert("type".into(), json!(event));
            object.insert("sequence_number".into(), json!(self.sequence));
        }
        self.sequence += 1;
        (event.to_string(), data)
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
            "code": code.as_str(),
            "message": message,
            "request_id": request_id,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_tool_items_fold_into_messages_and_unfold_again() {
        let body = json!({
            "model": "gpt-5",
            "instructions": "你是助手",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "天气"}
                ]},
                {"type": "function_call", "call_id": "call_1", "name": "weather",
                 "arguments": "{\"city\":\"北京\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "晴"}
            ],
            "tools": [{"type": "function", "name": "weather", "parameters": {"type": "object"}}]
        });
        let request = parse_request(&body).unwrap();
        assert_eq!(request.messages[0].role, Role::System);
        assert_eq!(request.tools.len(), 1);
        assert!(matches!(
            request.messages[2].parts[0],
            Part::ToolCall { .. }
        ));

        let emitted = emit_request(&request).unwrap();
        assert!(emitted.is_lossless());
        assert_eq!(emitted.body["instructions"], "你是助手");
        let input = emitted.body["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(emitted.body["tools"][0]["name"], "weather");
    }

    #[test]
    fn response_chain_fields_are_inexpressible_across_protocols() {
        // previous_response_id 只有原生 Responses 上游能理解（§15.2 是阶段 4）。
        let body = json!({"model": "m", "input": "hi", "previous_response_id": "resp_1"});
        let request = parse_request(&body).unwrap();
        assert!(request.reject_inexpressible().is_err());

        // 显式给 null 不算使用了这个字段。
        let body = json!({"model": "m", "input": "hi", "previous_response_id": null});
        assert!(parse_request(&body).unwrap().reject_inexpressible().is_ok());
    }

    #[test]
    fn encrypted_reasoning_survives_but_bare_thinking_is_dropped() {
        let mut request = Request::new(Protocol::OpenAiResponses, "m");
        request.messages.push(Message {
            role: Role::Assistant,
            parts: vec![Part::Thinking(ThinkingBlock {
                text: "摘要".into(),
                encrypted: Some("enc".into()),
                id: Some("rs_1".into()),
                ..ThinkingBlock::default()
            })],
        });
        let emitted = emit_request(&request).unwrap();
        assert!(emitted.is_lossless());
        assert_eq!(emitted.body["input"][0]["encrypted_content"], "enc");
        assert_eq!(emitted.body["input"][0]["summary"][0]["text"], "摘要");

        // 来自 Anthropic 的思考块只有签名，Responses 用不了。
        let mut foreign = Request::new(Protocol::AnthropicMessages, "m");
        foreign.messages.push(Message {
            role: Role::Assistant,
            parts: vec![Part::Thinking(ThinkingBlock {
                text: "推理".into(),
                signature: Some("sig".into()),
                ..ThinkingBlock::default()
            })],
        });
        let emitted = emit_request(&foreign).unwrap();
        assert_eq!(emitted.degraded, vec!["thinking".to_string()]);
    }

    #[test]
    fn builtin_tools_are_rejected_not_silently_dropped() {
        let body = json!({
            "model": "m", "input": "hi",
            "tools": [{"type": "web_search_preview"}]
        });
        let request = parse_request(&body).unwrap();
        assert_eq!(request.inexpressible.len(), 1);
        assert!(request.reject_inexpressible().is_err());
    }

    #[test]
    fn structured_output_maps_onto_text_format() {
        let schema = json!({"type": "object"});
        let body = json!({
            "model": "m", "input": "hi",
            "text": {"format": {"type": "json_schema", "name": "out", "strict": true, "schema": schema}}
        });
        let request = parse_request(&body).unwrap();
        let emitted = emit_request(&request).unwrap();
        assert_eq!(emitted.body["text"]["format"]["type"], "json_schema");
        assert_eq!(emitted.body["text"]["format"]["schema"], schema);
        assert_eq!(emitted.body["text"]["format"]["strict"], true);
    }

    #[test]
    fn streaming_events_parse_and_emit_symmetrically() {
        let events = parse_event(
            Some("response.output_text.delta"),
            &json!({"output_index": 0, "delta": "嗨"}),
        );
        assert_eq!(
            events,
            vec![Event::TextDelta {
                index: 0,
                text: "嗨".into()
            }]
        );

        let mut emitter = StreamEmitter::new(None);
        emitter.push(&Event::Start {
            id: "resp_1".into(),
            model: "m".into(),
        });
        emitter.push(&Event::ItemStart {
            index: 0,
            kind: ItemKind::Text,
        });
        emitter.push(&Event::TextDelta {
            index: 0,
            text: "嗨".into(),
        });
        emitter.push(&Event::Finish {
            stop: StopReason::EndTurn,
            stop_sequence: None,
        });
        let frames = emitter.push(&Event::Done);
        let names: Vec<&str> = frames.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names.last(), Some(&"response.completed"));
        // 收尾的 response 对象必须带完整输出，SDK 依赖它拿最终文本。
        let completed = &frames.last().unwrap().1;
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "嗨"
        );
    }

    #[test]
    fn sequence_numbers_are_monotonic() {
        let mut emitter = StreamEmitter::new(None);
        let mut frames = emitter.push(&Event::Start {
            id: "r".into(),
            model: "m".into(),
        });
        frames.extend(emitter.push(&Event::ItemStart {
            index: 0,
            kind: ItemKind::Text,
        }));
        let numbers: Vec<u64> = frames
            .iter()
            .map(|(_, data)| data["sequence_number"].as_u64().unwrap())
            .collect();
        assert_eq!(numbers, vec![0, 1, 2, 3]);
    }

    #[test]
    fn incomplete_responses_report_max_output_tokens() {
        let response = parse_response(&json!({
            "id": "resp_1", "model": "m", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "半句"}]}]
        }))
        .unwrap();
        assert_eq!(response.stop, StopReason::MaxTokens);
        let emitted = emit_response(&response).unwrap();
        assert_eq!(emitted["status"], "incomplete");
        assert_eq!(emitted["incomplete_details"]["reason"], "max_output_tokens");
    }
}
