//! 领域类型：分组、上游账号、逻辑模型与调度目标（§4）。

pub mod fixed;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use fixed::Multiplier;

/// 下游入口协议，同时也是上游原生端点的类型。
///
/// serde 名称必须与 [`Protocol::as_str`] 完全一致：前者是 API 契约，后者是
/// 数据库取值，两者一旦分叉，后台存进去的值网关就读不出来。因此每个变体都
/// 显式 `rename`，不依赖 `rename_all` 的推导规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Protocol {
    /// `POST /v1/chat/completions`
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    /// `POST /v1/responses`
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// `POST /v1/messages`
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

impl Protocol {
    /// 数据库与 API 中使用的稳定标识。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai_chat",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "openai_chat" => Some(Self::OpenAiChat),
            "openai_responses" => Some(Self::OpenAiResponses),
            "anthropic_messages" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }
}

/// 逻辑模型的来源，决定零目标时是清理还是保留（§4.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOrigin {
    /// 勾选上游模型时自动创建，最后一个目标移除后自动清理。
    Auto,
    /// 管理员显式创建，零目标时保留记录但从 `/v1/models` 移除。
    Manual,
}

impl ModelOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "auto" => Some(Self::Auto),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// 账号的倍率来源（§11.2）。每个账号只能选一种。
///
/// 手动来源的状态永远是"已知"，不受自动刷新失败影响；两种自动来源都需要
/// 后台探针周期性刷新，失败后按 §11.4 进入宽限期。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MultiplierMode {
    #[serde(rename = "manual")]
    Manual,
    /// Key 级 `/v1/sub2api/billing`，一把 API Key 即可。
    #[serde(rename = "sub2api")]
    Sub2Api,
    /// `/api/user/self/groups`，需要访问令牌与用户 ID 两个额外凭据。
    #[serde(rename = "new_api")]
    NewApi,
}

impl MultiplierMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Sub2Api => "sub2api",
            Self::NewApi => "new_api",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "manual" => Some(Self::Manual),
            "sub2api" => Some(Self::Sub2Api),
            "new_api" => Some(Self::NewApi),
            _ => None,
        }
    }

    /// 是否需要后台探针刷新。
    pub fn is_automatic(self) -> bool {
        !matches!(self, Self::Manual)
    }
}

/// RPM、TPM 与最大并发限制（§17.1）。`None` 表示不限。
///
/// 账号提供默认值，调度目标可以逐项覆盖——覆盖的粒度是"某一项"，不是整组，
/// 否则想单独收紧 RPM 的人还得把并发也抄一遍。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Limits {
    pub rpm: Option<u32>,
    /// TPM 只能按请求体保守估算，后台需要显式标注"估算限流"（§17.2）。
    pub tpm: Option<u32>,
    pub max_concurrency: Option<u32>,
}

impl Limits {
    /// 调用方没提并发时，沿用已经配好的上限。
    ///
    /// `None` 在准入路径上表示"调用方对并发没有意见"，而**不是**"把上限改成
    /// 不限"。两者混同会让一个已经校准到 1 的目标被当成有空位（§17.1）。
    pub fn with_configured(self, configured: Option<u32>) -> Limits {
        Limits {
            max_concurrency: self.max_concurrency.or(configured),
            ..self
        }
    }

    /// 用目标的覆盖值盖住账号默认值，逐项生效。
    pub fn overridden_by(self, over: Limits) -> Limits {
        Limits {
            rpm: over.rpm.or(self.rpm),
            tpm: over.tpm.or(self.tpm),
            max_concurrency: over.max_concurrency.or(self.max_concurrency),
        }
    }
}

/// 分组调度权重，四项之和必须为 100（§6.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulingWeights {
    pub multiplier: u32,
    pub reliability: u32,
    pub first_token: u32,
    pub throughput: u32,
}

impl SchedulingWeights {
    pub const TOTAL: u32 = 100;

    pub fn sum(&self) -> u32 {
        self.multiplier + self.reliability + self.first_token + self.throughput
    }

    pub fn is_valid(&self) -> bool {
        self.sum() == Self::TOTAL
    }
}

impl Default for SchedulingWeights {
    /// 默认 40 / 25 / 20 / 15（§6.3）。
    fn default() -> Self {
        Self {
            multiplier: 40,
            reliability: 25,
            first_token: 20,
            throughput: 15,
        }
    }
}

/// 分组：完全独立的调用空间，也是调度的硬边界（§4.1）。
#[derive(Debug, Clone)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub key_digest_hex: String,
    /// 允许使用的最高有效倍率，绝对红线。
    pub multiplier_limit: Multiplier,
    pub weights: SchedulingWeights,
    pub queue_capacity: u32,
    /// 层内全忙时最多等待多久；0 表示跟随请求总超时（§6.3）。
    pub max_wait_secs: u32,
    pub allow_degrade: bool,
    /// 上游不支持原生后台时，是否允许网关托管后台任务（计划 §29.1）。
    pub allow_managed_background: bool,
    pub created_at: OffsetDateTime,
}

/// 上游账号：一套独立凭据与连接配置（§4.2）。
#[derive(Debug, Clone)]
pub struct Account {
    pub id: String,
    pub group_id: String,
    pub name: String,
    pub base_url: String,
    pub preferred_protocol: Protocol,
    pub adaptive_protocol: bool,
    pub default_priority: i32,
    /// 手填的账号级校准系数，`有效倍率 = 上游倍率 × 校准系数`。
    pub calibration: Multiplier,
    /// 倍率来源。手动以外的来源由后台探针刷新（§11.2）。
    pub multiplier_mode: MultiplierMode,
    /// 手动倍率，同时也是自动来源尚未刷新成功前的初值。
    pub manual_multiplier: Multiplier,
    /// New API 探针必需的用户 ID（`New-Api-User` 请求头）。
    pub new_api_user_id: Option<String>,
    /// New API 站点上这把 Key 所属的分组名；留空时保守取可用分组中的最高倍率。
    pub new_api_group: Option<String>,
    /// 账号级 RPM / TPM / 最大并发默认值（§17.1）。
    pub limits: Limits,
    pub allow_private_network: bool,
    pub enabled: bool,
    /// 模型自动同步：打开后忽略选择集，全量托管上游模型（§16.2）。
    pub auto_sync: bool,
    /// 账号级隐藏原始模型开关（§16.4 修订）。
    ///
    /// 打开后只暴露目录里设置了"下游模型名"的模型；没有设置下游模型名的
    /// 目录行不会对下游开放。
    pub hide_original: bool,
    /// 上一次模型同步完成的时间；`None` 表示从未同步过。
    pub model_synced_at: Option<i64>,
    pub created_at: OffsetDateTime,
}

impl Account {
    /// 由手动倍率算出的有效倍率。
    ///
    /// 自动来源的账号必须改用 [`crate::multiplier`] 中的动态状态：倍率是动态
    /// 安全状态，不跟随配置版本（§21）。这里保留的是"配置里写着什么"。
    pub fn configured_effective_multiplier(&self) -> Multiplier {
        self.manual_multiplier.mul_ceil(self.calibration)
    }
}

/// 逻辑模型：下游看到的名称（§4.4）。
#[derive(Debug, Clone)]
pub struct LogicalModel {
    pub id: String,
    pub group_id: String,
    pub name: String,
    pub origin: ModelOrigin,
    pub enabled: bool,
    pub created_at: OffsetDateTime,
}

/// 调度目标：`分组 + 上游账号 + 具体上游模型`（§4.5）。
#[derive(Debug, Clone)]
pub struct DispatchTarget {
    pub id: String,
    pub logical_model_id: String,
    pub account_id: String,
    pub upstream_model: String,
    /// 是否隐藏这个目标的上游原始模型名。
    ///
    /// 为 false 时，除了逻辑模型的对外名，客户端还可以直接用上游真名请求，
    /// 这样同一模型在不同站点的不同命名都能被同一组目标接住（§16.4）。
    pub hide_original: bool,
    /// 历史字段：调度目标不再支持独立优先级覆盖，统一继承账号人工优先级。
    /// 保留在数据结构里是为了兼容旧备份；配置装配与界面都不再使用它（§9.2 修订）。
    pub priority_override: Option<i32>,
    /// 逐项覆盖账号的 RPM / TPM / 最大并发（§17.1）。
    pub limits: Limits,
    pub enabled: bool,
    pub created_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_roundtrips_through_its_stable_identifier() {
        for protocol in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ] {
            assert_eq!(Protocol::parse(protocol.as_str()), Some(protocol));
        }
        assert_eq!(Protocol::parse("gemini"), None);
    }

    #[test]
    fn serde_names_match_the_database_identifiers() {
        // 两者分叉会让后台写入的值在网关侧无法识别，必须锁死。
        for protocol in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ] {
            let encoded = serde_json::to_string(&protocol).unwrap();
            assert_eq!(encoded, format!("\"{}\"", protocol.as_str()));
            assert_eq!(
                serde_json::from_str::<Protocol>(&encoded).unwrap(),
                protocol
            );
        }
        for origin in [ModelOrigin::Auto, ModelOrigin::Manual] {
            let encoded = serde_json::to_string(&origin).unwrap();
            assert_eq!(encoded, format!("\"{}\"", origin.as_str()));
        }
        for mode in [
            MultiplierMode::Manual,
            MultiplierMode::Sub2Api,
            MultiplierMode::NewApi,
        ] {
            let encoded = serde_json::to_string(&mode).unwrap();
            assert_eq!(encoded, format!("\"{}\"", mode.as_str()));
            assert_eq!(MultiplierMode::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn target_limits_override_the_account_item_by_item() {
        let account = Limits {
            rpm: Some(600),
            tpm: Some(200_000),
            max_concurrency: Some(8),
        };
        // 只想单独收紧并发的人不必把 RPM 和 TPM 重抄一遍。
        let effective = account.overridden_by(Limits {
            max_concurrency: Some(2),
            ..Limits::default()
        });
        assert_eq!(effective.rpm, Some(600));
        assert_eq!(effective.tpm, Some(200_000));
        assert_eq!(effective.max_concurrency, Some(2));
    }

    #[test]
    fn only_manual_multipliers_skip_the_refresh_task() {
        assert!(!MultiplierMode::Manual.is_automatic());
        assert!(MultiplierMode::Sub2Api.is_automatic());
        assert!(MultiplierMode::NewApi.is_automatic());
    }

    #[test]
    fn default_weights_sum_to_one_hundred() {
        let weights = SchedulingWeights::default();
        assert_eq!(weights.multiplier, 40);
        assert!(weights.is_valid());
    }

    #[test]
    fn invalid_weights_are_detected() {
        let weights = SchedulingWeights {
            multiplier: 40,
            reliability: 25,
            first_token: 20,
            throughput: 10,
        };
        assert!(!weights.is_valid());
    }
}
