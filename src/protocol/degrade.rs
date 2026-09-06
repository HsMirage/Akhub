//! 能力降级白名单与标记（§14.8）。
//!
//! 判断标准只有一条：**丢掉这个能力，客户端还能不能正常工作？**
//!
//! | 能力 | 丢了会怎样 | 结论 |
//! |---|---|---|
//! | 工具定义 / 调用 / 结果 | agent 完全瘫痪 | 不可降级，不能表达就报错 |
//! | 结构化输出 Schema | 客户端解析崩溃 | 不可降级 |
//! | 图片 / 文件输入 | 模型看不到内容，答案错但不报错 | 不可降级 |
//! | 角色语义 | 对话结构被破坏 | 不可降级 |
//! | thinking / reasoning 块 | 模型正常回答，只是没有思考过程 | 可降级 |
//! | 协议独有采样参数（top_k 等） | 生成分布略变 | 可降级 |
//!
//! 降级发生时响应带 `X-Akhub-Degraded` 头，请求记录标红，响应体本身不做任何
//! 掩盖。

use std::fmt;

use serde_json::Value;

/// 跨协议无法表达且不在白名单内的能力。整个请求对该目标不合格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported(pub String);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Unsupported {
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// 一次发射的结果：目标协议的请求体，以及为此丢弃的白名单能力。
#[derive(Debug, Clone, PartialEq)]
pub struct Emitted {
    pub body: Value,
    /// 被降级的能力名，按 `X-Akhub-Degraded` 头的口径：`thinking`、`top_k`……
    pub degraded: Vec<String>,
    /// 用"强制工具调用"表达结构化输出时的合成工具名，响应侧据此还原为文本。
    pub structured_tool: Option<String>,
}

impl Emitted {
    pub fn lossless(body: Value) -> Self {
        Self {
            body,
            degraded: Vec::new(),
            structured_tool: None,
        }
    }

    pub fn is_lossless(&self) -> bool {
        self.degraded.is_empty()
    }
}

/// 发射过程中累积降级项的记录器。
#[derive(Debug, Default)]
pub struct Degradations(Vec<String>);

impl Degradations {
    /// 记录一次白名单内的丢弃。重复的能力只记一次。
    pub fn drop(&mut self, capability: &str) {
        if !self.0.iter().any(|c| c == capability) {
            self.0.push(capability.to_string());
        }
    }

    pub fn into_list(self) -> Vec<String> {
        self.0
    }
}

/// 一个目标对当前请求的保真度，决定它在层内的排序（§14.8）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fidelity {
    /// 无损：原生端点，或转换后没有丢弃任何东西。
    Lossless,
    /// 只有丢弃白名单内的能力才能表达。
    Degraded(Vec<String>),
}

/// `X-Akhub-Degraded` 头的值。
pub fn header_value(degraded: &[String]) -> String {
    degraded.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degradations_are_deduplicated_and_ordered() {
        let mut degraded = Degradations::default();
        degraded.drop("thinking");
        degraded.drop("top_k");
        degraded.drop("thinking");
        assert_eq!(header_value(&degraded.into_list()), "thinking,top_k");
    }
}
