//! 模型能力目录（§16.6）：只为候选排序提供提示，不从请求错误生成调度禁令。
//!
//! 参数错误受请求内容、上游端点与凭据影响，不能推断整个账号模型不可用。
//! 每次请求都交由上游实际处理，恢复后下一次调用立即生效。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 随二进制发布的能力目录快照（§16.6）。构建期嵌入，运行期只读。
static CATALOG: &str = include_str!("../../assets/capabilities.json");

/// 内置目录里描述一个模型能力的字段。
///
/// 只保留 Akhub 调度真正用得上的判断：能不能带工具、能不能吐 Schema、
/// 能不能看图、上下文有多长。价格数据从源头就被裁掉，Akhub 不维护价格表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCapability {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub function_calling: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parallel_function_calling: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub response_schema: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vision: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pdf_input: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prompt_caching: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system_messages: bool,
    pub max_input_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

/// 内置目录的查询入口。
pub fn builtin() -> &'static Catalog {
    use std::sync::OnceLock;
    static CATALOG_CACHE: OnceLock<Catalog> = OnceLock::new();
    CATALOG_CACHE.get_or_init(|| {
        serde_json::from_str(CATALOG).expect("内置能力目录损坏：assets/capabilities.json 无法解析")
    })
}

/// 解析后的内置目录。
#[derive(Debug, Clone)]
pub struct Catalog {
    pub revision: String,
    models: HashMap<String, ModelCapability>,
}

impl Catalog {
    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// 精确名查询（§16.6：目录只提供初始能力判断）。
    pub fn get(&self, model: &str) -> Option<&ModelCapability> {
        self.models.get(model)
    }

    /// 目录对这个模型的某项能力怎么说（§16.6）。
    ///
    /// 三个返回值含义严格区分：
    /// - \`Some(true)\`：目录明确说支持；
    /// - \`Some(false)\`：目录明确说不支持；
    /// - \`None\`：目录里没有这个模型，或该项字段没写——**一律按未知处理**。
    ///
    /// 把 \`None\` 当成 \`Some(false)\` 会让目录里没收录的新模型全部被降权，
    /// 那比不接入还糟。
    pub fn supports(&self, model: &str, capability: &str) -> Option<bool> {
        let entry = self.models.get(model)?;
        match capability {
            "function_calling" => Some(entry.function_calling),
            "vision" => Some(entry.vision),
            "response_schema" => Some(entry.response_schema),
            "reasoning" => Some(entry.reasoning),
            // 其余能力目录不表态。能力表达由协议层判断。
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Catalog {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            revision: String,
            models: HashMap<String, ModelCapability>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            revision: raw.revision,
            models: raw.models,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_parses_and_carries_no_prices() {
        let catalog = builtin();
        assert!(!catalog.is_empty());
        assert!(!catalog.revision().is_empty());
        let known = catalog.get("gpt-4o").expect("目录里应该有 gpt-4o");
        assert!(known.function_calling);
        assert!(known.vision);
        assert!(known.max_input_tokens.unwrap_or(0) > 100_000);
    }

    #[test]
    fn unknown_models_fall_back_to_none() {
        assert!(builtin().get("不存在的模型").is_none());
        // 目录里没有的模型 -> 能力未知，绝不等于"不支持"（§16.6）。
        assert_eq!(builtin().supports("不存在的模型", "vision"), None);
        // 目录不表态的能力也是未知，而不是不支持。
        assert_eq!(builtin().supports("gpt-4o", "some_exotic_capability"), None);
    }

    #[test]
    fn the_catalog_answers_the_capabilities_it_knows() {
        let catalog = builtin();
        // 只用目录里一定存在的模型断言，避免目录换版后测试变脆。
        assert_eq!(catalog.supports("gpt-4o", "vision"), Some(true));
        assert_eq!(catalog.supports("gpt-4o", "function_calling"), Some(true));
        // gpt-3.5-turbo 在 LiteLLM 目录里不标 vision。
        assert_eq!(
            catalog.supports("gpt-3.5-turbo", "vision"),
            Some(false),
            "目录明确标了不支持就要如实返回 false"
        );
    }
}
