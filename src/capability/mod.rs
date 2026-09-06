//! 模型能力：内置目录、证据学习与查询（§16.6、§16.7）。
//!
//! 三层证据来源，优先级固定（§16.6）：
//!
//! 1. **明确的真实请求结果**——本进程里学到的东西，最高优先级。
//! 2. **上游接口返回**——同上，只是证据来自上游的明确声明。
//! 3. **Akhub 内置适配规则**——由协议层直接判定，不经过这里。
//! 4. **开源能力目录**——随二进制发布的 LiteLLM 精简快照，只提供初始判断。
//!
//! 目录版本随源数据写死；运行中绝不读取远程仓库，也不静默热替换（§16.6）。

use std::collections::HashMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::upstream::evidence::TTL;

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

// ---------------------------------------------------------------- 能力学习

/// 一次能力限制的证据（§16.7）。
#[derive(Debug, Clone)]
struct Limitation {
    /// 能力名，与 `X-Akhub-Degraded` / 硬性资格过滤用的同一套词表。
    capability: String,
    expires_at: std::time::Instant,
}

/// 进程内能力证据（§16.7）。
///
/// 只缓存**明确"不支持"**：普通 400、5xx、超时和网络错误不能证明能力不支持，
/// 调用方在记录前必须先判定错误形状。限制默认 24 小时过期；模型列表变化、
/// 账号协议变化、适配器版本变化时由配置重载路径调用 [`Capabilities::clear`]
/// 立即失效。
///
/// 不持久化：重启后第一个请求用一次明确失败重新学会，代价可控，而把它写进
/// 数据库要多一张表和一条恢复路径。
#[derive(Default)]
pub struct Capabilities {
    limitations: RwLock<HashMap<(String, String), Limitation>>,
}

impl Capabilities {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次"这个账号的这个模型明确不支持这个能力"。
    pub fn note_unsupported(
        &self,
        account_id: &str,
        model: &str,
        capability: &str,
        now: std::time::Instant,
    ) {
        if let Ok(mut map) = self.limitations.write() {
            map.insert(
                (account_id.to_string(), model.to_string()),
                Limitation {
                    capability: capability.to_string(),
                    expires_at: now + TTL,
                },
            );
        }
    }

    /// 该模型此刻是否已被证实不支持该能力。
    pub fn is_unsupported(
        &self,
        account_id: &str,
        model: &str,
        capability: &str,
        now: std::time::Instant,
    ) -> bool {
        self.limitations
            .read()
            .ok()
            .and_then(|map| {
                map.get(&(account_id.to_string(), model.to_string()))
                    .cloned()
            })
            .is_some_and(|limitation| {
                limitation.capability == capability && limitation.expires_at > now
            })
    }

    /// 全部失效。配置变化（模型列表、账号协议、适配器版本）时调用（§16.7）。
    pub fn clear(&self) {
        if let Ok(mut map) = self.limitations.write() {
            map.clear();
        }
    }

    /// 当前有效限制条数，供后台展示。
    pub fn len(&self, now: std::time::Instant) -> usize {
        self.limitations
            .read()
            .map(|map| map.values().filter(|l| l.expires_at > now).count())
            .unwrap_or(0)
    }

    pub fn is_empty(&self, now: std::time::Instant) -> bool {
        self.len(now) == 0
    }
}

/// 上游错误能否证明"该模型不支持某能力"，能的话给出能力名（§16.7）。
///
/// 只有明确的拒绝才算数：模型名错误、参数错误、限流、5xx、超时与网络错误
/// 全部不能证明。`404` 单独处理——它更可能是"模型不存在"而不是"能力缺失"，
/// 由端点证据与模型目录去管，不在这里缓存。
///
/// 能力名与内置目录的同一套词表（`function_calling`、`vision`……），也和
/// 请求侧的 [`crate::protocol::canonical::Request::requested_capabilities`]
/// 对齐；提取不出具体能力就不缓存——一条无法归因的错误不能关掉整个模型。
pub fn unsupported_from_error(status: u16, body: &serde_json::Value) -> Option<&'static str> {
    if status != 400 {
        return None;
    }
    let text = body.to_string().to_lowercase();
    if !REJECTION_HINTS.iter().any(|hint| text.contains(hint)) {
        return None;
    }
    CAPABILITY_HINTS
        .iter()
        .find(|(pattern, _)| text.contains(pattern))
        .map(|(_, capability)| *capability)
}

/// 上游在拒绝时常用的说法。
const REJECTION_HINTS: &[&str] = &["not support", "unsupported", "不支持", "无法支持"];

/// 错误文案里的能力线索 → 词表能力名。
const CAPABILITY_HINTS: &[(&str, &str)] = &[
    ("function", "function_calling"),
    ("tool", "function_calling"),
    ("image", "vision"),
    ("vision", "vision"),
    ("pdf", "vision"),
    ("schema", "response_schema"),
    ("response_format", "response_schema"),
    ("structured output", "response_schema"),
    ("thinking", "reasoning"),
    ("reasoning", "reasoning"),
];

/// 丢掉也不影响客户端工作的能力（§14.8 降级白名单）。
///
/// 请求需要的能力被证实不支持时：白名单内的照样发（最多降级），白名单外的
/// 该目标硬性不合格（§9.1）。
pub const DEGRADABLE: &[&str] = &["reasoning"];

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
    }

    #[test]
    fn limitations_are_scoped_expire_and_clear() {
        let caps = Capabilities::new();
        let now = std::time::Instant::now();
        caps.note_unsupported("acc", "m1", "vision", now);

        assert!(caps.is_unsupported("acc", "m1", "vision", now));
        assert!(
            !caps.is_unsupported("acc", "m1", "tools", now),
            "能力粒度独立"
        );
        assert!(
            !caps.is_unsupported("acc", "m2", "vision", now),
            "模型粒度独立"
        );
        assert!(
            !caps.is_unsupported("acc2", "m1", "vision", now),
            "账号粒度独立"
        );

        assert!(caps.is_empty(now + TTL + std::time::Duration::from_secs(1)));
        caps.note_unsupported("acc", "m1", "vision", now);
        caps.clear();
        assert!(caps.is_empty(now));
    }

    #[test]
    fn only_explicit_rejections_earn_a_limitation() {
        let unsupported = serde_json::json!({
            "error": {"message": "model does not support images", "type": "invalid_request_error"}
        });
        assert_eq!(unsupported_from_error(400, &unsupported), Some("vision"));

        // 参数错误、限流、模型不存在、5xx 都不能证明能力不支持（§16.7）。
        let plain =
            serde_json::json!({"error": {"message": "bad", "type": "invalid_request_error"}});
        assert_eq!(unsupported_from_error(400, &plain), None);
        assert_eq!(unsupported_from_error(404, &unsupported), None);
        assert_eq!(unsupported_from_error(429, &unsupported), None);
        assert_eq!(unsupported_from_error(500, &unsupported), None);

        // 拒绝说得清楚才能归因；说不清楚就不缓存。
        let vague = serde_json::json!({"error": {"message": "请求无法被支持", "type": "invalid_request_error"}});
        assert_eq!(unsupported_from_error(400, &vague), None);
    }

    #[test]
    fn the_degrade_whitelist_matches_the_plan() {
        // 思考可以丢（§14.8），工具与 Schema 不行。
        assert!(DEGRADABLE.contains(&"reasoning"));
        assert!(!DEGRADABLE.contains(&"function_calling"));
        assert!(!DEGRADABLE.contains(&"response_schema"));
        assert!(!DEGRADABLE.contains(&"vision"));
    }
}
