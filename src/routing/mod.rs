//! 候选资格、分层、层内选择、粘性与排队（§9、§10、§13.6）。
//!
//! **严格阶梯**是这个模块唯一不可协商的规则：优先级数字相同的目标构成一层，
//! 当前层只要还有一个合格目标，就绝不使用下一层。层内才轮到评分说话——评分
//! 决定**层内选谁**，永远不影响跨层顺序。

pub mod queue;
pub mod score;
pub mod sticky;

use std::sync::Arc;
use std::time::Duration;

use crate::capability;
use crate::config::{GroupView, TargetView};
use crate::domain::{Multiplier, Protocol};
use crate::gateway::error::ErrorCode;
use crate::health;
use crate::multiplier;
use crate::protocol::translate::Translation;
use crate::upstream::endpoints::{self, Choice};
use crate::upstream::{Endpoint, evidence::Evidence};

/// 失控保护：一次请求最多尝试这么多个目标（§13.1）。
pub const MAX_TARGET_ATTEMPTS: usize = 8;

/// 某个目标不合格的原因。用于在全部目标都失败时给出准确的错误码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ineligible {
    /// 账号或目标被管理员停用。
    Disabled,
    /// 有效倍率高于分组倍率上限（§11.1）。
    MultiplierExceeded,
    /// 自动倍率未知且已超过宽限期（§11.4）。
    MultiplierUnknown,
    /// 这个账号没有能表达本次请求的端点（§14.3、§14.8）。
    Unsupported(String),
    /// 动态状态不允许：熔断、额度耗尽、鉴权失败或暂时容量不足。
    Unavailable(health::Unavailable),
}

impl Ineligible {
    /// 给请求记录用的可读原因（§24.1）。只描述类别，不带账号 ID 等细节。
    fn describe(&self) -> String {
        match self {
            Self::Disabled => "已停用".to_string(),
            Self::MultiplierExceeded => "倍率超限".to_string(),
            Self::MultiplierUnknown => "倍率未知".to_string(),
            Self::Unsupported(_) => "能力不支持".to_string(),
            Self::Unavailable(reason) => match reason {
                health::Unavailable::KeyInvalid => "Key 失效".to_string(),
                health::Unavailable::QuotaExhausted => "额度耗尽".to_string(),
                health::Unavailable::Cooling => "冷却中".to_string(),
                health::Unavailable::ConcurrencyFull => "并发已满".to_string(),
                health::Unavailable::RateLimited => "限流中".to_string(),
            },
        }
    }

    fn error_code(&self) -> ErrorCode {
        match self {
            Self::MultiplierExceeded => ErrorCode::MultiplierExceeded,
            Self::MultiplierUnknown => ErrorCode::MultiplierUnknown,
            Self::Unsupported(_) => ErrorCode::UnsupportedParameter,
            Self::Unavailable(health::Unavailable::RateLimited) => ErrorCode::RateLimited,
            _ => ErrorCode::NoEligibleTarget,
        }
    }

    /// 只有临时容量不足才允许排队等待（§13.6）。
    fn is_queueable(&self) -> bool {
        matches!(self, Self::Unavailable(reason) if reason.is_queueable())
    }
}

/// 选择失败的结果，携带对外错误码与可读原因。
#[derive(Debug, Clone)]
pub struct SelectionFailure {
    pub code: ErrorCode,
    pub message: String,
}

/// 一个候选目标及其此刻的倍率、评分与端点尝试顺序。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub target: Arc<TargetView>,
    /// 此刻的有效倍率，已含校准系数与峰值时段。
    pub multiplier: Multiplier,
    pub score: score::Score,
    /// 按 §14.3 排好的端点尝试顺序，第一个是保真度最高的。
    pub endpoints: Vec<Choice>,
    /// 内置能力目录明确说"这个模型不支持本次请求需要的某项能力"（§16.6）。
    ///
    /// 这只是**初判**，不是硬性不合格：目录可能过时，而真实请求结果才是最高
    /// 优先级的证据。所以它只让这个候选在层内排到后面，不把它踢出计划。
    pub catalog_discouraged: bool,
}

impl Candidate {
    /// 这个目标能否无损表达本次请求。
    pub fn is_lossless(&self) -> bool {
        self.endpoints.first().is_some_and(Choice::is_lossless)
    }

    /// 层内选谁时的偏好档：0 最好。
    ///
    /// - 0：无损，且目录没有异议；
    /// - 1：无损，但目录说这个模型缺某项能力（先试别的，最后才轮到它）；
    /// - 2：需要降级表达（本来就在最后，§14.8）。
    ///
    /// 目录档排在"需要降级"之前是有意的：宁可试一个目录存疑但能无损表达的目标，
    /// 也不要直接丢掉 thinking 这类白名单内的能力。目录也会出错，丢能力不会。
    pub fn preference(&self) -> u8 {
        if !self.is_lossless() {
            2
        } else if self.catalog_discouraged {
            1
        } else {
            0
        }
    }
}

/// 一层候选：优先级数字相同的目标构成一层（§9.2）。
#[derive(Debug, Clone)]
pub struct Layer {
    pub priority: i32,
    /// 已按加权随机排好的尝试顺序。
    pub candidates: Vec<Candidate>,
}

/// 一次选择的完整结果：按"层内先耗尽再降层"排好的尝试计划。
#[derive(Debug, Clone)]
pub struct Plan {
    pub layers: Vec<Layer>,
    /// 分组内最便宜与最贵的合格目标倍率，用于成本反事实基准（§11.6）。
    pub cheapest: Option<Multiplier>,
    pub dearest: Option<Multiplier>,
    /// 被过滤掉的目标及原因，用于请求记录的"候选过滤原因"（§24.1）。
    pub filtered: Vec<(String, usize)>,
}

impl Plan {
    /// 按尝试顺序展开全部候选，并施加 8 个目标的失控保护上限。
    pub fn attempts(&self) -> Vec<&Candidate> {
        self.layers
            .iter()
            .flat_map(|layer| layer.candidates.iter())
            .take(MAX_TARGET_ATTEMPTS)
            .collect()
    }

    /// 第一层的候选。无粘性请求"当前层全满"时只在这一层里等（§13.6）。
    pub fn first_layer(&self) -> &[Candidate] {
        self.layers
            .first()
            .map(|l| &l.candidates[..])
            .unwrap_or(&[])
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// 候选过滤原因摘要（§24.1）。
    ///
    /// 形如 `倍率超限×2,能力不支持×1`；没有过滤掉任何目标时返回 `无`。
    /// 只用于诊断，不参与调度。
    pub fn filter_summary(&self) -> String {
        if self.filtered.is_empty() {
            return "无".to_string();
        }
        self.filtered
            .iter()
            .map(|(reason, count)| format!("{reason}×{count}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// 在计划里找出某个目标，供粘性命中时复用已算好的倍率与评分。
    pub fn find(&self, target_id: &str) -> Option<&Candidate> {
        self.layers
            .iter()
            .flat_map(|layer| layer.candidates.iter())
            .find(|candidate| candidate.target.target.id == target_id)
    }
}

/// 选择时需要的全部动态状态。
pub struct Context<'a> {
    pub health: &'a health::Registry,
    pub perf: &'a score::Registry,
    pub multipliers: &'a multiplier::View,
    /// 端点能力证据：已证实不存在的路由不再重复尝试（§14.2）。
    pub evidence: &'a Evidence,
    /// 模型能力限制：已证实不支持某能力的账号模型（§16.7）。
    pub capabilities: &'a capability::Capabilities,
    /// 一次请求在三个协议上的转换缓存（§14.3）。
    pub translation: &'a Translation<'a>,
    /// 下游入口端点。`count_tokens` 与推理端点的可转换性不同（§15.5）。
    pub endpoint: Endpoint,
    /// 分组是否允许能力降级（§14.8）。
    pub allow_degrade: bool,
    pub protocol: Protocol,
    pub streaming: bool,
    pub now_unix: i64,
    pub now: std::time::Instant,
}

/// 判定单个目标是否可以参与本次请求（§9.1）。
///
/// 返回有效倍率与端点尝试顺序。协议不匹配**不再**是硬性不合格：只要能跨协议
/// 表达，这个目标就是合格候选；只能降级表达时仍然合格，但排在无损目标之后
/// （§9.1 的修订说明与 §14.8）。
pub fn check_eligibility(
    group: &GroupView,
    target: &TargetView,
    context: &Context<'_>,
) -> Result<(Multiplier, Vec<Choice>), Ineligible> {
    if !target.target.enabled || !target.account.enabled {
        return Err(Ineligible::Disabled);
    }

    let effective = context.multipliers.effective(
        &target.account,
        group.group.multiplier_limit,
        context.now_unix,
    );
    // 宽限期已过的未知倍率是硬停：不知道要花多少钱就不能花（§11.4）。
    if !effective.status.is_usable() {
        return Err(Ineligible::MultiplierUnknown);
    }
    // 倍率门必须在定点域内比较：等于上限允许调用，高于则拒绝（§11.1）。
    if effective.value > group.group.multiplier_limit {
        return Err(Ineligible::MultiplierExceeded);
    }

    let choices = endpoints::choices(
        &target.account,
        context.endpoint,
        context.translation,
        context.evidence,
        context.allow_degrade,
        context.now,
    )
    .map_err(|reason| Ineligible::Unsupported(reason.to_string()))?;

    // 已被明确证实不支持、且不在降级白名单内的能力是硬性不合格（§9.1、§16.7）。
    // 白名单内的能力仍可参与——真要丢的时候由降级标记显式呈现（§14.8）。
    for capability in context.translation.requested_capabilities() {
        let prohibited = context.capabilities.is_unsupported(
            &target.account.id,
            &target.target.upstream_model,
            capability,
            context.now,
        ) && !capability::DEGRADABLE.contains(&capability);
        if prohibited {
            return Err(Ineligible::Unsupported(format!(
                "模型 {} 已被证实不支持 {capability}",
                target.target.upstream_model
            )));
        }
    }

    context
        .health
        .check(
            &target.account.id,
            &target.target.id,
            health::AdmissionLimits {
                account: target.account.limits,
                target: target.target.limits,
            },
        )
        .map_err(Ineligible::Unavailable)?;
    Ok((effective.value, choices))
}

/// 为一个逻辑模型排出完整的尝试计划。
///
/// `random` 是层内加权抽签的随机源，注入进来是为了让分配行为可被测试断言。
pub fn plan(
    group: &GroupView,
    model_name: &str,
    context: &Context<'_>,
    random: &mut impl FnMut() -> f64,
) -> Result<Plan, SelectionFailure> {
    // 分组是硬边界：查不到就是查不到，绝不跨组搜索（§7.3）。
    let Some(model) = group.find_model(model_name) else {
        return Err(SelectionFailure {
            code: ErrorCode::ModelNotFound,
            message: format!("分组「{}」中不存在逻辑模型 {model_name}", group.group.name),
        });
    };
    if !model.model.enabled {
        return Err(SelectionFailure {
            code: ErrorCode::ModelNotFound,
            message: format!("逻辑模型 {model_name} 已被停用"),
        });
    }
    // 账号级"隐藏原始模型"可能让一个没有任何下游模型名的目标整体不可达；
    // 这种模型对下游等同于不存在。
    if !model.is_exposed() {
        return Err(SelectionFailure {
            code: ErrorCode::ModelNotFound,
            message: format!("分组「{}」中不存在逻辑模型 {model_name}", group.group.name),
        });
    }

    let mut eligible: Vec<(Arc<TargetView>, Multiplier, Vec<Choice>)> = Vec::new();
    let mut reasons: Vec<Ineligible> = Vec::new();
    for target in &model.targets {
        match check_eligibility(group, target, context) {
            Ok((multiplier, choices)) => eligible.push((Arc::clone(target), multiplier, choices)),
            Err(reason) => {
                // "忙"不是"坏"：暂时满载的目标仍然进入计划，由排队环节处理。
                if reason.is_queueable()
                    && let Ok(choices) = endpoints::choices(
                        &target.account,
                        context.endpoint,
                        context.translation,
                        context.evidence,
                        context.allow_degrade,
                        context.now,
                    )
                {
                    let multiplier = context
                        .multipliers
                        .effective(
                            &target.account,
                            group.group.multiplier_limit,
                            context.now_unix,
                        )
                        .value;
                    eligible.push((Arc::clone(target), multiplier, choices));
                }
                reasons.push(reason);
            }
        }
    }

    if eligible.is_empty() {
        return Err(failure(model_name, group, &reasons));
    }

    // 归一化参照系：倍率是账号级属性，用整个分组作参照系，同一个账号在不同
    // 模型下才会得到一致的倍率得分（§9.4）。
    let cheapest_in_group = group
        .models
        .values()
        .flat_map(|model| model.targets.iter())
        .filter(|target| target.account.enabled && target.target.enabled)
        .map(|target| {
            context
                .multipliers
                .effective(
                    &target.account,
                    group.group.multiplier_limit,
                    context.now_unix,
                )
                .value
        })
        .min();

    let scoring: Vec<score::Candidate> = eligible
        .iter()
        .map(|(target, multiplier, _)| score::Candidate {
            target_id: target.target.id.clone(),
            multiplier: *multiplier,
            multiplier_stale: context
                .multipliers
                .effective(
                    &target.account,
                    group.group.multiplier_limit,
                    context.now_unix,
                )
                .status
                == multiplier::Status::Stale,
            stats: context.perf.stats(
                &target.target.id,
                score::Dimension {
                    protocol: context.protocol,
                    streaming: context.streaming,
                },
            ),
        })
        .collect();
    let scores = score::score_all(&scoring, group.group.weights, cheapest_in_group);

    // 本次请求真正需要的能力（§14.2 的 requested_capabilities）。
    let requested = context.translation.requested_capabilities();
    let candidates: Vec<Candidate> = eligible
        .into_iter()
        .zip(scores)
        .map(|((target, multiplier, endpoints), score)| {
            let catalog_discouraged = catalog_discouraged(context, &target, &requested);
            Candidate {
                target,
                multiplier,
                score,
                endpoints,
                catalog_discouraged,
            }
        })
        .collect();

    let cheapest = candidates.iter().map(|c| c.multiplier).min();
    let dearest = candidates.iter().map(|c| c.multiplier).max();
    // 过滤原因按文本合并计数，写进请求记录供诊断（§24.1）。
    let mut filtered: Vec<(String, usize)> = Vec::new();
    for reason in &reasons {
        let label = reason.describe();
        match filtered.iter_mut().find(|(name, _)| *name == label) {
            Some((_, count)) => *count += 1,
            None => filtered.push((label, 1)),
        }
    }
    Ok(Plan {
        layers: into_layers(candidates, random),
        cheapest,
        dearest,
        filtered,
    })
}

/// 内置能力目录是否明确说这个目标缺本次请求需要的能力（§16.6）。
///
/// **只在该能力没有任何真实证据时才看目录**——证据优先级是
/// `明确的真实请求结果 > 上游接口返回 > 内置适配规则 > 开源目录`，
/// 有了更高优先级的证据，目录的意见就作废。
fn catalog_discouraged(
    context: &Context<'_>,
    target: &TargetView,
    requested: &[&'static str],
) -> bool {
    let catalog = crate::capability::builtin();
    requested.iter().any(|capability| {
        // 已经学到证据的能力不归目录管：支持或不支持都由证据说话。
        let learned = context.capabilities.is_unsupported(
            &target.account.id,
            &target.target.upstream_model,
            capability,
            context.now,
        );
        !learned && catalog.supports(&target.target.upstream_model, capability) == Some(false)
    })
}

/// 把候选按有效优先级切成层，层内用 `score^k` 加权随机排序（§9.2、§9.5）。
///
/// 层内还有一道**硬**分界，按 §14.8 与 §16.6：能无损表达且目录没有异议的目标
/// 排最前；能无损表达但目录存疑的排中间；只能降级表达的排最后。这就是
/// "降级只在故障切换时生效"——层内的无损目标全部试完之前，需要丢弃 thinking
/// 的目标根本轮不到。
fn into_layers(mut candidates: Vec<Candidate>, random: &mut impl FnMut() -> f64) -> Vec<Layer> {
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.target.priority));

    let mut layers: Vec<Layer> = Vec::new();
    for candidate in candidates {
        let priority = candidate.target.priority;
        match layers.last_mut() {
            Some(layer) if layer.priority == priority => layer.candidates.push(candidate),
            _ => layers.push(Layer {
                priority,
                candidates: vec![candidate],
            }),
        }
    }

    for layer in &mut layers {
        // 按偏好档分成三段：目录无异议的无损目标、目录存疑的无损目标、需要降级的
        // 目标（§14.8、§16.6）。档与档之间是硬顺序，档内才抽签。
        let mut buckets: [Vec<Candidate>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for candidate in layer.candidates.drain(..) {
            buckets[candidate.preference() as usize].push(candidate);
        }
        layer.candidates = buckets
            .into_iter()
            .flat_map(|bucket| shuffle(bucket, random))
            .collect();
    }
    layers
}

/// 按综合评分加权随机排出一组候选的尝试顺序。
fn shuffle(candidates: Vec<Candidate>, random: &mut impl FnMut() -> f64) -> Vec<Candidate> {
    let scores: Vec<score::Score> = candidates.iter().map(|c| c.score).collect();
    let order = score::weighted_order(&scores, random);
    let mut slots: Vec<Option<Candidate>> = candidates.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|index| slots.get_mut(index).and_then(Option::take))
        .collect()
}

/// 全部目标都不合格时，挑一个最值得报出的原因。
fn failure(model_name: &str, group: &GroupView, reasons: &[Ineligible]) -> SelectionFailure {
    // 倍率相关的原因最值得单独报出——它是"你的钱包在拦你"，客户端重试没有
    // 意义，必须映射到不可重试的状态码（§18.3）。
    let all =
        |predicate: fn(&Ineligible) -> bool| !reasons.is_empty() && reasons.iter().all(predicate);
    let code = if all(|r| *r == Ineligible::MultiplierExceeded) {
        ErrorCode::MultiplierExceeded
    } else if all(|r| {
        matches!(
            r,
            Ineligible::MultiplierUnknown | Ineligible::MultiplierExceeded
        )
    }) {
        ErrorCode::MultiplierUnknown
    } else if all(|r| matches!(r, Ineligible::Unsupported(_))) {
        // 没有任何目标能表达这个请求：重试不会有别的结果，快速失败（§18.3）。
        ErrorCode::UnsupportedParameter
    } else {
        // 混合原因下优先报出可重试的那一个：其他目标只是暂时不可用。
        reasons
            .iter()
            .find(|r| !matches!(r, Ineligible::Unsupported(_)))
            .or(reasons.first())
            .map(Ineligible::error_code)
            .unwrap_or(ErrorCode::NoEligibleTarget)
    };

    let unsupported = reasons.iter().find_map(|reason| match reason {
        Ineligible::Unsupported(message) => Some(message.clone()),
        _ => None,
    });

    SelectionFailure {
        code,
        message: match code {
            ErrorCode::MultiplierExceeded => format!(
                "逻辑模型 {model_name} 的所有目标有效倍率都高于分组上限 {}",
                group.group.multiplier_limit
            ),
            ErrorCode::MultiplierUnknown => {
                format!("逻辑模型 {model_name} 的所有目标倍率未知且已超过宽限期")
            }
            ErrorCode::UnsupportedParameter => {
                unsupported.unwrap_or_else(|| format!("逻辑模型 {model_name} 无法表达本次请求"))
            }
            _ => format!("逻辑模型 {model_name} 当前没有可用的调度目标"),
        },
    }
}

/// 粘性绑定是否还能继续使用（§10.2）。
///
/// "忙"不属于失效条件：并发满或收到带 `Retry-After` 的 429 时按 §10.3 等待，
/// 不立即换号。
pub fn sticky_still_valid(group: &GroupView, target: &TargetView, context: &Context<'_>) -> bool {
    match check_eligibility(group, target, context) {
        Ok(_) => true,
        Err(reason) => reason.is_queueable(),
    }
}
/// 排队等待的上限：不超过请求总超时的剩余时间。
pub fn clamp_wait(budget: Duration, remaining: Duration) -> Duration {
    budget.min(remaining)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use time::OffsetDateTime;

    use super::*;
    use crate::config::LogicalModelView;
    use crate::domain::{
        Account, DispatchTarget, Group, Limits, LogicalModel, ModelOrigin, MultiplierMode,
        SchedulingWeights, UpstreamType,
    };

    fn account(id: &str, multiplier: &str, protocol: Protocol, enabled: bool) -> Arc<Account> {
        Arc::new(Account {
            id: id.into(),
            group_id: "g1".into(),
            name: id.into(),
            upstream_type: UpstreamType::OpenAiCompatible,
            base_url: "https://api.example.com".into(),
            preferred_protocol: protocol,
            adaptive_protocol: true,
            default_priority: 50,
            calibration: Multiplier::ONE,
            multiplier_mode: MultiplierMode::Manual,
            manual_multiplier: Multiplier::parse(multiplier).unwrap(),
            new_api_user_id: None,
            new_api_group: None,
            limits: Limits::default(),
            allow_private_network: false,
            enabled,
            hide_original: false,
            auto_sync: false,
            model_synced_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        })
    }

    /// 一个关掉运行时适配的账号：只用自己的首选端点，不去猜别的（§14.2）。
    fn pinned(id: &str, multiplier: &str, protocol: Protocol) -> Arc<Account> {
        let mut account = (*account(id, multiplier, protocol, true)).clone();
        account.adaptive_protocol = false;
        Arc::new(account)
    }

    fn target(id: &str, account: Arc<Account>, priority: i32) -> Arc<TargetView> {
        Arc::new(TargetView {
            target: DispatchTarget {
                id: id.into(),
                logical_model_id: "m1".into(),
                account_id: account.id.clone(),
                upstream_model: "glm-4.6".into(),
                hide_original: false,
                priority_override: Some(priority),
                limits: Limits::default(),
                enabled: true,
                created_at: OffsetDateTime::UNIX_EPOCH,
            },
            account,
            priority,
        })
    }

    /// 指定上游模型名，用来驱动内置能力目录的判断（§16.6）。
    ///
    /// 逻辑模型名与上游真名是两件事：目录按**上游真名**查，所以测目录行为
    /// 必须能控制这个名字。
    fn target_of_model(
        id: &str,
        account: Arc<Account>,
        priority: i32,
        upstream_model: &str,
    ) -> Arc<TargetView> {
        let base = target(id, account, priority);
        Arc::new(TargetView {
            target: DispatchTarget {
                upstream_model: upstream_model.into(),
                ..base.target.clone()
            },
            account: Arc::clone(&base.account),
            priority,
        })
    }

    fn group_with(limit: &str, targets: Vec<Arc<TargetView>>) -> GroupView {
        let mut sorted = targets;
        sorted.sort_by_key(|t| std::cmp::Reverse(t.priority));
        let mut models = HashMap::new();
        models.insert(
            "glm-4.6".to_string(),
            Arc::new(LogicalModelView {
                model: LogicalModel {
                    id: "m1".into(),
                    group_id: "g1".into(),
                    name: "glm-4.6".into(),
                    origin: ModelOrigin::Auto,
                    enabled: true,
                    created_at: OffsetDateTime::UNIX_EPOCH,
                },
                targets: sorted,
                exposed: true,
                aliases: Vec::new(),
            }),
        );
        GroupView {
            group: Group {
                id: "g1".into(),
                name: "主力".into(),
                key_prefix: "akh-000000".into(),
                key_digest_hex: "d1".into(),
                multiplier_limit: Multiplier::parse(limit).unwrap(),
                weights: SchedulingWeights::default(),
                queue_capacity: 100,
                max_wait_secs: 60,
                allow_managed_background: false,
                allow_degrade: true,
                created_at: OffsetDateTime::UNIX_EPOCH,
            },
            models,
        }
    }

    struct Fixture {
        health: health::Registry,
        perf: score::Registry,
        multipliers: multiplier::Registry,
        evidence: Evidence,
        capabilities: capability::Capabilities,
        /// 一个普通的 Chat 请求体，三个协议都能无损表达。
        body: serde_json::Value,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                health: health::Registry::new(),
                perf: score::Registry::new(),
                multipliers: multiplier::Registry::new(),
                evidence: Evidence::new(),
                capabilities: capability::Capabilities::new(),
                body: serde_json::json!({
                    "model": "glm-4.6",
                    "messages": [{"role": "user", "content": "hi"}],
                }),
            }
        }

        fn context(&self) -> (multiplier::View, Protocol) {
            (self.multipliers.view(), Protocol::OpenAiChat)
        }

        fn translation(&self) -> Translation<'_> {
            Translation::new(Protocol::OpenAiChat, &self.body)
        }
    }

    /// 固定随机源，让层内顺序在断言里可复现。
    fn fixed(value: f64) -> impl FnMut() -> f64 {
        move || value
    }

    macro_rules! context {
        ($fixture:expr, $view:expr, $translation:expr) => {
            Context {
                health: &$fixture.health,
                perf: &$fixture.perf,
                multipliers: &$view,
                evidence: &$fixture.evidence,
                capabilities: &$fixture.capabilities,
                translation: &$translation,
                endpoint: Endpoint::ChatCompletions,
                allow_degrade: true,
                protocol: Protocol::OpenAiChat,
                streaming: false,
                now_unix: 0,
                now: std::time::Instant::now(),
            }
        };
    }

    #[test]
    fn equal_priorities_form_one_layer_and_different_ones_do_not() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();
        let group = group_with(
            "1",
            vec![
                target("a", account("a1", "0.5", Protocol::OpenAiChat, true), 60),
                target("b", account("a2", "0.5", Protocol::OpenAiChat, true), 60),
                target("c", account("a3", "0.5", Protocol::OpenAiChat, true), 20),
            ],
        );
        let plan = plan(
            &group,
            "glm-4.6",
            &context!(fixture, view, translation),
            &mut fixed(0.5),
        )
        .unwrap();

        assert_eq!(plan.layers.len(), 2);
        assert_eq!(plan.layers[0].priority, 60);
        assert_eq!(plan.layers[0].candidates.len(), 2);
        assert_eq!(plan.layers[1].priority, 20);
    }

    #[test]
    fn the_ladder_is_strict_even_when_the_lower_layer_scores_higher() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();
        // 第 2 层又便宜又快，但第 1 层只要还有合格目标就绝不下沉（§9.2）。
        let group = group_with(
            "1",
            vec![
                target(
                    "expensive",
                    account("a1", "0.9", Protocol::OpenAiChat, true),
                    100,
                ),
                target(
                    "cheap",
                    account("a2", "0.05", Protocol::OpenAiChat, true),
                    60,
                ),
            ],
        );
        let plan = plan(
            &group,
            "glm-4.6",
            &context!(fixture, view, translation),
            &mut fixed(0.5),
        )
        .unwrap();
        let order: Vec<_> = plan
            .attempts()
            .iter()
            .map(|c| c.target.target.id.clone())
            .collect();
        assert_eq!(order, vec!["expensive", "cheap"]);
        // 分数确实是低层更高——这正是"系统中不存在让自适应越过优先级的机制"。
        assert!(
            plan.layers[1].candidates[0].score.total > plan.layers[0].candidates[0].score.total
        );
    }

    #[test]
    fn a_multiplier_equal_to_the_limit_is_allowed_but_above_is_not() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();

        let allowed = group_with(
            "0.5",
            vec![target(
                "t1",
                account("a1", "0.5", Protocol::OpenAiChat, true),
                50,
            )],
        );
        assert_eq!(
            plan(
                &allowed,
                "glm-4.6",
                &context!(fixture, view, translation),
                &mut fixed(0.5)
            )
            .unwrap()
            .attempts()
            .len(),
            1
        );

        let refused = group_with(
            "0.4",
            vec![target(
                "t1",
                account("a1", "0.5", Protocol::OpenAiChat, true),
                50,
            )],
        );
        let failure = plan(
            &refused,
            "glm-4.6",
            &context!(fixture, view, translation),
            &mut fixed(0.5),
        )
        .unwrap_err();
        assert_eq!(failure.code, ErrorCode::MultiplierExceeded);
        assert!(!failure.code.is_retryable());
    }

    #[test]
    fn disabled_accounts_are_skipped_but_others_still_serve() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();
        let group = group_with(
            "1",
            vec![
                target(
                    "disabled",
                    account("a1", "0.1", Protocol::OpenAiChat, false),
                    100,
                ),
                target("live", account("a2", "0.2", Protocol::OpenAiChat, true), 50),
            ],
        );
        let plan = plan(
            &group,
            "glm-4.6",
            &context!(fixture, view, translation),
            &mut fixed(0.5),
        )
        .unwrap();
        assert_eq!(plan.attempts().len(), 1);
        assert_eq!(plan.attempts()[0].target.target.id, "live");
    }

    #[test]
    fn a_different_upstream_protocol_is_now_a_candidate_not_a_rejection() {
        // 阶段 3 之前协议不匹配是硬性不合格，故障切换在跨协议场景下形同虚设。
        // 现在只要能表达，它就是合格候选（§9.1 的修订说明）。
        let fixture = Fixture::new();
        let view = fixture.multipliers.view();
        let body = serde_json::json!({
            "model": "glm-4.6", "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let translation = Translation::new(Protocol::AnthropicMessages, &body);
        let group = group_with(
            "1",
            vec![target(
                "t1",
                account("a1", "0.1", Protocol::OpenAiChat, true),
                50,
            )],
        );
        let context = Context {
            health: &fixture.health,
            perf: &fixture.perf,
            multipliers: &view,
            evidence: &fixture.evidence,
            capabilities: &fixture.capabilities,
            translation: &translation,
            endpoint: Endpoint::Messages,
            allow_degrade: true,
            protocol: Protocol::AnthropicMessages,
            streaming: false,
            now_unix: 0,
            now: std::time::Instant::now(),
        };
        let native = plan(&group, "glm-4.6", &context, &mut fixed(0.5)).unwrap();
        let candidate = &native.attempts()[0];
        assert!(candidate.is_lossless(), "纯文本请求跨协议无损");
        // 原生端点未被证实缺失时先走它：上游很可能两个端点都有（§14.2）。
        assert_eq!(candidate.endpoints[0].endpoint, Endpoint::Messages);

        // 证实上游没有 /v1/messages 之后，同一个账号改走转换后的 Chat 端点。
        fixture
            .evidence
            .note_unsupported("a1", Endpoint::Messages, context.now);
        let converted = plan(&group, "glm-4.6", &context, &mut fixed(0.5)).unwrap();
        assert_eq!(
            converted.attempts()[0].endpoints[0].endpoint,
            Endpoint::ChatCompletions
        );
    }

    #[test]
    fn a_request_no_target_can_express_fails_fast() {
        // `n: 3` 在 Messages 与 Responses 都表达不了，也不在降级白名单内。
        let fixture = Fixture::new();
        let view = fixture.multipliers.view();
        let body = serde_json::json!({"model": "glm-4.6", "messages": [], "n": 3});
        let translation = Translation::new(Protocol::OpenAiChat, &body);
        let group = group_with(
            "1",
            vec![target(
                "t1",
                pinned("a1", "0.1", Protocol::AnthropicMessages),
                50,
            )],
        );
        let context = Context {
            health: &fixture.health,
            perf: &fixture.perf,
            multipliers: &view,
            evidence: &fixture.evidence,
            capabilities: &fixture.capabilities,
            translation: &translation,
            endpoint: Endpoint::ChatCompletions,
            allow_degrade: true,
            protocol: Protocol::OpenAiChat,
            streaming: false,
            now_unix: 0,
            now: std::time::Instant::now(),
        };
        let failure = plan(&group, "glm-4.6", &context, &mut fixed(0.5)).unwrap_err();
        assert_eq!(failure.code, ErrorCode::UnsupportedParameter);
        assert!(!failure.code.is_retryable(), "换个目标也是同样结果");
    }

    #[test]
    fn lossless_targets_are_exhausted_before_degraded_ones_in_the_same_layer() {
        // 带签名思考历史：转到 Chat 必然丢思考，转到 Messages 无损。
        let fixture = Fixture::new();
        let view = fixture.multipliers.view();
        let body = serde_json::json!({
            "model": "glm-4.6", "max_tokens": 16,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "推理", "signature": "sig"},
                {"type": "text", "text": "答案"}
            ]}]
        });
        let translation = Translation::new(Protocol::AnthropicMessages, &body);
        // 会降级的那个又便宜又该赢下抽签，但它仍然必须排在无损目标之后。
        let group = group_with(
            "1",
            vec![
                target(
                    "cheap-but-degrades",
                    pinned("a1", "0.05", Protocol::OpenAiChat),
                    50,
                ),
                target(
                    "lossless",
                    pinned("a2", "0.9", Protocol::AnthropicMessages),
                    50,
                ),
            ],
        );
        let make = |allow_degrade| Context {
            health: &fixture.health,
            perf: &fixture.perf,
            multipliers: &view,
            evidence: &fixture.evidence,
            capabilities: &fixture.capabilities,
            translation: &translation,
            endpoint: Endpoint::Messages,
            allow_degrade,
            protocol: Protocol::AnthropicMessages,
            streaming: false,
            now_unix: 0,
            now: std::time::Instant::now(),
        };

        let lossless_first = plan(&group, "glm-4.6", &make(true), &mut fixed(0.5)).unwrap();
        let order: Vec<_> = lossless_first
            .attempts()
            .iter()
            .map(|c| c.target.target.id.as_str())
            .collect();
        assert_eq!(
            order,
            vec!["lossless", "cheap-but-degrades"],
            "降级只在故障切换时生效：无损目标没试完之前轮不到它（§14.8）"
        );
        assert!(lossless_first.layers[0].candidates[0].is_lossless());

        // 分组关掉降级开关：只能降级表达的目标直接出局。
        let strict = plan(&group, "glm-4.6", &make(false), &mut fixed(0.5)).unwrap();
        assert_eq!(strict.attempts().len(), 1);
        assert_eq!(strict.attempts()[0].target.target.id, "lossless");
    }

    #[test]
    fn unknown_models_report_model_not_found() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();
        let group = group_with("1", vec![]);
        let failure = plan(
            &group,
            "不存在的模型",
            &context!(fixture, view, translation),
            &mut fixed(0.5),
        )
        .unwrap_err();
        assert_eq!(failure.code, ErrorCode::ModelNotFound);
    }

    #[test]
    fn a_model_with_zero_targets_is_not_eligible() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();
        let group = group_with("1", vec![]);
        let failure = plan(
            &group,
            "glm-4.6",
            &context!(fixture, view, translation),
            &mut fixed(0.5),
        )
        .unwrap_err();
        assert_eq!(failure.code, ErrorCode::NoEligibleTarget);
    }

    #[test]
    fn a_cooling_target_leaves_the_plan_but_a_busy_one_stays() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();
        let group = group_with(
            "1",
            vec![
                target(
                    "cooling",
                    account("a1", "0.5", Protocol::OpenAiChat, true),
                    50,
                ),
                target("busy", account("a2", "0.5", Protocol::OpenAiChat, true), 50),
            ],
        );

        // 熔断：真正的"坏"，退出候选。
        for _ in 0..5 {
            fixture
                .health
                .try_admit("a1", "cooling", Limits::default(), 0)
                .unwrap()
                .settle(health::Outcome::Fault, None);
        }
        // 并发满：只是"忙"，仍留在计划里等排队（§13.6）。
        let busy_limits = Limits {
            max_concurrency: Some(1),
            ..Limits::default()
        };
        let _held = fixture
            .health
            .try_admit("a2", "busy", busy_limits, 0)
            .unwrap();

        let mut group = group;
        if let Some(model) = group.models.get_mut("glm-4.6")
            && let Some(model) = Arc::get_mut(model)
        {
            let busy = Arc::get_mut(&mut model.targets[1]).unwrap();
            busy.target.limits = busy_limits;
        }

        let plan = plan(
            &group,
            "glm-4.6",
            &context!(fixture, view, translation),
            &mut fixed(0.5),
        )
        .unwrap();
        let ids: Vec<_> = plan
            .attempts()
            .iter()
            .map(|c| c.target.target.id.clone())
            .collect();
        assert_eq!(ids, vec!["busy"]);
    }

    #[test]
    fn an_unknown_multiplier_hard_stops_the_target() {
        let fixture = Fixture::new();
        let auto = Account {
            multiplier_mode: MultiplierMode::Sub2Api,
            ..(*account("a1", "1", Protocol::OpenAiChat, true)).clone()
        };
        // 余量为零的账号刷新失败即刻硬停（§11.4）。
        fixture
            .multipliers
            .seed(std::slice::from_ref(&auto), &[], 0);
        let view = fixture.multipliers.view();

        let group = group_with("1", vec![target("t1", Arc::new(auto), 50)]);
        let translation = fixture.translation();
        let context = Context {
            health: &fixture.health,
            perf: &fixture.perf,
            multipliers: &view,
            evidence: &fixture.evidence,
            capabilities: &fixture.capabilities,
            translation: &translation,
            endpoint: Endpoint::ChatCompletions,
            allow_degrade: true,
            protocol: Protocol::OpenAiChat,
            streaming: false,
            now_unix: 1,
            now: std::time::Instant::now(),
        };
        let failure = plan(&group, "glm-4.6", &context, &mut fixed(0.5)).unwrap_err();
        assert_eq!(failure.code, ErrorCode::MultiplierUnknown);
        assert!(!failure.code.is_retryable());
    }

    #[test]
    fn the_attempt_plan_is_capped_for_runaway_protection() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();
        let targets: Vec<_> = (0..12)
            .map(|i| {
                target(
                    &format!("t{i}"),
                    account(&format!("a{i}"), "0.5", Protocol::OpenAiChat, true),
                    50,
                )
            })
            .collect();
        let group = group_with("1", targets);
        let plan = plan(
            &group,
            "glm-4.6",
            &context!(fixture, view, translation),
            &mut fixed(0.5),
        )
        .unwrap();
        assert_eq!(plan.attempts().len(), MAX_TARGET_ATTEMPTS);
        assert_eq!(plan.layers[0].candidates.len(), 12, "候选本身不被截断");
    }

    #[test]
    fn a_busy_binding_is_kept_but_a_broken_one_is_dropped() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();
        let busy_limits = Limits {
            max_concurrency: Some(1),
            ..Limits::default()
        };
        let mut busy = target("busy", account("a1", "0.5", Protocol::OpenAiChat, true), 50);
        Arc::get_mut(&mut busy).unwrap().target.limits = busy_limits;
        let cooling = target(
            "cooling",
            account("a2", "0.5", Protocol::OpenAiChat, true),
            50,
        );
        let group = group_with("1", vec![Arc::clone(&busy), Arc::clone(&cooling)]);

        let _held = fixture
            .health
            .try_admit("a1", "busy", busy_limits, 0)
            .unwrap();
        for _ in 0..5 {
            fixture
                .health
                .try_admit("a2", "cooling", Limits::default(), 0)
                .unwrap()
                .settle(health::Outcome::Fault, None);
        }

        let context = context!(fixture, view, translation);
        assert!(
            sticky_still_valid(&group, &busy, &context),
            "「忙」不该导致换号，应当按等待预算排队"
        );
        assert!(
            !sticky_still_valid(&group, &cooling, &context),
            "熔断必须清除绑定并重新选择"
        );
    }

    #[test]
    fn the_cost_benchmark_covers_the_full_eligible_range() {
        let fixture = Fixture::new();
        let (view, _) = fixture.context();
        let translation = fixture.translation();
        let group = group_with(
            "1",
            vec![
                target(
                    "cheap",
                    account("a1", "0.1", Protocol::OpenAiChat, true),
                    50,
                ),
                target("mid", account("a2", "0.5", Protocol::OpenAiChat, true), 50),
                target("dear", account("a3", "0.9", Protocol::OpenAiChat, true), 50),
            ],
        );
        let plan = plan(
            &group,
            "glm-4.6",
            &context!(fixture, view, translation),
            &mut fixed(0.5),
        )
        .unwrap();
        assert_eq!(plan.cheapest, Some(Multiplier::parse("0.1").unwrap()));
        assert_eq!(plan.dearest, Some(Multiplier::parse("0.9").unwrap()));
    }

    #[test]
    fn waiting_never_outlives_the_request_deadline() {
        assert_eq!(
            clamp_wait(Duration::from_secs(60), Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            clamp_wait(Duration::from_secs(6), Duration::from_secs(600)),
            Duration::from_secs(6)
        );
    }

    /// 目录明确说不支持某项能力时，该目标在同一层内排到最后（§16.6）。
    ///
    /// 这是目录接入路由决策的可见效果：它**不**把目标踢出计划（目录可能过时，
    /// 真实请求结果才是最高优先级的证据），只是让别的候选先被尝试。
    #[test]
    fn a_catalog_objection_moves_a_target_to_the_back_of_its_layer() {
        let fixture = Fixture::new();
        let view = fixture.multipliers.view();
        // 带图片的请求：目录里 gpt-3.5-turbo 明确不标 vision。
        let body = serde_json::json!({
            "model": "glm-4.6",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}]
        });
        let translation = Translation::new(Protocol::OpenAiChat, &body);
        let group = group_with(
            "1",
            vec![
                // 目录里 gpt-3.5-turbo 明确不标 vision，gpt-4o 标了。
                target_of_model(
                    "discouraged",
                    account("a1", "0.1", Protocol::OpenAiChat, true),
                    50,
                    "gpt-3.5-turbo",
                ),
                target_of_model(
                    "capable",
                    account("a2", "0.1", Protocol::OpenAiChat, true),
                    50,
                    "gpt-4o",
                ),
            ],
        );
        let context = Context {
            health: &fixture.health,
            perf: &fixture.perf,
            multipliers: &view,
            evidence: &fixture.evidence,
            capabilities: &fixture.capabilities,
            translation: &translation,
            endpoint: Endpoint::ChatCompletions,
            allow_degrade: true,
            protocol: Protocol::OpenAiChat,
            streaming: false,
            now_unix: 0,
            now: std::time::Instant::now(),
        };
        let planned = plan(&group, "glm-4.6", &context, &mut fixed(0.5)).unwrap();
        // 两个候选都还在计划里——目录不构成硬性不合格。
        assert_eq!(planned.attempts().len(), 2, "目录不该把目标踢出计划");
        // 两个都在第一层。
        assert_eq!(planned.layers.len(), 1, "同优先级应当同层");
        let layer = &planned.layers[0];
        // 目录无异议的 gpt-4o 必须排在目录存疑的 gpt-3.5-turbo 之前。
        assert_eq!(
            layer.candidates[0].target.target.upstream_model, "gpt-4o",
            "目录无异议的无损目标应当先被尝试"
        );
        assert!(
            !layer.candidates[0].catalog_discouraged && layer.candidates[1].catalog_discouraged,
            "档位应当反映目录的意见"
        );
    }

    /// 有了真实证据就不再听目录的（§16.6 的证据优先级）。
    #[test]
    fn learned_evidence_outranks_the_catalog() {
        let fixture = Fixture::new();
        let view = fixture.multipliers.view();
        let body = serde_json::json!({
            "model": "glm-4.6",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}]
        });
        let translation = Translation::new(Protocol::OpenAiChat, &body);
        let group = group_with(
            "1",
            vec![target(
                "t1",
                account("a1", "0.1", Protocol::OpenAiChat, true),
                50,
            )],
        );
        let now = std::time::Instant::now();
        // 假设这个模型被真实请求证实"支持 vision"——目录就算说不行也不算数。
        // 这里用"学到的是别的能力"来间接表达：学到证据的能力不归目录管。
        fixture
            .capabilities
            .note_unsupported("a1", "glm-4.6", "reasoning", now);
        let context = Context {
            health: &fixture.health,
            perf: &fixture.perf,
            multipliers: &view,
            evidence: &fixture.evidence,
            capabilities: &fixture.capabilities,
            translation: &translation,
            endpoint: Endpoint::ChatCompletions,
            allow_degrade: true,
            protocol: Protocol::OpenAiChat,
            streaming: false,
            now_unix: 0,
            now,
        };
        let planned = plan(&group, "glm-4.6", &context, &mut fixed(0.5)).unwrap();
        // 图片请求仍然只有一个候选，且没有因为目录被降权之外的影响。
        assert_eq!(planned.attempts().len(), 1);
    }
}
