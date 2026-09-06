//! 端点选择顺序（§14.3）。
//!
//! 对一个已选调度目标，按以下顺序排出可用端点：
//!
//! 1. 与下游协议相同且未被证实不支持的端点——透传，零转换代价。
//! 2. 账号首选端点，前提是请求可无损转换。
//! 3. 其他转换保真度相同的端点。
//! 4. 需要降级白名单内能力才能使用的端点。
//!
//! 第 4 档被排在最后，等价于"降级只在故障切换时生效"：层内的无损目标全部
//! 试完之前，需要降级的端点根本轮不到（§14.8）。分组关掉降级开关时它们直接
//! 被剔除。
//!
//! ��确不支持的端点在证据过期或配置变化前不会重复尝试。

use std::time::Instant;

use crate::domain::{Account, Protocol};
use crate::protocol::degrade::{Fidelity, Unsupported};
use crate::protocol::translate::Translation;
use crate::upstream::Endpoint;
use crate::upstream::evidence::Evidence;

/// 一个可用端点及其保真度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub endpoint: Endpoint,
    pub fidelity: Fidelity,
}

impl Choice {
    pub fn is_lossless(&self) -> bool {
        self.fidelity == Fidelity::Lossless
    }
}

/// 一次尝试最多试几个端点。
///
/// 纯失控保护：真正有价值的只有"原生优先，不行就转换"这两步。同一个账号连
/// 换三个端点都 404，说明 Base URL 本身就是错的，再试也是浪费预算（§13.1）。
pub const MAX_ENDPOINTS_PER_TARGET: usize = 2;

/// 排出某个账号对这次请求的端点尝试顺序。
///
/// 返回空列表意味着这个目标对本次请求不合格，错误里带着原因。
pub fn choices(
    account: &Account,
    downstream: Endpoint,
    translation: &Translation<'_>,
    evidence: &Evidence,
    allow_degrade: bool,
    now: Instant,
) -> Result<Vec<Choice>, Unsupported> {
    // `count_tokens` 只能原生转发：Chat 与 Responses 没有等价端点，本地精确
    // 计数也不可行，按 §15.5 直接返回不支持而不是估算一个数字冒充精确值。
    if downstream == Endpoint::CountTokens {
        let plausible =
            account.adaptive_protocol || account.preferred_protocol == Protocol::AnthropicMessages;
        if !plausible || evidence.is_unsupported(&account.id, downstream, now) {
            return Err(Unsupported::new(
                "该账号没有 /v1/messages/count_tokens 端点，Token 计数无法跨协议表达",
            ));
        }
        return Ok(vec![Choice {
            endpoint: downstream,
            fidelity: Fidelity::Lossless,
        }]);
    }

    let native = Endpoint::native(translation.downstream());
    let preferred = Endpoint::native(account.preferred_protocol);
    // 关掉运行时适配就只用首选端点，哪怕下游协议正好有原生端点也不用——这是
    // 管理员显式表达的"我知道这个站只认这一条路"（§14.2）。
    let ordered: Vec<Endpoint> = if account.adaptive_protocol {
        let mut all = vec![native, preferred];
        all.extend(Endpoint::INFERENCE);
        all
    } else {
        vec![preferred]
    };

    let mut choices: Vec<Choice> = Vec::new();
    let mut refusal: Option<Unsupported> = None;
    for endpoint in ordered {
        if choices.iter().any(|choice| choice.endpoint == endpoint) {
            continue;
        }
        if evidence.is_unsupported(&account.id, endpoint, now) {
            continue;
        }
        // 与下游同协议的端点是纯透传：它没被证实缺失之前就是最优解，后面的
        // 端点连算都不用算。热路径因此不会为一个用不上的备胎解析整个请求体
        // （§14.3 的第 1 档、§19.4）。
        if endpoint.protocol() == translation.downstream() {
            choices.push(Choice {
                endpoint,
                fidelity: Fidelity::Lossless,
            });
            break;
        }
        match translation.fidelity(endpoint.protocol()) {
            Ok(Fidelity::Degraded(_)) if !allow_degrade => {}
            Ok(fidelity) => choices.push(Choice { endpoint, fidelity }),
            // 表达不了就记下原因；只有一个端点都排不出来时才把它报出来。
            Err(reason) => refusal = refusal.or(Some(reason)),
        }
    }

    if choices.is_empty() {
        return Err(refusal.unwrap_or_else(|| {
            Unsupported::new(format!("账号「{}」没有可用于本次请求的端点", account.name))
        }));
    }
    // 无损端点一律排在需要降级的端点之前。
    choices.sort_by_key(|choice| u8::from(!choice.is_lossless()));
    choices.truncate(MAX_ENDPOINTS_PER_TARGET);
    Ok(choices)
}

/// 该端点的 404 / 405 是否足以证明"这条路由不存在"（§16.7）。
///
/// 只有**推测性**尝试才算数：账号首选端点上的 404 更可能是"模型不存在"，把它
/// 当成端点缺失会误关一条本来可用的通路。推测端点上猜错的代价只是接下来 24
/// 小时改走转换，不影响正确性。
pub fn proves_missing_endpoint(account: &Account, endpoint: Endpoint, status: u16) -> bool {
    let speculative = endpoint != Endpoint::native(account.preferred_protocol);
    speculative && matches!(status, 404 | 405)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::OffsetDateTime;

    use super::*;
    use crate::domain::{Limits, Multiplier, MultiplierMode, UpstreamType};

    fn account(preferred: Protocol, adaptive: bool) -> Account {
        Account {
            id: "acc".into(),
            group_id: "g".into(),
            name: "账号A".into(),
            upstream_type: UpstreamType::OpenAiCompatible,
            base_url: "https://api.example.com".into(),
            preferred_protocol: preferred,
            adaptive_protocol: adaptive,
            default_priority: 50,
            calibration: Multiplier::ONE,
            multiplier_mode: MultiplierMode::Manual,
            manual_multiplier: Multiplier::ONE,
            new_api_user_id: None,
            new_api_group: None,
            limits: Limits::default(),
            allow_private_network: false,
            enabled: true,
            auto_sync: false,
            model_synced_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn plain() -> serde_json::Value {
        json!({"model": "m", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]})
    }

    #[test]
    fn the_native_endpoint_comes_first_even_when_it_is_not_preferred() {
        let body = plain();
        let translation = Translation::new(Protocol::AnthropicMessages, &body);
        let picked = choices(
            &account(Protocol::OpenAiChat, true),
            Endpoint::Messages,
            &translation,
            &Evidence::new(),
            true,
            Instant::now(),
        )
        .unwrap();

        // 同协议端点未被证实缺失时就是最优解，不必再为备胎做一次转换。
        assert_eq!(
            picked,
            vec![Choice {
                endpoint: Endpoint::Messages,
                fidelity: Fidelity::Lossless
            }]
        );
    }

    #[test]
    fn the_converted_endpoint_appears_once_the_native_one_is_ruled_out() {
        let body = plain();
        let translation = Translation::new(Protocol::AnthropicMessages, &body);
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_unsupported("acc", Endpoint::Messages, now);

        let picked = choices(
            &account(Protocol::OpenAiChat, true),
            Endpoint::Messages,
            &translation,
            &evidence,
            true,
            now,
        )
        .unwrap();
        assert_eq!(picked[0].endpoint, Endpoint::ChatCompletions, "改走转换");
        assert!(
            picked.iter().all(|c| c.endpoint != Endpoint::Messages),
            "已证实不存在的端点不再重复尝试"
        );
    }

    #[test]
    fn turning_off_adaptive_protocol_pins_the_preferred_endpoint() {
        let body = plain();
        let translation = Translation::new(Protocol::AnthropicMessages, &body);
        let picked = choices(
            &account(Protocol::OpenAiChat, false),
            Endpoint::Messages,
            &translation,
            &Evidence::new(),
            true,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].endpoint, Endpoint::ChatCompletions);
    }

    #[test]
    fn degraded_endpoints_sort_last_and_vanish_when_the_group_forbids_them() {
        // 带签名思考历史的 Anthropic 请求：转到 Chat 必然丢思考。
        let body = json!({
            "model": "m", "max_tokens": 16,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "推理", "signature": "sig"},
                {"type": "text", "text": "答案"}
            ]}]
        });
        let translation = Translation::new(Protocol::AnthropicMessages, &body);
        let evidence = Evidence::new();
        let now = Instant::now();
        // 首选 Responses 的账号：原生 Messages 无损，Chat 与 Responses 都要降级。
        let account = account(Protocol::OpenAiResponses, true);

        let picked = choices(
            &account,
            Endpoint::Messages,
            &translation,
            &evidence,
            true,
            now,
        )
        .unwrap();
        assert_eq!(picked[0].endpoint, Endpoint::Messages);
        assert!(picked[0].is_lossless(), "无损端点必须排在最前");

        // 原生端点已经证实不存在：只剩需要降级的端点，仍然可用但被标记。
        evidence.note_unsupported("acc", Endpoint::Messages, now);
        let picked = choices(
            &account,
            Endpoint::Messages,
            &translation,
            &evidence,
            true,
            now,
        )
        .unwrap();
        assert!(picked.iter().all(|choice| !choice.is_lossless()));
        assert_eq!(
            picked[0].fidelity,
            Fidelity::Degraded(vec!["thinking".into()])
        );

        // 分组禁止降级：这个目标彻底不合格，绝不静默丢思考（§14.8）。
        let refused = choices(
            &account,
            Endpoint::Messages,
            &translation,
            &evidence,
            false,
            now,
        );
        assert!(refused.is_err());
    }

    #[test]
    fn count_tokens_is_native_only() {
        let body = json!({"model": "m", "messages": []});
        let translation = Translation::new(Protocol::AnthropicMessages, &body);
        let evidence = Evidence::new();
        let now = Instant::now();

        // 首选 Chat 的账号也只能用原生 count_tokens，绝不转换。
        let picked = choices(
            &account(Protocol::OpenAiChat, true),
            Endpoint::CountTokens,
            &translation,
            &evidence,
            true,
            now,
        )
        .unwrap();
        assert_eq!(
            picked,
            vec![Choice {
                endpoint: Endpoint::CountTokens,
                fidelity: Fidelity::Lossless
            }]
        );

        // 证实没有这个端点之后返回不支持，而不是估算一个 Token 数（§15.5）。
        evidence.note_unsupported("acc", Endpoint::CountTokens, now);
        assert!(
            choices(
                &account(Protocol::OpenAiChat, true),
                Endpoint::CountTokens,
                &translation,
                &evidence,
                true,
                now
            )
            .is_err()
        );

        // 关掉适配的 Chat 账号根本没有 Anthropic 端点可用，连试都不必试。
        assert!(
            choices(
                &account(Protocol::OpenAiChat, false),
                Endpoint::CountTokens,
                &translation,
                &Evidence::new(),
                true,
                now
            )
            .is_err()
        );
        // 关掉适配的 Anthropic 账号则照常原生转发。
        assert!(
            choices(
                &account(Protocol::AnthropicMessages, false),
                Endpoint::CountTokens,
                &translation,
                &Evidence::new(),
                true,
                now
            )
            .is_ok()
        );
    }

    #[test]
    fn an_inexpressible_request_leaves_only_the_native_endpoint() {
        // `n: 3` 无法跨协议表达，但原生 Chat 目标照常透传。
        let body = json!({"model": "m", "messages": [], "n": 3});
        let translation = Translation::new(Protocol::OpenAiChat, &body);
        let picked = choices(
            &account(Protocol::AnthropicMessages, true),
            Endpoint::ChatCompletions,
            &translation,
            &Evidence::new(),
            true,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].endpoint, Endpoint::ChatCompletions);
    }

    #[test]
    fn only_speculative_endpoints_can_prove_a_missing_route() {
        let account = account(Protocol::OpenAiChat, true);
        // 首选端点上的 404 更可能是"模型不存在"，不能当作端点缺失。
        assert!(!proves_missing_endpoint(
            &account,
            Endpoint::ChatCompletions,
            404
        ));
        assert!(proves_missing_endpoint(&account, Endpoint::Messages, 404));
        assert!(proves_missing_endpoint(&account, Endpoint::Messages, 405));
        // 5xx 与超时不能证明任何能力（§16.7）。
        assert!(!proves_missing_endpoint(&account, Endpoint::Messages, 500));
        assert!(!proves_missing_endpoint(&account, Endpoint::Messages, 429));
    }
}
