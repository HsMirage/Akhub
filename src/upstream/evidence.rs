//! 端点成功记录（§14.3）与"哪个思考档位在这个上游真的能用"（§14.6）。
//!
//! 404/405 可能来自模型、凭据或具体路径，不能证明整个账号没有这个接口。
//! 拒绝的端点只在本次请求内排除；这里只记录真实成功，24 小时后回到未知。
//!
//! 同一个上游站会按域名给出不同的能力：一个域名收全套档位，另一个域名只收
//! `low`，其余档位回同一句 "requires adaptive thinking" 的 400。档位是
//! **请求**的字段，能否接受却是**这个账号上的这个模型**的属性，所以按
//! （账号，上游模型）记下"哪些档位试过能用、哪些被拒过"。
//!
//! **记的是事实，不是上限**：调度与发射绝不因此把客户端要的档位调低。
//! 被拒时的处置见 [`EffortFit::plan`]——升档优于不指定，不指定优于降档，
//! 而"降档"这条路根本不存在（§14.6）。

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::protocol::canonical::Effort;
use crate::upstream::Endpoint;

pub const TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
pub struct Evidence {
    supported: RwLock<HashMap<(String, Endpoint), Instant>>,
    /// （账号，上游模型）→ 这个组合上的档位实测结果。
    ///
    /// 粒度是**账号内的模型**，不是账号：同一个账号上 opus-5-5 可能只收 low，
    /// 而 sonnet-5 收全套；记在账号上会顺手改掉别的模型的发射形状。
    ///
    /// 过期是刻意的：中转站升级后不该被永久当成"只收 low"。TTL 一到就回到
    /// 未知，重新按客户端要的档位乐观发射。
    effort_fit: RwLock<HashMap<(String, String), EffortFit>>,
}

/// 某个（账号，上游模型）上档位的实测结果。
#[derive(Debug, Clone, Copy)]
struct EffortFit {
    /// 事实本身（可拷贝，能安全地跨 `await` 传递）。
    facts: EffortFacts,
    /// "这些档位可用"的有效期。
    accepted_expires: Instant,
    /// "这些档位被拒"的有效期。
    ///
    /// 与可用事实**分开计时**是必须的：一个账号上正常流量一直在用别的档位，
    /// 如果每次成功都给整条记录续期，一条拒绝就永远不过期了——中转站后来放宽
    /// 了也发现不了（§14.6）。
    rejected_expires: Instant,
}

impl EffortFit {
    /// 一条从 `now` 起算的新记录。
    fn new(now: Instant) -> Self {
        Self {
            facts: EffortFacts::default(),
            accepted_expires: now + TTL,
            rejected_expires: now + TTL,
        }
    }

    /// 按 `now` 过滤掉已经过期的事实。
    fn live(&self, now: Instant) -> EffortFacts {
        EffortFacts {
            accepted: if self.accepted_expires > now {
                self.facts.accepted
            } else {
                0
            },
            rejected: if self.rejected_expires > now {
                self.facts.rejected
            } else {
                0
            },
        }
    }
}

/// 客户端要的档位在这个（账号，上游模型）上该怎么发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortPlan {
    /// 按这个档位发射（等于客户端要的那一档，或者更高的一档）。
    Send(Effort),
    /// 不发射 `output_config`，把档位交回上游模型自己的默认。
    ///
    /// 这是"客户端要的那一档不被接受、又没有更高的档可升"时的处置。**它不是
    /// 降档**：网关没有替客户端选一个更弱的档位，思考仍然按上游自己的默认强度
    /// 发生。发一个更低的档位才是降智，那条路不存在（§14.6）。
    Omit,
}

/// 某个（账号，上游模型）上档位实测结果的**只读快照**。
///
/// 快照而不是句柄：发射发生在异步路径上，闭包要跨 `await` 保持 `Send`，
/// 而 `RwLock` 守卫不能跨 `await`。快照是 `Copy` 的，且语义上更对——
/// 一次尝试从头到尾该按同一份事实决定发什么。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffortFacts {
    /// 试过、上游接受过的档位（位图，见 [`level_bit`]）。
    accepted: u8,
    /// 试过、上游拒过的档位（位图）。
    rejected: u8,
}

/// 档位在位图里的位。
///
/// 六个档位恰好塞进一个 `u8`，于是整份事实是 `Copy` 的，可以按值跨
/// `await` 传，也不必为它分配。
fn level_bit(level: Effort) -> u8 {
    match level {
        Effort::Minimal => 1 << 0,
        Effort::Low => 1 << 1,
        Effort::Medium => 1 << 2,
        Effort::High => 1 << 3,
        Effort::XHigh => 1 << 4,
        Effort::Max => 1 << 5,
    }
}

/// 从低到高遍历所有档位。
const LEVELS: [Effort; 6] = [
    Effort::Minimal,
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
    Effort::Max,
];

impl EffortFacts {
    /// 记下「这一档被上游接受过」。
    ///
    /// 两张位图**互斥**：最近的观测说了算。历史成功不能给一个后来被拒的档位
    /// 免死金牌——否则 plan() 会反复选中「已确认可用」的它，在同一档位上打转
    /// （2026-10-15 现场：high → xhigh → max → xhigh，最后把上游的 400 透传）。
    fn accept(&mut self, level: Effort) {
        self.accepted |= level_bit(level);
        self.rejected &= !level_bit(level);
    }

    /// 记下「这一档被上游拒过」。互斥口径见 accept()。
    fn reject(&mut self, level: Effort) {
        self.rejected |= level_bit(level);
        self.accepted &= !level_bit(level);
    }

    fn is_rejected(&self, level: Effort) -> bool {
        self.rejected & level_bit(level) != 0
    }

    fn is_accepted(&self, level: Effort) -> bool {
        self.accepted & level_bit(level) != 0
    }

    /// 比 `requested` 更高、且满足 `wanted` 的**最低**档位。
    ///
    /// 取最低而不是最高：升档是为了绕开一个不被接受的档位，不是为了让客户端
    /// 多花钱。能一档够用就不上两档（§14.6）。
    fn lowest_above(
        &self,
        requested: Effort,
        wanted: impl Fn(&Self, Effort) -> bool,
    ) -> Option<Effort> {
        LEVELS
            .into_iter()
            .find(|level| *level > requested && wanted(self, *level))
    }

    /// 客户端要 `requested` 时该怎么发。
    ///
    /// 优先级是刻意的：**升到已确认可用的更高档 > 往上试探一档 > 不指定**。
    /// **降档这条路不存在**——发一个更低的档位等于替客户端降智（§14.6）。
    pub fn plan(&self, requested: Effort) -> EffortPlan {
        if !self.is_rejected(requested) {
            // 没被拒过就按客户端要的发；被接受过当然也走这里。
            return EffortPlan::Send(requested);
        }
        // 这一档被拒过。先看有没有**更高**的、已经证明可用的档位——那是确定
        // 能成功的升档，也是这里最好的结果。
        if let Some(higher) = self.lowest_above(requested, |facts, level| facts.is_accepted(level))
        {
            return EffortPlan::Send(higher);
        }
        // 没有已知可用的更高档，但更高的档位里还有没试过的：往上试**最近的
        // 那一档**。
        //
        // 中转站实现的档位**不单调**（现场：一个站收 low 却拒 medium），所以
        // "这一档不行"推不出"更高的也不行"。一次只上一档：试中了就是真的升档，
        // 客户端拿到的比它要的还强；试不中会被记下来，下一次继续往上挪。
        if let Some(next) = self.lowest_above(requested, |facts, level| !facts.is_rejected(level)) {
            return EffortPlan::Send(next);
        }
        // 更高的档位都试过且都被拒：不指定，交回上游自己的默认。
        EffortPlan::Omit
    }

    /// 单元测试用：直接造一条"这一档被拒过"的事实。
    #[cfg(test)]
    pub(crate) fn reject_for_test(&mut self, level: Effort) {
        self.reject(level);
    }

    /// 单元测试用：直接造一条"这一档可用"的事实。
    #[cfg(test)]
    pub(crate) fn accept_for_test(&mut self, level: Effort) {
        self.accept(level);
    }
}

impl Evidence {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只记录真正成功的调用，不能将收到 HTTP 响应头当成成功。
    pub fn note_supported(&self, account_id: &str, endpoint: Endpoint, now: Instant) {
        if let Ok(mut map) = self.supported.write() {
            map.insert((account_id.to_string(), endpoint), now + TTL);
        }
    }

    pub fn is_supported(&self, account_id: &str, endpoint: Endpoint, now: Instant) -> bool {
        self.supported
            .read()
            .ok()
            .and_then(|map| map.get(&(account_id.to_string(), endpoint)).copied())
            .is_some_and(|expires| expires > now)
    }

    /// 这个（账号，上游模型）上的档位实测结果快照。
    ///
    /// 没有任何记录时返回全空的快照，它的 `plan` 会按客户端要的档位发射：
    /// **默认相信客户端**，被拒了再学（§14.6）。
    pub fn effort_facts(
        &self,
        account_id: &str,
        upstream_model: &str,
        now: Instant,
    ) -> EffortFacts {
        self.effort_fit
            .read()
            .ok()
            .and_then(|map| {
                map.get(&(account_id.to_string(), upstream_model.to_string()))
                    .copied()
            })
            .map(|fit| fit.live(now))
            .unwrap_or_default()
    }

    /// 记下"这个（账号，上游模型）接受了这个档位"。
    ///
    /// 每次成功都刷新期限：还在正常工作的组合不该因为一次旧的拒绝过期就丢掉
    /// 已确认的事实。
    pub fn note_effort_accepted(
        &self,
        account_id: &str,
        upstream_model: &str,
        effort: Effort,
        now: Instant,
    ) {
        let key = (account_id.to_string(), upstream_model.to_string());
        if let Ok(mut map) = self.effort_fit.write() {
            let fit = map.entry(key).or_insert_with(|| EffortFit::new(now));
            // 逐档记下，不只留最高：升档要挑**够用的最低档**，只留最高会把
            // 一个"要 low"的请求抬到 max，白白多花钱。
            fit.facts.accept(effort);
            // 只续"可用事实"的期：这条成功不构成任何关于被拒档位的新证据，
            // 顺手续期会让一条拒绝在忙账号上永远不过期。
            fit.accepted_expires = now + TTL;
        }
    }

    /// 记下"这个（账号，上游模型）拒绝了这一档"。
    ///
    /// 只记事实，不推导任何"上限"：上限是把"这一档不行"误读成"更高的档也不行"
    /// 才需要的东西，而那个推理没有依据——现场就是一个站收 low 却拒 medium，
    /// 单调假设不成立。发射时按 [`EffortFit::plan`] 用事实现算（§14.6）。
    pub fn note_effort_rejected(
        &self,
        account_id: &str,
        upstream_model: &str,
        rejected: Effort,
        now: Instant,
    ) {
        let key = (account_id.to_string(), upstream_model.to_string());
        if let Ok(mut map) = self.effort_fit.write() {
            let fit = map.entry(key).or_insert_with(|| EffortFit::new(now));
            fit.facts.reject(rejected);
            // 同理只续"被拒事实"的期。
            fit.rejected_expires = now + TTL;
        }
    }

    /// 配置变更后旧的成功记录与档位实测结果都不再参与决策。
    pub fn clear(&self) {
        if let Ok(mut map) = self.supported.write() {
            map.clear();
        }
        if let Ok(mut map) = self.effort_fit.write() {
            map.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::canonical::Effort;

    fn plan(evidence: &Evidence, requested: Effort, now: Instant) -> EffortPlan {
        evidence.effort_facts("a", "m", now).plan(requested)
    }

    #[test]
    fn nothing_is_learned_yet_so_the_client_gets_what_it_asked_for() {
        let evidence = Evidence::new();
        let now = Instant::now();
        for requested in [Effort::Minimal, Effort::Low, Effort::High, Effort::Max] {
            assert_eq!(
                plan(&evidence, requested, now),
                EffortPlan::Send(requested),
                "没有证据时按客户端要的档位乐观发射"
            );
        }
    }

    #[test]
    fn a_rejected_level_is_never_replaced_by_a_lower_one() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_effort_rejected("a", "m", Effort::Medium, now);

        // 被拒的那一档：往上试一档——**不是**降到 low。
        assert_eq!(
            plan(&evidence, Effort::Medium, now),
            EffortPlan::Send(Effort::High)
        );
        // 没被拒过的档位照常发，包括比它更低的：降智的决定权不在网关。
        assert_eq!(
            plan(&evidence, Effort::Low, now),
            EffortPlan::Send(Effort::Low)
        );
        assert_eq!(
            plan(&evidence, Effort::Minimal, now),
            EffortPlan::Send(Effort::Minimal)
        );
    }

    #[test]
    fn a_level_accepted_then_rejected_is_no_longer_treated_as_known_good() {
        let evidence = Evidence::new();
        let now = Instant::now();
        // 先成功过一次，后来又被拒：最近的观测说了算，它不再是「已确认可用」。
        // 2026-10-15 现场：并发的那条 200 把 xhigh 写成 accepted，正在重试的这条
        // 立刻把 xhigh 又发了一遍——而 xhigh 上一秒刚被拒。
        evidence.note_effort_rejected("a", "m", Effort::High, now);
        evidence.note_effort_accepted("a", "m", Effort::XHigh, now);
        evidence.note_effort_rejected("a", "m", Effort::XHigh, now);
        assert_eq!(
            plan(&evidence, Effort::High, now),
            EffortPlan::Send(Effort::Max),
            "被拒过的档位不能再以「已确认可用」的身份被发出去"
        );
        // 反过来：先拒后成，最近一次是成功，就回到「可用」。
        let evidence = Evidence::new();
        evidence.note_effort_rejected("a", "m", Effort::High, now);
        evidence.note_effort_rejected("a", "m", Effort::XHigh, now);
        evidence.note_effort_accepted("a", "m", Effort::XHigh, now);
        assert_eq!(
            plan(&evidence, Effort::High, now),
            EffortPlan::Send(Effort::XHigh)
        );
    }

    #[test]
    fn the_highest_level_falls_back_to_omitting_rather_than_lowering() {
        let evidence = Evidence::new();
        let now = Instant::now();
        // max 已经最高：没有更高的档可试，只能不指定。
        evidence.note_effort_rejected("a", "m", Effort::Max, now);
        assert_eq!(plan(&evidence, Effort::Max, now), EffortPlan::Omit);
    }

    #[test]
    fn the_upgrade_uses_the_cheapest_level_that_is_known_to_work() {
        let evidence = Evidence::new();
        let now = Instant::now();
        // 这个账号上 medium 与 max 都被证明可用，low 被拒。
        evidence.note_effort_accepted("a", "m", Effort::Medium, now);
        evidence.note_effort_accepted("a", "m", Effort::Max, now);
        evidence.note_effort_rejected("a", "m", Effort::Low, now);
        // 升到刚好够用的 medium，不是一路抬到 max：升档是为了绕开一个不被
        // 接受的档位，不是为了让客户端多花钱。
        assert_eq!(
            plan(&evidence, Effort::Low, now),
            EffortPlan::Send(Effort::Medium)
        );
    }

    #[test]
    fn upward_probing_stops_once_every_higher_level_is_known_bad() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_effort_rejected("a", "m", Effort::Low, now);
        // 第一次：往上一档试 medium。
        assert_eq!(
            plan(&evidence, Effort::Low, now),
            EffortPlan::Send(Effort::Medium)
        );
        // medium 也被拒：再往上一档试 high。
        evidence.note_effort_rejected("a", "m", Effort::Medium, now);
        assert_eq!(
            plan(&evidence, Effort::Low, now),
            EffortPlan::Send(Effort::High)
        );
        // high 也被拒：继续往上挪到 xhigh（还没试过）。
        evidence.note_effort_rejected("a", "m", Effort::High, now);
        assert_eq!(
            plan(&evidence, Effort::Low, now),
            EffortPlan::Send(Effort::XHigh)
        );
        // xhigh 也被拒：只剩 max 没试过。
        evidence.note_effort_rejected("a", "m", Effort::XHigh, now);
        assert_eq!(
            plan(&evidence, Effort::Low, now),
            EffortPlan::Send(Effort::Max)
        );
        // max 也被拒：每一档更高的都试过且都被拒，这才改不指定。
        evidence.note_effort_rejected("a", "m", Effort::Max, now);
        assert_eq!(plan(&evidence, Effort::Low, now), EffortPlan::Omit);
    }

    #[test]
    fn a_known_good_higher_level_is_preferred_over_probing_further() {
        let evidence = Evidence::new();
        let now = Instant::now();
        // 现场形态：站收 low 与 high，拒 medium。
        evidence.note_effort_rejected("a", "m", Effort::Medium, now);
        evidence.note_effort_accepted("a", "m", Effort::Low, now);
        evidence.note_effort_accepted("a", "m", Effort::High, now);

        // 要 medium 被拒：升到已确认可用的 high（确定成功），而不是往上乱试。
        assert_eq!(
            plan(&evidence, Effort::Medium, now),
            EffortPlan::Send(Effort::High),
            "已确认可用的升档优于继续试探"
        );
        // 要 high 没被拒：原样发。
        assert_eq!(
            plan(&evidence, Effort::High, now),
            EffortPlan::Send(Effort::High)
        );
    }

    #[test]
    fn effort_facts_are_scoped_to_one_model_on_one_account() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_effort_rejected("a", "claude-opus-5-5", Effort::Medium, now);

        // 同一个账号的别的模型不受牵连：照客户端要的发，不往上试探。
        assert_eq!(
            evidence
                .effort_facts("a", "claude-sonnet-5", now)
                .plan(Effort::Medium),
            EffortPlan::Send(Effort::Medium)
        );
        // 别的账号的同一个模型也不受牵连。
        assert_eq!(
            evidence
                .effort_facts("b", "claude-opus-5-5", now)
                .plan(Effort::Medium),
            EffortPlan::Send(Effort::Medium)
        );
        // 只有被拒的那一对改行为。
        assert_eq!(
            evidence
                .effort_facts("a", "claude-opus-5-5", now)
                .plan(Effort::Medium),
            EffortPlan::Send(Effort::High)
        );
    }

    #[test]
    fn effort_facts_expire_back_to_optimistic() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_effort_rejected("a", "m", Effort::Medium, now);
        assert_eq!(
            plan(&evidence, Effort::Medium, now + TTL / 2),
            EffortPlan::Send(Effort::High)
        );
        // TTL 一到就重新乐观：中转站升级后不该被永久当成"只收 low"。
        assert_eq!(
            plan(&evidence, Effort::Medium, now + TTL),
            EffortPlan::Send(Effort::Medium)
        );
    }

    #[test]
    fn a_success_on_another_level_does_not_keep_a_rejection_alive() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_effort_rejected("a", "m", Effort::Medium, now);

        // 这个账号上正常流量一直在用 low，每次都成功。如果成功给整条记录续期，
        // 那条拒绝就永远不过期——中转站后来放宽了也发现不了。
        for hour in 1..=10 {
            evidence.note_effort_accepted(
                "a",
                "m",
                Effort::Low,
                now + Duration::from_secs(hour * 3_600),
            );
        }

        assert_eq!(
            plan(
                &evidence,
                Effort::Medium,
                now + TTL + Duration::from_secs(60)
            ),
            EffortPlan::Send(Effort::Medium),
            "拒绝按自己的期限过期，不被别的档位的成功续命"
        );
    }

    #[test]
    fn a_rejection_does_not_keep_a_success_alive() {
        let evidence = Evidence::new();
        let now = Instant::now();
        // 这个账号上 max 被证明可用；high 被拒。
        evidence.note_effort_accepted("a", "m", Effort::Max, now);
        for hour in 1..=10 {
            evidence.note_effort_rejected(
                "a",
                "m",
                Effort::High,
                now + Duration::from_secs(hour * 3_600),
            );
        }

        // 期限内：走"升到已确认可用的 max"。
        assert_eq!(
            plan(&evidence, Effort::High, now + Duration::from_secs(3_600)),
            EffortPlan::Send(Effort::Max),
            "期限内用已确认可用的档位"
        );
        // max 的可用事实按自己的期限过期（high 的拒绝还在期限内）：不再拿它
        // 当确定的目标，改回逐档试探。
        assert_eq!(
            plan(&evidence, Effort::High, now + TTL + Duration::from_secs(60)),
            EffortPlan::Send(Effort::XHigh),
            "可用事实过期后不再被当作已知目标"
        );
    }

    #[test]
    fn a_configuration_change_forgets_the_effort_facts() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_effort_rejected("a", "m", Effort::Medium, now);
        evidence.clear();
        assert_eq!(
            plan(&evidence, Effort::Medium, now),
            EffortPlan::Send(Effort::Medium)
        );
    }

    #[test]
    fn success_is_scoped_to_one_account_and_endpoint() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_supported("a", Endpoint::Responses, now);
        assert!(evidence.is_supported("a", Endpoint::Responses, now));
        assert!(!evidence.is_supported("a", Endpoint::Messages, now));
        assert!(!evidence.is_supported("b", Endpoint::Responses, now));
    }

    #[test]
    fn success_expires_back_to_unknown() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_supported("a", Endpoint::Responses, now);
        assert!(evidence.is_supported("a", Endpoint::Responses, now + TTL / 2));
        assert!(!evidence.is_supported("a", Endpoint::Responses, now + TTL));
    }

    #[test]
    fn configuration_changes_clear_success_records() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_supported("a", Endpoint::Responses, now);
        evidence.clear();
        assert!(!evidence.is_supported("a", Endpoint::Responses, now));
    }
}
