//! 完整中间格式（§14.4、§14.5）。
//!
//! 这不是"三个协议的公共最小字段"，而是三个协议的**并集**：角色语义、有序
//! 内容块、工具定义与调用、结构化输出、思考配置与思考块、生成参数、细分
//! usage，外加两块"不能表达就必须报错"的记录区——未识别字段与供应商专有
//! 能力。同协议透传不经过这里；只有跨协议转换才需要它。

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

/// 思考块：可见文本，加上只有产生它的供应商才认的不透明载荷。
///
/// 签名、`redacted_thinking.data` 与 `encrypted_content` 都是供应商签过名或
/// 加过密的东西，送到另一家只会换回一个 400（v1.1.40 的跨渠道签名 400 就是
/// 这一类）。所以载荷带着来源协议，发射器只把它还给同一个协议；其余情况只
/// 丢载荷、保留可见文本，并记一次 `thinking` 降级（§14.8）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThinkingBlock {
    pub text: String,
    pub opaque: Option<Opaque>,
}

/// 思考块的不透明载荷。每种载荷只属于一个协议，见 [`Opaque::origin`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opaque {
    /// Anthropic `thinking.signature`。
    Signature(String),
    /// Anthropic `redacted_thinking.data`：整块思考都被加密，没有可见文本。
    Redacted(String),
    /// Responses `reasoning.encrypted_content` 与它的条目 ID。
    Encrypted { id: Option<String>, content: String },
}

impl Opaque {
    /// 产生这份载荷的协议。只有它能原样收回去。
    pub fn origin(&self) -> Protocol {
        match self {
            Self::Signature(_) | Self::Redacted(_) => Protocol::AnthropicMessages,
            Self::Encrypted { .. } => Protocol::OpenAiResponses,
        }
    }
}

impl ThinkingBlock {
    /// 发往 `target` 时还能沿用的载荷；来源不同就是 `None`。
    pub fn opaque_for(&self, target: Protocol) -> Option<&Opaque> {
        self.opaque
            .as_ref()
            .filter(|opaque| opaque.origin() == target)
    }
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
    /// 4.7 起在 high 与 max 之间新增的一档（OpenAI 两个协议也认）。
    XHigh,
    /// 自适应契约的最高一档。
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// OpenAI 两个协议的档位：minimal/low/medium/high/xhigh 原样，只有 Anthropic
    /// 独有的 `max` 没有对应档位，折到最接近的 xhigh。第二个值表示是否发生了
    /// 折叠——折叠就是降级，调用方必须把它记进 `X-Akhub-Degraded`（§14.8）。
    pub fn to_openai(self) -> (&'static str, bool) {
        match self {
            Self::Max => (Self::XHigh.as_str(), true),
            other => (other.as_str(), false),
        }
    }

    /// Anthropic 的档位：它没有 `minimal`，往上取最近的 `low`。这是升档而不是
    /// 削弱，不算降级。
    pub fn to_anthropic(self) -> Self {
        match self {
            Self::Minimal => Self::Low,
            other => other,
        }
    }

    /// 思考预算的近似换算：档位 → Token 数（只对手工契约的模型有意义）。
    pub fn budget_tokens(self) -> u32 {
        match self {
            Self::Minimal => 1_024,
            Self::Low => 2_048,
            Self::Medium => 8_192,
            Self::High => 16_384,
            // 手工契约的最后一代（4.5 系）thinking 上限就是 32k；再往上只会把
            // max_tokens 顶穿，所以 xhigh 与 max 都封在这里。
            Self::XHigh | Self::Max => 32_000,
        }
    }

    /// 比这一档更高的最弱档；已经是最高档时给 `None`。
    ///
    /// 上游拒了客户端要的档位时用它往上试探：中转站实现的档位并不单调
    /// （现场就是一个站收 low 却拒 medium），所以"更低的不行"推不出"更高的
    /// 也不行"，只能自己试一次（§14.6）。
    pub fn above(self) -> Option<Self> {
        match self {
            Self::Minimal => Some(Self::Low),
            Self::Low => Some(Self::Medium),
            Self::Medium => Some(Self::High),
            Self::High => Some(Self::XHigh),
            Self::XHigh => Some(Self::Max),
            Self::Max => None,
        }
    }

    /// Token 数 → 档位。
    pub fn from_budget(budget: u32) -> Self {
        if budget <= 2_048 {
            Self::Low
        } else if budget <= 8_192 {
            Self::Medium
        } else if budget <= 16_384 {
            Self::High
        } else {
            Self::XHigh
        }
    }
}

/// 思考文本的展示方式。4.7 起默认 `omitted`（只回签名与加密内容），想让客户端
/// 看见推理过程必须显式要 `summarized`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
}

impl ThinkingDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "summarized" => Some(Self::Summarized),
            "omitted" => Some(Self::Omitted),
            _ => None,
        }
    }
}

/// 思考配置。`enabled = false` 表示客户端显式关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingConfig {
    pub enabled: bool,
    pub budget_tokens: Option<u32>,
    pub effort: Option<Effort>,
    /// 客户端显式要求的展示方式；`None` 表示它没表态。
    pub display: Option<ThinkingDisplay>,
}

impl ThinkingConfig {
    /// 客户端**显式**表达的档位。`None` 表示"没说要多少"。
    ///
    /// 自适应契约下这两者必须分开：模型自己的默认档位通常比中间格式猜的那个
    /// 高，替客户端选一档等于悄悄削弱它要的推理深度。
    pub fn explicit_effort(&self) -> Option<Effort> {
        self.effort
            .or_else(|| self.budget_tokens.map(Effort::from_budget))
    }

    pub fn effort(&self) -> Effort {
        self.explicit_effort().unwrap_or(Effort::Medium)
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
    /// OpenAI 的 `prompt_cache_key`。只是缓存路由提示，不改变输出语义：两个
    /// OpenAI 协议之间原样带过去，Anthropic 没有对应字段时直接不发。
    pub prompt_cache_key: Option<String>,
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
            prompt_cache_key: None,
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

    /// 本次请求使用的能力，只供内置目录参与候选排序（§16.6）。
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
        assert_eq!(Effort::from_budget(30_000), Effort::XHigh);
        assert_eq!(Effort::High.budget_tokens(), 16_384);
        assert_eq!(Effort::parse("medium"), Some(Effort::Medium));
        assert_eq!(Effort::parse("极高"), None);
        // 自适应契约新增的两档只在 Anthropic 侧成立。
        assert_eq!(Effort::parse("xhigh"), Some(Effort::XHigh));
        assert_eq!(Effort::parse("max"), Some(Effort::Max));
        assert_eq!(Effort::Max.as_str(), "max");
        assert_eq!(Effort::Max.to_openai(), ("xhigh", true));
        assert_eq!(Effort::XHigh.to_openai(), ("xhigh", false));
        assert_eq!(Effort::Low.to_openai(), ("low", false));
        assert_eq!(Effort::Minimal.to_anthropic(), Effort::Low);
        assert_eq!(
            ThinkingDisplay::parse("summarized"),
            Some(ThinkingDisplay::Summarized)
        );
        assert_eq!(ThinkingDisplay::Omitted.as_str(), "omitted");
    }
}
