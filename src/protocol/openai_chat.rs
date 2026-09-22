//! OpenAI Chat Completions 的解析与发射（§14.6）。
//!
//! Chat 的形状特点：
//! 1. `system` / `developer` 是消息数组里的普通条目。
//! 2. 工具结果是独立的 `role: "tool"` 消息，用 `tool_call_id` 关联。
//! 3. 工具参数是 JSON **字符串**，与中间格式一致。
//! 4. 思考只有 `reasoning_effort` 一个入口，历史里的思考块无处安放。

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
    "messages",
    "stream",
    "stream_options",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "response_format",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "stop",
    "reasoning_effort",
    "user",
    "metadata",
    "store",
    "service_tier",
];

/// OpenAI 专有采样参数。目标协议表达不了时按白名单降级（§14.8）。
const SAMPLING_FIELDS: &[&str] = &[
    "frequency_penalty",
    "presence_penalty",
    "seed",
    "logit_bias",
    "logprobs",
    "top_logprobs",
];

/// 明确无法在其他协议表达的字段。
const INEXPRESSIBLE_FIELDS: &[&str] = &[
    "n",
    "audio",
    "modalities",
    "prediction",
    "web_search_options",
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
    let mut request = Request::new(Protocol::OpenAiChat, model);

    request.stream = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    request.include_usage = object
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    request.max_tokens = object
        .get("max_completion_tokens")
        .or_else(|| object.get("max_tokens"))
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    request.temperature = object.get("temperature").and_then(Value::as_f64);
    request.top_p = object.get("top_p").and_then(Value::as_f64);
    request.stop = string_list(object.get("stop"));
    request.user = object
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_string);
    request.parallel_tool_calls = object.get("parallel_tool_calls").and_then(Value::as_bool);

    for message in object
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        parse_message(message, &mut request.messages);
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
    request.output_format = object
        .get("response_format")
        .and_then(parse_response_format);

    if let Some(effort) = object.get("reasoning_effort").and_then(Value::as_str) {
        request.thinking = Some(match Effort::parse(effort) {
            Some(effort) => ThinkingConfig {
                enabled: true,
                budget_tokens: None,
                effort: Some(effort),
            },
            // `reasoning_effort: "none"` 是显式关闭。
            None => ThinkingConfig {
                enabled: false,
                budget_tokens: None,
                effort: None,
            },
        });
    }

    for field in SAMPLING_FIELDS {
        if let Some(value) = object.get(*field) {
            request.sampling.insert((*field).to_string(), value.clone());
        }
    }
    for field in INEXPRESSIBLE_FIELDS {
        if object.contains_key(*field) {
            request
                .inexpressible
                .push(format!("OpenAI Chat 的 {field}"));
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

/// 解析一条 Chat 消息。`tool` 角色会被折叠进上一条用户轮次的工具结果块。
fn parse_message(message: &Value, messages: &mut Vec<Message>) {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    if role == "tool" {
        let part = Part::ToolResult {
            call_id: message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            content: parse_content(message.get("content").unwrap_or(&Value::Null)),
            is_error: false,
        };
        // 工具结果在中间格式里属于用户轮次（Anthropic 的形状）。
        match messages.last_mut() {
            Some(last) if last.role == Role::User => last.parts.push(part),
            _ => messages.push(Message {
                role: Role::User,
                parts: vec![part],
            }),
        }
        return;
    }

    let mut parts = parse_content(message.get("content").unwrap_or(&Value::Null));
    if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
        parts.push(Part::Refusal(refusal.to_string()));
    }
    // OpenAI 兼容站点常把思考文本放在 `reasoning_content` 里。
    if let Some(reasoning) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        parts.insert(
            0,
            Part::Thinking(ThinkingBlock {
                text: reasoning.to_string(),
                ..ThinkingBlock::default()
            }),
        );
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = call.get("function");
        parts.push(Part::ToolCall {
            id: call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string(),
        });
    }

    let role = match role {
        "system" => Role::System,
        "developer" => Role::Developer,
        "assistant" => Role::Assistant,
        _ => Role::User,
    };
    messages.push(Message { role, parts });
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
        "text" | "input_text" | "output_text" => Some(Part::text(
            part.get("text").and_then(Value::as_str).unwrap_or_default(),
        )),
        "image_url" => {
            let image = part.get("image_url")?;
            Some(Part::Image {
                source: MediaSource::from_url(image.get("url").and_then(Value::as_str)?),
                detail: image
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        "file" => {
            let file = part.get("file")?;
            let data = file.get("file_data").and_then(Value::as_str)?;
            Some(Part::Document {
                source: MediaSource::from_url(data),
                name: file
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        "refusal" => Some(Part::Refusal(
            part.get("refusal")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        _ => None,
    }
}

fn parse_tool(tool: &Value) -> Result<Tool, String> {
    match tool.get("type").and_then(Value::as_str) {
        Some("function") | None => {}
        Some(other) => return Err(format!("OpenAI 内置工具 {other}")),
    }
    let function = tool
        .get("function")
        .ok_or_else(|| "缺少 function 的工具定义".to_string())?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "缺少名称的工具定义".to_string())?;
    Ok(Tool {
        name: name.to_string(),
        description: function
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        parameters: function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
        strict: function.get("strict").and_then(Value::as_bool),
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
            choice
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)?
                .to_string(),
        )),
        _ => None,
    }
}

fn parse_response_format(format: &Value) -> Option<OutputFormat> {
    match format.get("type").and_then(Value::as_str)? {
        "json_object" => Some(OutputFormat::JsonObject),
        "json_schema" => {
            let schema = format.get("json_schema")?;
            Some(OutputFormat::JsonSchema {
                name: schema
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("response")
                    .to_string(),
                description: schema
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                schema: schema.get("schema").cloned().unwrap_or(Value::Null),
                strict: schema
                    .get("strict")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        }
        _ => None,
    }
}

pub fn parse_response(body: &Value) -> Result<Response, Unsupported> {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| Unsupported::new("响应体缺少 choices"))?;

    let mut parts = Vec::new();
    if let Some(message) = choice.get("message") {
        let mut collected = Vec::new();
        parse_message(message, &mut collected);
        if let Some(first) = collected.into_iter().next() {
            parts = first.parts;
        }
    }
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
        stop: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(|reason| parse_finish(reason, &parts))
            .unwrap_or(StopReason::EndTurn),
        stop_sequence: None,
        usage: parse_usage(body.get("usage")),
        parts,
    })
}

fn parse_finish(reason: &str, parts: &[Part]) -> StopReason {
    match reason {
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::ContentFilter,
        // 兼容站点在工具调用时也常报 stop；有调用块就按工具轮次处理。
        _ if parts.iter().any(|p| matches!(p, Part::ToolCall { .. })) => StopReason::ToolUse,
        _ if parts.iter().any(|p| matches!(p, Part::Refusal(_))) => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

fn parse_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    Usage {
        input: field("prompt_tokens").or_else(|| field("input_tokens")),
        output: field("completion_tokens").or_else(|| field("output_tokens")),
        cache_read: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write: None,
        reasoning: usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64),
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
        // 网关自己必须拿到 usage：输出速度评分（默认权重 15）与 TPM 归还都
        // 依赖它。若只在客户端主动写了 stream_options.include_usage 时才向
        // 上游索取，那么绝大多数下游（SDK 默认都不写）在这两件事上永远是
        // 瞎的——吞吐维退化成常数，TPM 只能按预留保守占用到窗口过期。
        //
        // 下发形状不受影响：跨协议路径由 emitter 的 include_usage 决定要不要
        // 把这个收尾块发给客户端，同协议路径由响应侧过滤器决定（§9.3、§17.2）。
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }

    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        emit_message(message, &mut messages, &mut degraded)?;
    }
    body.insert("messages".into(), Value::Array(messages));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                let mut function = json!({
                    "name": tool.name,
                    "parameters": tool.parameters,
                });
                if let Some(object) = function.as_object_mut() {
                    if let Some(description) = &tool.description {
                        object.insert("description".into(), json!(description));
                    }
                    if let Some(strict) = tool.strict {
                        object.insert("strict".into(), json!(strict));
                    }
                }
                json!({"type": "function", "function": function})
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
                ToolChoice::Named(name) => {
                    json!({"type": "function", "function": {"name": name}})
                }
            },
        );
    }
    if let Some(parallel) = request.parallel_tool_calls {
        body.insert("parallel_tool_calls".into(), json!(parallel));
    }

    if let Some(format) = &request.output_format {
        body.insert("response_format".into(), emit_response_format(format));
    }
    if let Some(thinking) = request.thinking {
        // Chat 只有档位，没有预算；Anthropic 的 budget_tokens 按档位换算。
        let effort = if thinking.enabled {
            thinking.effort().as_str()
        } else {
            "none"
        };
        body.insert("reasoning_effort".into(), json!(effort));
    }
    if let Some(max) = request.max_tokens {
        body.insert("max_completion_tokens".into(), json!(max));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    if !request.stop.is_empty() {
        body.insert("stop".into(), json!(request.stop));
    }
    if let Some(user) = &request.user {
        body.insert("user".into(), json!(user));
    }
    for (field, value) in &request.sampling {
        match sampling_key(field, Protocol::OpenAiChat) {
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

/// 发射一条消息。工具结果必须拆成独立的 `role: "tool"` 条目。
fn emit_message(
    message: &Message,
    messages: &mut Vec<Value>,
    degraded: &mut Degradations,
) -> Result<(), Unsupported> {
    let role = match message.role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::Assistant => "assistant",
        Role::User => "user",
    };

    let mut content: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut refusal: Option<String> = None;

    for part in &message.parts {
        match part {
            Part::Text(text) if text.is_empty() => {}
            Part::Text(text) => content.push(json!({"type": "text", "text": text})),
            Part::Image { source, detail } => {
                let mut image = json!({"url": source.to_url()});
                if let Some(detail) = detail
                    && let Some(object) = image.as_object_mut()
                {
                    object.insert("detail".into(), json!(detail));
                }
                content.push(json!({"type": "image_url", "image_url": image}));
            }
            Part::Document { source, name } => content.push(json!({
                "type": "file",
                "file": {
                    "filename": name.clone().unwrap_or_else(|| "file".into()),
                    "file_data": source.to_url(),
                }
            })),
            Part::Refusal(text) => refusal = Some(text.clone()),
            Part::ToolCall {
                id,
                name,
                arguments,
            } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            })),
            // 工具结果是独立消息：先把已积累的内容收尾，再单独发一条。
            Part::ToolResult {
                call_id,
                content: result,
                ..
            } => {
                flush(messages, role, &mut content, &mut tool_calls, &mut refusal);
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": flatten_text(result),
                }));
            }
            // Chat 的历史里没有思考块的位置（§14.8 白名单内）。
            Part::Thinking(_) => degraded.drop("thinking"),
        }
    }
    flush(messages, role, &mut content, &mut tool_calls, &mut refusal);
    Ok(())
}

/// 把积累的内容写成一条消息。全空时不产生空消息。
fn flush(
    messages: &mut Vec<Value>,
    role: &str,
    content: &mut Vec<Value>,
    tool_calls: &mut Vec<Value>,
    refusal: &mut Option<String>,
) {
    if content.is_empty() && tool_calls.is_empty() && refusal.is_none() {
        return;
    }
    let mut message = Map::new();
    message.insert("role".into(), json!(role));
    // 只有一段纯文本时用字符串形式：兼容站点对数组形式的支持参差不齐。
    let text_only =
        content.len() == 1 && content[0].get("type").and_then(Value::as_str) == Some("text");
    if text_only {
        message.insert("content".into(), content[0]["text"].clone());
    } else if content.is_empty() {
        message.insert("content".into(), Value::Null);
    } else {
        message.insert("content".into(), Value::Array(std::mem::take(content)));
    }
    content.clear();
    if !tool_calls.is_empty() {
        message.insert(
            "tool_calls".into(),
            Value::Array(std::mem::take(tool_calls)),
        );
    }
    if let Some(text) = refusal.take() {
        message.insert("refusal".into(), json!(text));
    }
    messages.push(Value::Object(message));
}

/// 工具结果在 Chat 里只能是文本。图片等非文本块无法表达。
fn flatten_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            Part::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn emit_response_format(format: &OutputFormat) -> Value {
    match format {
        OutputFormat::JsonObject => json!({"type": "json_object"}),
        OutputFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        } => json!({
            "type": "json_schema",
            "json_schema": {
                "name": name,
                "description": description,
                "schema": schema,
                "strict": strict,
            }
        }),
    }
}

pub fn emit_response(response: &Response) -> Result<Value, Unsupported> {
    let mut degraded = Degradations::default();
    let mut messages = Vec::new();
    emit_message(
        &Message {
            role: Role::Assistant,
            parts: response.parts.clone(),
        },
        &mut messages,
        &mut degraded,
    )?;
    let mut message = messages
        .into_iter()
        .next()
        .unwrap_or_else(|| json!({"role": "assistant", "content": Value::Null}));
    // 思考文本放进 reasoning_content：这是兼容站点的既成惯例，比丢掉更有用。
    if let Some(reasoning) = reasoning_text(&response.parts)
        && let Some(object) = message.as_object_mut()
    {
        object.insert("reasoning_content".into(), json!(reasoning));
    }

    Ok(json!({
        "id": response.id,
        "object": "chat.completion",
        "created": 0,
        "model": response.model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": emit_finish(response.stop),
        }],
        "usage": emit_usage(&response.usage),
    }))
}

fn reasoning_text(parts: &[Part]) -> Option<String> {
    let text: Vec<String> = parts
        .iter()
        .filter_map(|part| match part {
            Part::Thinking(thinking) if !thinking.text.is_empty() => Some(thinking.text.clone()),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

fn emit_finish(stop: StopReason) -> &'static str {
    match stop {
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::ContentFilter => "content_filter",
        // Chat 没有 refusal 这个 finish_reason；拒绝内容在 message.refusal 里。
        StopReason::EndTurn | StopReason::StopSequence | StopReason::Refusal => "stop",
    }
}

fn emit_usage(usage: &Usage) -> Value {
    let mut object = Map::new();
    if let Some(input) = usage.input {
        object.insert("prompt_tokens".into(), json!(input));
    }
    if let Some(output) = usage.output {
        object.insert("completion_tokens".into(), json!(output));
    }
    if let (Some(input), Some(output)) = (usage.input, usage.output) {
        object.insert("total_tokens".into(), json!(input + output));
    }
    if let Some(read) = usage.cache_read {
        object.insert(
            "prompt_tokens_details".into(),
            json!({"cached_tokens": read}),
        );
    }
    let mut details = Map::new();
    if let Some(reasoning) = usage.reasoning {
        details.insert("reasoning_tokens".into(), json!(reasoning));
    }
    // Anthropic 的缓存写入在 Chat 侧没有标准字段，放进扩展区而不是丢弃。
    if let Some(write) = usage.cache_write {
        object.insert("akhub_cache_creation_tokens".into(), json!(write));
    }
    if !details.is_empty() {
        object.insert("completion_tokens_details".into(), Value::Object(details));
    }
    Value::Object(object)
}

// ------------------------------------------------------------ 流式解析

/// Chat 的流式解析需要状态：`index` 是 `tool_calls` 数组下标，而中间格式的
/// `index` 是内容项序号，两者不同；同一个工具调用的名字只在第一帧出现。
#[derive(Debug, Default)]
pub struct StreamParser {
    started: bool,
    /// 当前文本内容项是否已经开过。
    text_open: bool,
    /// tool_calls 下标 → 中间格式的内容项序号。
    tools: Vec<(u64, usize)>,
    next_index: usize,
    finished: bool,
}

impl StreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 解析一帧，返回零到多个中间事件。
    pub fn push(&mut self, data: &Value) -> Vec<Event> {
        if data.get("error").is_some() {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(Event::Start {
                id: data
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                model: data
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }

        let usage = parse_usage(data.get("usage"));
        let choice = data
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());

        if let Some(choice) = choice {
            let delta = choice.get("delta");
            if let Some(text) = delta
                .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let index = self.open_thinking(&mut events);
                events.push(Event::ThinkingDelta {
                    index,
                    text: text.to_string(),
                });
            }
            if let Some(text) = delta
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let index = self.open_text(&mut events);
                events.push(Event::TextDelta {
                    index,
                    text: text.to_string(),
                });
            }
            for call in delta
                .and_then(|d| d.get("tool_calls"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.push_tool_call(call, &mut events);
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
            {
                self.close_all(&mut events);
                if !usage.is_empty() {
                    events.push(Event::Usage(usage));
                }
                self.finished = true;
                events.push(Event::Finish {
                    stop: parse_finish(reason, &[]),
                    stop_sequence: None,
                });
                return events;
            }
        }

        // `include_usage` 的收尾块没有 choices，只带 usage。
        if !usage.is_empty() {
            events.push(Event::Usage(usage));
        }
        events
    }

    /// 流结束（`[DONE]` 或连接正常关闭）。
    pub fn finish(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if !self.started {
            return events;
        }
        if !self.finished {
            self.close_all(&mut events);
            events.push(Event::Finish {
                stop: StopReason::EndTurn,
                stop_sequence: None,
            });
        }
        events.push(Event::Done);
        events
    }

    fn open_text(&mut self, events: &mut Vec<Event>) -> usize {
        if self.text_open {
            return self.next_index - 1;
        }
        let index = self.allocate();
        self.text_open = true;
        events.push(Event::ItemStart {
            index,
            kind: ItemKind::Text,
        });
        index
    }

    fn open_thinking(&mut self, events: &mut Vec<Event>) -> usize {
        // 思考在文本之前；已经开了文本项就不再回头开思考项。
        if self.text_open {
            return self.next_index - 1;
        }
        let index = self.allocate();
        self.text_open = true;
        events.push(Event::ItemStart {
            index,
            kind: ItemKind::Thinking,
        });
        index
    }

    fn push_tool_call(&mut self, call: &Value, events: &mut Vec<Event>) {
        let slot = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let function = call.get("function");
        let index = match self.tools.iter().find(|(s, _)| *s == slot) {
            Some((_, index)) => *index,
            None => {
                let index = self.allocate();
                self.tools.push((slot, index));
                events.push(Event::ItemStart {
                    index,
                    kind: ItemKind::ToolCall {
                        id: call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: function
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                });
                index
            }
        };
        if let Some(fragment) = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .filter(|fragment| !fragment.is_empty())
        {
            events.push(Event::ToolArgsDelta {
                index,
                fragment: fragment.to_string(),
            });
        }
    }

    fn allocate(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn close_all(&mut self, events: &mut Vec<Event>) {
        let mut open: Vec<usize> = self.tools.iter().map(|(_, index)| *index).collect();
        if self.text_open {
            // 文本项的序号是第一个被分配的。
            open.push(0);
        }
        open.sort_unstable();
        for index in open {
            events.push(Event::ItemEnd { index });
        }
        self.text_open = false;
        self.tools.clear();
    }
}

// ------------------------------------------------------------ 流式发射

/// 把中间事件发射成 Chat 的 `chat.completion.chunk` 帧。
#[derive(Debug, Default)]
pub struct StreamEmitter {
    id: String,
    model: String,
    /// 中间格式的内容项序号 → Chat 的 tool_calls 下标。
    tools: Vec<(usize, u64)>,
    usage: Usage,
    include_usage: bool,
}

impl StreamEmitter {
    pub fn new(include_usage: bool) -> Self {
        Self {
            include_usage,
            ..Self::default()
        }
    }

    pub fn push(&mut self, event: &Event) -> Vec<Value> {
        match event {
            Event::Start { id, model } => {
                self.id = id.clone();
                self.model = model.clone();
                // OpenAI 的首块只声明角色，不带内容。
                vec![self.chunk(json!({"role": "assistant", "content": ""}), None)]
            }
            Event::ItemStart { index, kind } => match kind {
                ItemKind::ToolCall { id, name } => {
                    let slot = self.tools.len() as u64;
                    self.tools.push((*index, slot));
                    vec![self.chunk(
                        json!({
                            "tool_calls": [{
                                "index": slot,
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": ""},
                            }]
                        }),
                        None,
                    )]
                }
                // 文本与思考项的开始在 Chat 里没有对应帧。
                _ => Vec::new(),
            },
            Event::TextDelta { text, .. } => {
                vec![self.chunk(json!({"content": text}), None)]
            }
            Event::ThinkingDelta { text, .. } => {
                vec![self.chunk(json!({"reasoning_content": text}), None)]
            }
            Event::ToolArgsDelta { index, fragment } => {
                let slot = self
                    .tools
                    .iter()
                    .find(|(item, _)| item == index)
                    .map(|(_, slot)| *slot)
                    .unwrap_or(0);
                vec![self.chunk(
                    json!({
                        "tool_calls": [{
                            "index": slot,
                            "function": {"arguments": fragment},
                        }]
                    }),
                    None,
                )]
            }
            Event::ItemEnd { .. } => Vec::new(),
            Event::Usage(usage) => {
                self.usage.merge(*usage);
                Vec::new()
            }
            Event::Finish { stop, .. } => {
                vec![self.chunk(json!({}), Some(emit_finish(*stop)))]
            }
            Event::Done => {
                // usage 块必须在 finish 之后单独发一帧（OpenAI 的约定）。
                if self.include_usage && !self.usage.is_empty() {
                    let mut chunk = json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "created": 0,
                        "model": self.model,
                        "choices": [],
                    });
                    if let Some(object) = chunk.as_object_mut() {
                        object.insert("usage".into(), emit_usage(&self.usage));
                    }
                    return vec![chunk];
                }
                Vec::new()
            }
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish,
            }],
        })
    }
}

/// 流中途出错时的错误帧（§18.2）。
///
/// 带上稳定网关错误码与请求 ID：客户端拿到之后能区分"上游坏了"和"被限流"，
/// 也能在报问题时给出可检索的请求 ID。
pub fn error_event(
    code: crate::gateway::error::ErrorCode,
    message: &str,
    request_id: Option<&str>,
) -> Value {
    json!({"error": {
        "message": message,
        "type": code.openai_type_for_stream(),
        "code": code.as_str(),
        "request_id": request_id,
    }})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_messages_fold_into_the_user_turn_and_unfold_again() {
        let body = json!({
            "model": "glm-4.6",
            "messages": [
                {"role": "system", "content": "你是助手"},
                {"role": "user", "content": "天气"},
                {"role": "assistant", "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "weather", "arguments": "{\"city\":\"北京\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "晴"}
            ]
        });
        let request = parse_request(&body).unwrap();
        assert_eq!(request.messages.len(), 4);
        // 工具结果在中间格式里是用户轮次的一个块。
        assert!(matches!(
            request.messages[3].parts[0],
            Part::ToolResult { .. }
        ));

        let emitted = emit_request(&request).unwrap();
        let messages = emitted.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"北京\"}"
        );
    }

    #[test]
    fn images_survive_both_directions() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA", "detail": "high"}}
            ]}]
        });
        let request = parse_request(&body).unwrap();
        match &request.messages[0].parts[1] {
            Part::Image { source, detail } => {
                assert_eq!(detail.as_deref(), Some("high"));
                assert!(matches!(source, MediaSource::Base64 { .. }));
            }
            other => panic!("期望图片，得到 {other:?}"),
        }
        let emitted = emit_request(&request).unwrap();
        assert_eq!(
            emitted.body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn json_schema_is_preserved_exactly() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let body = json!({
            "model": "m",
            "messages": [],
            "response_format": {"type": "json_schema", "json_schema": {
                "name": "out", "strict": true, "schema": schema
            }}
        });
        let request = parse_request(&body).unwrap();
        let emitted = emit_request(&request).unwrap();
        assert_eq!(
            emitted.body["response_format"]["json_schema"]["schema"],
            schema
        );
        assert_eq!(
            emitted.body["response_format"]["json_schema"]["strict"],
            true
        );
    }

    #[test]
    fn n_greater_than_one_is_inexpressible() {
        // 多候选无法在 Messages 或 Responses 表达，也不在降级白名单内。
        let body = json!({"model": "m", "messages": [], "n": 3});
        let request = parse_request(&body).unwrap();
        assert!(request.reject_inexpressible().is_err());
    }

    #[test]
    fn thinking_history_is_dropped_but_effort_is_kept() {
        let mut request = Request::new(Protocol::AnthropicMessages, "m");
        request.thinking = Some(ThinkingConfig {
            enabled: true,
            budget_tokens: Some(10_000),
            effort: None,
        });
        request.messages.push(Message {
            role: Role::Assistant,
            parts: vec![
                Part::Thinking(ThinkingBlock {
                    text: "推理".into(),
                    signature: Some("sig".into()),
                    ..ThinkingBlock::default()
                }),
                Part::text("答案"),
            ],
        });
        let emitted = emit_request(&request).unwrap();
        // 历史里的思考块丢弃并标记，但"要思考"这个意图必须传下去。
        assert_eq!(emitted.degraded, vec!["thinking".to_string()]);
        assert_eq!(emitted.body["reasoning_effort"], "high");
        assert_eq!(emitted.body["messages"][0]["content"], "答案");
    }

    #[test]
    fn usage_details_survive_the_round_trip() {
        let usage = parse_usage(Some(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 40},
            "completion_tokens_details": {"reasoning_tokens": 8},
        })));
        assert_eq!(usage.input, Some(100));
        assert_eq!(usage.cache_read, Some(40));
        assert_eq!(usage.reasoning, Some(8));
        let emitted = emit_usage(&usage);
        assert_eq!(emitted["total_tokens"], 120);
        assert_eq!(emitted["completion_tokens_details"]["reasoning_tokens"], 8);
    }

    #[test]
    fn streaming_tool_calls_are_reassembled_by_slot() {
        let mut parser = StreamParser::new();
        let mut events = parser.push(&json!({
            "id": "chatcmpl-1", "model": "m",
            "choices": [{"delta": {"role": "assistant", "content": ""}}]
        }));
        events.extend(parser.push(&json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "f", "arguments": "{\"a"}}
            ]}}]
        })));
        events.extend(parser.push(&json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "\":1}"}}
            ]}}]
        })));
        events.extend(parser.push(&json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        })));
        events.extend(parser.finish());

        assert!(matches!(events[0], Event::Start { .. }));
        assert!(matches!(
            &events[1],
            Event::ItemStart { kind: ItemKind::ToolCall { id, name }, .. }
                if id == "call_1" && name == "f"
        ));
        let fragments: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                Event::ToolArgsDelta { fragment, .. } => Some(fragment.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(fragments.concat(), "{\"a\":1}");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Finish {
                stop: StopReason::ToolUse,
                ..
            }
        )));
        assert_eq!(events.last(), Some(&Event::Done));
    }

    #[test]
    fn a_stream_that_ends_without_finish_still_gets_one() {
        // 兼容站点常常直接断流；下游的状态机不能因此悬空。
        let mut parser = StreamParser::new();
        parser.push(&json!({"id": "1", "model": "m", "choices": [{"delta": {"content": "嗨"}}]}));
        let events = parser.finish();
        assert!(events.iter().any(|e| matches!(e, Event::ItemEnd { .. })));
        assert!(events.iter().any(|e| matches!(e, Event::Finish { .. })));
    }

    #[test]
    fn emitted_stream_puts_usage_after_finish() {
        let mut emitter = StreamEmitter::new(true);
        emitter.push(&Event::Start {
            id: "1".into(),
            model: "m".into(),
        });
        emitter.push(&Event::Usage(Usage {
            input: Some(10),
            output: Some(2),
            ..Usage::default()
        }));
        let finish = emitter.push(&Event::Finish {
            stop: StopReason::EndTurn,
            stop_sequence: None,
        });
        assert_eq!(finish[0]["choices"][0]["finish_reason"], "stop");
        let done = emitter.push(&Event::Done);
        assert_eq!(done[0]["usage"]["total_tokens"], 12);
        assert_eq!(done[0]["choices"].as_array().unwrap().len(), 0);
    }
}
