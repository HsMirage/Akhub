//! 完整中间格式（§14.4、§14.5）。
//!
//! 这不是"三个协议的公共最小字段"，而是三个协议的**并集**：角色语义、有序
//! 内容块、工具定义与调用、结构化输出、思考配置与思考块、生成参数、细分
//! usage，外加两块"不能表达就必须报错"的记录区——未识别字段与供应商专有
//! 能力。同协议透传永远不经过这里；只有跨协议转换才需要它。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::domain::Protocol;

/// 消息角色。工具结果不是独立角色：它是用户轮次里的一个内容块（Anthropic
/// 的形状），Chat 与 Responses 的发射器会把它拆回各自的独立条目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

/// 图片或文档的来源。绝不为转换主动下载外部 URL（§14.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaSource {
    Base64 { media_type: String, data: String },
    Url(String),
}

impl MediaSource {
    /// 解析 `data:` URL；其余一律视为外部 URL。
    pub fn from_url(url: &str) -> Self {
        if let Some(rest) = url.strip_prefix("data:")
            && let Some((header, data)) = rest.split_once(',')
            && let Some(media_type) = header.strip_suffix(";base64")
        {
            return Self::Base64 {
                media_type: media_type.to_string(),
                data: data.to_string(),
            };
        }
        Self::Url(url.to_string())
    }

    /// 输出为 `data:` URL 或原始 URL。
    pub fn to_url(&self) -> String {
        match self {
            Self::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
            Self::Url(url) => url.clone(),
        }
    }
}

/// 思考块：可见文本、签名、加密内容与来源协议的条目 ID。
///
/// 签名与加密内容只对产生它们的供应商有意义，跨协议一律无法沿用——这正是
/// 思考被列入降级白名单的原因（§14.8）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThinkingBlock {
    pub text: String,
    pub signature: Option<String>,
    pub encrypted: Option<String>,
    pub id: Option<String>,
    pub redacted: bool,
}

/// 有顺序的内容块。
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Image {
        source: MediaSource,
        detail: Option<String>,
    },
    Document {
        source: MediaSource,
        name: Option<String>,
    },
    /// 工具调用。参数保存为 JSON 文本：Chat 与 Responses 的原生形状就是文本，
    /// 只有 Anthropic 需要解析成对象。
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        call_id: String,
        content: Vec<Part>,
        is_error: bool,
    },
    Thinking(ThinkingBlock),
    Refusal(String),
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    /// 思考预算的近似换算：档位 → Token 数。
    pub fn budget_tokens(self) -> u32 {
        match self {
            Self::Minimal => 1_024,
            Self::Low => 2_048,
            Self::Medium => 8_192,
            Self::High => 16_384,
        }
    }

    /// Token 数 → 档位。
    pub fn from_budget(budget: u32) -> Self {
        if budget <= 2_048 {
            Self::Low
        } else if budget <= 8_192 {
            Self::Medium
        } else {
            Self::High
        }
    }
}

/// 思考配置。`enabled = false` 表示客户端显式关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingConfig {
    pub enabled: bool,
    pub budget_tokens: Option<u32>,
    pub effort: Option<Effort>,
}

impl ThinkingConfig {
    pub fn effort(&self) -> Effort {
        self.effort
            .or_else(|| self.budget_tokens.map(Effort::from_budget))
            .unwrap_or(Effort::Medium)
    }

    pub fn budget(&self) -> u32 {
        self.budget_tokens
            .unwrap_or_else(|| self.effort.unwrap_or(Effort::Medium).budget_tokens())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputFormat {
    JsonObject,
    JsonSchema {
        name: String,
        description: Option<String>,
        schema: Value,
        strict: bool,
    },
}

/// 一次推理请求的中间表示。
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub origin: Protocol,
    pub model: String,
    pub stream: bool,
    /// Chat 的 `stream_options.include_usage`；其余协议流式响应天然带 usage。
    pub include_usage: bool,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub tool_choice: Option<ToolChoice>,
    pub parallel_tool_calls: Option<bool>,
    pub output_format: Option<OutputFormat>,
    pub thinking: Option<ThinkingConfig>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    /// 协议独有的采样参数（`top_k`、`seed`、`frequency_penalty`……）。目标协议
    /// 能表达就原样带过去，不能表达时按白名单降级并逐项标记（§14.8）。
    pub sampling: BTreeMap<String, Value>,
    pub user: Option<String>,
    /// 任何其他协议都表达不了的能力（供应商内置工具、MCP、`n > 1`……）。
    /// 跨协议时整个请求对该目标不合格（§14.6 "供应商专有工具"）。
    pub inexpressible: Vec<String>,
    /// 未识别的顶层字段。同协议原样透传，跨协议明确拒绝（§14.6）。
    pub unknown: Vec<String>,
}

impl Request {
    pub fn new(origin: Protocol, model: impl Into<String>) -> Self {
        Self {
            origin,
            model: model.into(),
            stream: false,
            include_usage: false,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            output_format: None,
            thinking: None,
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: Vec::new(),
            sampling: BTreeMap::new(),
            user: None,
            inexpressible: Vec::new(),
            unknown: Vec::new(),
        }
    }

    /// 跨协议发射前的统一门槛：未识别字段与专有能力都不能静默丢弃（§14.6）。
    pub fn reject_inexpressible(&self) -> Result<(), crate::protocol::degrade::Unsupported> {
        if let Some(field) = self.unknown.first() {
            return Err(crate::protocol::degrade::Unsupported::new(format!(
                "字段 {field} 无法跨协议表达"
            )));
        }
        if let Some(feature) = self.inexpressible.first() {
            return Err(crate::protocol::degrade::Unsupported::new(format!(
                "{feature} 无法跨协议表达"
            )));
        }
        Ok(())
    }

    /// 历史里是否带有思考块（跨协议必然丢弃，构成降级）。
    pub fn has_thinking_history(&self) -> bool {
        self.messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .any(|p| matches!(p, Part::Thinking(_)))
    }

    /// 最后一条助手消息是否包含工具调用。
    ///
    /// Anthropic 开启思考时要求这样的助手轮次以带签名的思考块开头；跨协议
    /// 没有签名可用，只能在这种历史下关闭思考。
    pub fn last_assistant_has_tool_calls(&self) -> bool {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .is_some_and(|m| m.parts.iter().any(|p| matches!(p, Part::ToolCall { .. })))
    }

    /// 是否要求启用思考。
    pub fn wants_thinking(&self) -> bool {
        self.thinking.is_some_and(|t| t.enabled)
    }

    /// 本次请求用到的可缓存能力，按"证据查询用的能力名"给出（§16.7）。
    ///
    /// 调度前用它逐个目标查询能力限制：已被证实不支持的项按 §9.1 处理。
    /// 图片与文档输入共享 `vision` 一词——上游拒绝时也分不清两者。
    pub fn requested_capabilities(&self) -> Vec<&'static str> {
        let mut caps = Vec::new();
        let mut push = |name: &'static str| {
            if !caps.contains(&name) {
                caps.push(name);
            }
        };
        if !self.tools.is_empty() {
            push("function_calling");
        }
        if self.output_format.is_some() {
            push("response_schema");
        }
        if self
            .messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .any(|p| matches!(p, Part::Image { .. } | Part::Document { .. }))
        {
            push("vision");
        }
        if self.wants_thinking() || self.has_thinking_history() {
            push("reasoning");
        }
        caps
    }
}

/// 停止原因，映射到下游协议最接近且稳定的取值（§14.6）。
///
/// 默认值是 `EndTurn`：上游没说为什么停时，"这一轮说完了"是唯一不会误导
/// 客户端的答案。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopReason {
    #[default]
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    ContentFilter,
    Refusal,
}

/// 细分 usage。`input` 是**全部**输入 Token（含缓存命中与缓存写入），这是
/// OpenAI 的口径；Anthropic 的 `input_tokens` 不含缓存部分，转换时按此换算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

impl Usage {
    /// 用后到的数值覆盖已知数值；缺失的字段保持原样，绝不伪造（§14.6）。
    pub fn merge(&mut self, other: Usage) {
        self.input = other.input.or(self.input);
        self.output = other.output.or(self.output);
        self.cache_read = other.cache_read.or(self.cache_read);
        self.cache_write = other.cache_write.or(self.cache_write);
        self.reasoning = other.reasoning.or(self.reasoning);
    }

    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }

    /// Anthropic 口径的输入 Token：去掉缓存读写部分。
    pub fn uncached_input(&self) -> Option<u64> {
        self.input.map(|input| {
            input
                .saturating_sub(self.cache_read.unwrap_or(0))
                .saturating_sub(self.cache_write.unwrap_or(0))
        })
    }
}

/// 一次非流式响应的中间表示。
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub parts: Vec<Part>,
    pub stop: StopReason,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

impl Response {
    pub fn has_tool_calls(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, Part::ToolCall { .. }))
    }
}

/// 流式内容项的种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    Text,
    Thinking,
    ToolCall { id: String, name: String },
    Refusal,
}

/// 独立的流式事件状态机（§14.5）。
///
/// 适配器必须维护事件顺序、内容索引、工具调用 ID、stop reason 与最终 usage，
/// 不能用字符串替换 SSE。`Usage` 若出现，保证先于 `Finish`。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Start {
        id: String,
        model: String,
    },
    ItemStart {
        index: usize,
        kind: ItemKind,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    ToolArgsDelta {
        index: usize,
        fragment: String,
    },
    ItemEnd {
        index: usize,
    },
    Usage(Usage),
    Finish {
        stop: StopReason,
        stop_sequence: Option<String>,
    },
    /// 流正常结束。没有 `Finish` 就结束的流属于损坏响应。
    Done,
}

impl Event {
    /// 是否是"有语义"的事件：一旦发出就禁止拼接第二个上游（§13.4）。
    pub fn is_semantic(&self) -> bool {
        matches!(
            self,
            Self::ItemStart { .. }
                | Self::TextDelta { .. }
                | Self::ThinkingDelta { .. }
                | Self::ToolArgsDelta { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls_round_trip_through_media_source() {
        let source = MediaSource::from_url("data:image/png;base64,AAAA");
        assert_eq!(
            source,
            MediaSource::Base64 {
                media_type: "image/png".into(),
                data: "AAAA".into()
            }
        );
        assert_eq!(source.to_url(), "data:image/png;base64,AAAA");

        let external = MediaSource::from_url("https://example.com/a.png");
        assert_eq!(
            external,
            MediaSource::Url("https://example.com/a.png".into())
        );
    }

    #[test]
    fn usage_merge_never_fabricates_missing_numbers() {
        let mut usage = Usage {
            input: Some(100),
            cache_read: Some(40),
            ..Usage::default()
        };
        usage.merge(Usage {
            output: Some(7),
            ..Usage::default()
        });
        assert_eq!(usage.input, Some(100));
        assert_eq!(usage.output, Some(7));
        assert_eq!(usage.cache_write, None);
        // Anthropic 口径的输入不含缓存命中。
        assert_eq!(usage.uncached_input(), Some(60));
    }

    #[test]
    fn effort_and_budget_convert_both_ways() {
        assert_eq!(Effort::from_budget(1_024), Effort::Low);
        assert_eq!(Effort::from_budget(8_192), Effort::Medium);
        assert_eq!(Effort::from_budget(30_000), Effort::High);
        assert_eq!(Effort::High.budget_tokens(), 16_384);
        assert_eq!(Effort::parse("medium"), Some(Effort::Medium));
        assert_eq!(Effort::parse("极高"), None);
    }
}
