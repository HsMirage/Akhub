//! 端点选择顺序（§14.3）。
//!
//! 对一个已选调度目标，按五个档位排出可用端点。档位由下面的 tier() 计算，
//! 数字小的先试；**同档内保持候选的自然顺序**（稳定排序）：
//!
//! 1. 与下游协议相同，且**已确认支持**——透传，零转换代价。
//! 2. 与下游协议相同，但**能力未知**（还没成功过一次，也没被证伪）。
//! 3. 账号首选端点，前提是请求可无损转换。
//! 4. 其他**已确认支持**且能无损表达的端点。
//! 5. 其余能无损表达的端点（能力未知）。需要降级白名单内能力的端点排在所有
//!    无损端点之后。
//!
//! 第 1 档与第 2 档的差别只在"有没有成功过一次"，但它和第 4 档一起构成了
//! "已确认支持优先于能力未知"这条规则：两个都能表达这次请求的端点里，已经
//! 成功过的那个先试，不必再拿一次失败去试错。
//!
//! 降级端点排在最后，等价于"降级只在故障切换时生效"：层内的无损端点全部试完
//! 之前，需要降级的端点根本轮不到（§14.8）。分组关掉降级开关时它们直接被剔除。
//!
//! 已证实**不支持**的端点在证据过期或配置变化前不会重复尝试。

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
    // `count_tokens`、`compact` 与 `input_tokens` 只能原生转发：没有跨协议
    // 等价物，本地精确计数也不可行，按 §15.4、§15.5 直接返回不支持，而不是
    // 估算一个数字或替换一种语义来冒充。
    if downstream.is_native_only() {
        let (plausible, missing) = match downstream {
            Endpoint::CountTokens => (
                account.adaptive_protocol
                    || account.preferred_protocol == Protocol::AnthropicMessages,
                "该账号没有 /v1/messages/count_tokens 端点，Token 计数无法跨协议表达",
            ),
            Endpoint::ResponsesCompact => (
                account.adaptive_protocol
                    || account.preferred_protocol == Protocol::OpenAiResponses,
                "该账号没有 /v1/responses/compact 端点，压缩无法跨协议表达",
            ),
            Endpoint::ResponsesInputTokens => (
                account.adaptive_protocol
                    || account.preferred_protocol == Protocol::OpenAiResponses,
                "该账号没有 /v1/responses/input_tokens 端点，Token 计数无法跨协议表达",
            ),
            Endpoint::ImagesGenerations | Endpoint::ImagesEdits => {
                let plausible = matches!(
                    account.preferred_protocol,
                    Protocol::OpenAiChat | Protocol::OpenAiResponses
                ) || account.adaptive_protocol;
                let missing = format!(
                    "该账号没有 /{} 端点（图片接口仅支持 OpenAI 兼容上游原生转发）",
                    downstream.path()
                );
                if !plausible || evidence.is_unsupported(&account.id, downstream, now) {
                    return Err(Unsupported::new(missing));
                }
                return Ok(vec![Choice {
                    endpoint: downstream,
                    fidelity: Fidelity::Lossless,
                }]);
            }
            _ => unreachable!("is_native_only 只覆盖辅助端点"),
        };
        if !plausible || evidence.is_unsupported(&account.id, downstream, now) {
            return Err(Unsupported::new(missing));
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
    // 按 §14.3 的五档排序。sort_by_key 是稳定排序，所以同档内保留候选的
    // 自然顺序——INFERENCE 列表本身有一定道理（越靠前的越通用），不该被打乱。
    choices.sort_by_key(|choice| tier(choice, account, translation.downstream(), evidence, now));
    choices.truncate(MAX_ENDPOINTS_PER_TARGET);
    Ok(choices)
}

/// 一个端点候选在 §14.3 里的档位。数字小的先试。
///
/// 抽成独立函数是为了让"档位"这件事可以被单独测试：排序规则散在循环里时，
/// 只能靠构造整个账号来间接验证，很难说清哪一档到底有没有生效。
fn tier(
    choice: &Choice,
    account: &Account,
    downstream: Protocol,
    evidence: &Evidence,
    now: Instant,
) -> u8 {
    // 需要降级的端点不参与"已确认支持"的比较：先按无损与否分开，再看证据，
    // 才不会出现"降级但已确认支持"插到"无损但未知"前面（§14.8）。
    if !choice.is_lossless() {
        return 6;
    }
    let supported = evidence.is_supported(&account.id, choice.endpoint, now);
    if choice.endpoint.protocol() == downstream {
        return if supported { 1 } else { 2 };
    }
    if choice.endpoint == Endpoint::native(account.preferred_protocol) {
        return 3;
    }
    if supported { 4 } else { 5 }
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
    use super::*;
    use crate::domain::{Limits, Multiplier, MultiplierMode, Protocol};
    use serde_json::json;
    use time::OffsetDateTime;

    fn account(preferred: Protocol, adaptive: bool) -> Account {
        Account {
            id: "acc".into(),
            group_id: "g".into(),
            name: "账号A".into(),
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
            hide_original: false,
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
    fn image_endpoints_require_a_plausible_openai_account_and_route_evidence() {
        let body = json!({"model": "m"});
        let translation = Translation::new(Protocol::OpenAiChat, &body);
        let now = Instant::now();

        for endpoint in [Endpoint::ImagesGenerations, Endpoint::ImagesEdits] {
            let refused = choices(
                &account(Protocol::AnthropicMessages, false),
                endpoint,
                &translation,
                &Evidence::new(),
                true,
                now,
            )
            .unwrap_err();
            assert!(refused.to_string().contains(endpoint.path()));

            let picked = choices(
                &account(Protocol::OpenAiResponses, false),
                endpoint,
                &translation,
                &Evidence::new(),
                true,
                now,
            )
            .unwrap();
            assert_eq!(picked[0].endpoint, endpoint);

            let evidence = Evidence::new();
            evidence.note_unsupported("acc", endpoint, now);
            assert!(
                choices(
                    &account(Protocol::OpenAiResponses, false),
                    endpoint,
                    &translation,
                    &evidence,
                    true,
                    now,
                )
                .is_err()
            );
        }
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
    /// 五档排序：已确认支持的原生端点最前，其次是能力未知的原生端点，
    /// 然后是账号首选端点，再是已确认支持的其他端点，降级端点永远最后（§14.3）。
    #[test]
    fn the_five_tiers_order_endpoints_by_confirmed_support() {
        let account = account(Protocol::OpenAiChat, true);
        let downstream = Protocol::AnthropicMessages;
        let now = Instant::now();
        let evidence = Evidence::new();
        let choice = |endpoint| Choice {
            endpoint,
            fidelity: Fidelity::Lossless,
        };

        // 没有任何证据时：
        //  - 与下游同协议的 messages 落在第 2 档（未知）
        //  - 账号首选 chat_completions 落在第 3 档
        //  - 剩下的推测端点落在第 5 档
        let messages = choice(Endpoint::Messages);
        let chat = choice(Endpoint::ChatCompletions);
        let responses = choice(Endpoint::Responses);
        assert_eq!(tier(&messages, &account, downstream, &evidence, now), 2);
        assert_eq!(tier(&chat, &account, downstream, &evidence, now), 3);
        assert_eq!(tier(&responses, &account, downstream, &evidence, now), 5);

        // messages 成功过一次之后升到第 1 档。
        evidence.note_supported(&account.id, Endpoint::Messages, now);
        assert_eq!(tier(&messages, &account, downstream, &evidence, now), 1);

        // responses 成功过一次之后从第 5 档升到第 4 档——但仍排在首选端点之后，
        // 因为首选端点是管理员显式选的（第 3 档）。
        evidence.note_supported(&account.id, Endpoint::Responses, now);
        assert_eq!(tier(&responses, &account, downstream, &evidence, now), 4);
    }

    /// 降级端点永远排在无损端点之后，哪怕它已经被证实支持（§14.8）。
    #[test]
    fn a_degraded_endpoint_never_outranks_a_lossless_one() {
        let account = account(Protocol::OpenAiChat, true);
        let now = Instant::now();
        let evidence = Evidence::new();
        evidence.note_supported(&account.id, Endpoint::Responses, now);

        let degraded = Choice {
            endpoint: Endpoint::Responses,
            fidelity: Fidelity::Degraded(vec!["thinking".to_string()]),
        };
        let lossless_unknown = Choice {
            endpoint: Endpoint::Messages,
            fidelity: Fidelity::Lossless,
        };
        let degraded_tier = tier(
            &degraded,
            &account,
            Protocol::AnthropicMessages,
            &evidence,
            now,
        );
        let lossless_tier = tier(
            &lossless_unknown,
            &account,
            Protocol::AnthropicMessages,
            &evidence,
            now,
        );
        assert!(
            degraded_tier > lossless_tier,
            "降级端点（{degraded_tier}）必须排在无损端点（{lossless_tier}）之后"
        );
    }

    /// 证据过期后退回"能力未知"，不能一直享受第 1 档（§16.7）。
    #[test]
    fn support_evidence_expires_back_to_unknown() {
        let account = account(Protocol::OpenAiChat, true);
        let now = Instant::now();
        let evidence = Evidence::new();
        evidence.note_supported(&account.id, Endpoint::Messages, now);
        let choice = Choice {
            endpoint: Endpoint::Messages,
            fidelity: Fidelity::Lossless,
        };
        assert_eq!(
            tier(
                &choice,
                &account,
                Protocol::AnthropicMessages,
                &evidence,
                now
            ),
            1
        );
        let later = now + crate::upstream::evidence::TTL + std::time::Duration::from_secs(1);
        assert_eq!(
            tier(
                &choice,
                &account,
                Protocol::AnthropicMessages,
                &evidence,
                later
            ),
            2,
            "过期后退回未知档"
        );
    }
}
