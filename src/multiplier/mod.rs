//! 倍率状态、宽限期与自动刷新（§11）。
//!
//! 倍率是**动态安全状态**：它不跟随配置版本，每次真正发请求前都要重新读取
//! （§21）。所以这里维护一份与 [`crate::config::RuntimeConfig`] 平行的表，用
//! `ArcSwap` 整体替换——刷新每 5 分钟一次，读取每个请求一次，写少读多。

pub mod probe;
pub mod refresh;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use arc_swap::ArcSwap;

use crate::domain::{Account, Multiplier, MultiplierMode};
use crate::storage::store::MultiplierSnapshotRow;

/// 按风险余量决定的宽限期（§11.4）。
const GRACE_LOW_RISK: i64 = 60 * 60;
const GRACE_MEDIUM_RISK: i64 = 15 * 60;
/// 风险余量的两个分界：最后已知有效倍率占分组上限的比例。
const LOW_RISK_RATIO: f64 = 0.60;
const MEDIUM_RISK_RATIO: f64 = 0.90;
/// 判定为"探针侧系统性故障"后统一使用的宽限期。
const SYSTEMIC_GRACE: i64 = 60 * 60;

/// 倍率的可用状态（§12.2 中与倍率相关的三种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 值是新鲜的，或来源是手动。
    Known,
    /// 刷新失败但仍在宽限期内：可用，层内评分降权，后台告警。
    Stale,
    /// 宽限期已结束：硬暂停。
    Unknown,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Known => "known",
            Self::Stale => "multiplier_stale",
            Self::Unknown => "multiplier_unknown",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "multiplier_stale" => Self::Stale,
            "multiplier_unknown" => Self::Unknown,
            _ => Self::Known,
        }
    }

    /// 宽限期内的目标仍参与调度，但要在层内评分中降权（§11.4）。
    pub fn is_usable(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// 一个账号的倍率状态。
#[derive(Debug, Clone)]
pub struct Entry {
    /// 最后已知的**上游**倍率，尚未乘校准系数。
    pub upstream: Multiplier,
    pub source: MultiplierMode,
    /// 上游声明的观察时间。
    pub observed_at: Option<i64>,
    /// Akhub 最后一次成功刷新的时间。
    pub refreshed_at: i64,
    /// 进入 `Stale` 的时刻；`None` 表示当前不是 `Stale`。
    pub stale_since: Option<i64>,
    /// 最近一次刷新失败的原因，供后台展示。绝不包含凭据。
    pub last_error: Option<String>,
    /// 峰值时段。存在时由本地换算当前倍率（§11.3）。
    pub peak: Option<probe::PeakSchedule>,
}

impl Entry {
    /// 手动来源的初值：状态永远是"已知"。
    fn manual(account: &Account, now: i64) -> Self {
        Self {
            upstream: account.manual_multiplier,
            source: MultiplierMode::Manual,
            observed_at: None,
            refreshed_at: now,
            stale_since: None,
            last_error: None,
            peak: None,
        }
    }

    /// 自动来源尚未成功刷新过时的初值：用手填值顶着，并立刻开始计宽限期。
    fn pending(account: &Account, now: i64) -> Self {
        Self {
            upstream: account.manual_multiplier,
            source: account.multiplier_mode,
            observed_at: None,
            refreshed_at: now,
            stale_since: Some(now),
            last_error: Some("尚未完成首次自动刷新".into()),
            peak: None,
        }
    }

    /// 此刻的上游倍率，已考虑峰值时段。
    fn upstream_now(&self, now: i64) -> Multiplier {
        match &self.peak {
            Some(peak) if peak.covers(now) => peak.multiplier,
            _ => self.upstream,
        }
    }
}

/// 一个账号在此刻的有效倍率与状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    /// `上游倍率 × 校准系数`，定点乘法向上取整（§20.3）。
    pub value: Multiplier,
    pub status: Status,
    /// 已过期多久（秒）。仅在 `Stale` 时有值，供后台显示"倍率已过期 N 分钟"。
    pub stale_for: Option<i64>,
}

/// 倍率表的一份只读快照。
#[derive(Clone)]
pub struct View {
    entries: Arc<HashMap<String, Entry>>,
    /// 系统性故障保护的生效截止时间（Unix 秒），0 表示未生效。
    systemic_until: i64,
}

impl View {
    /// 算出账号此刻的有效倍率与状态。
    ///
    /// `limit` 是该账号所属分组的倍率上限，宽限期长度由"最后已知值占上限的
    /// 比例"决定：余量越薄，越不能拿旧数据冒险（§11.4）。
    pub fn effective(&self, account: &Account, limit: Multiplier, now: i64) -> Effective {
        // 手动倍率不受自动刷新失败影响，状态始终视为已知（§11.2）。
        if account.multiplier_mode == MultiplierMode::Manual {
            return Effective {
                value: account.configured_effective_multiplier(),
                status: Status::Known,
                stale_for: None,
            };
        }

        match self.entries.get(&account.id) {
            Some(entry) => self.evaluate(entry, account, limit, now),
            // 表里还没有这个账号（刚创建、尚未进入刷新轮次）：按待刷新处理。
            None => self.evaluate(&Entry::pending(account, now), account, limit, now),
        }
    }

    fn evaluate(&self, entry: &Entry, account: &Account, limit: Multiplier, now: i64) -> Effective {
        let value = entry.upstream_now(now).mul_ceil(account.calibration);
        let Some(since) = entry.stale_since else {
            return Effective {
                value,
                status: Status::Known,
                stale_for: None,
            };
        };

        let grace = if now < self.systemic_until {
            // 超过半数账号同时失败时，判定为探针侧故障而非上游集体改价：
            // 统一延长宽限期，防止 Akhub 因为自己这边的网络问题把自己打死。
            SYSTEMIC_GRACE
        } else {
            grace_for(value, limit)
        };
        let stale_for = (now - since).max(0);
        Effective {
            value,
            status: if stale_for >= grace {
                Status::Unknown
            } else {
                Status::Stale
            },
            stale_for: Some(stale_for),
        }
    }

    /// 探针侧系统性故障是否正在生效，供概览页红色告警。
    pub fn systemic_failure(&self, now: i64) -> bool {
        now < self.systemic_until
    }

    /// 遍历全部条目，供后台展示。
    pub fn entries(&self) -> impl Iterator<Item = (&String, &Entry)> {
        self.entries.iter()
    }
}

/// 按风险余量决定宽限期长度（§11.4）。
///
/// 刷新成功时每个请求用的也是最多 5 分钟前的旧值，失败后同一个值的信息量
/// 完全相同，不该突然变成绝对不可用；但余量越薄，赌错的代价越大。
fn grace_for(effective: Multiplier, limit: Multiplier) -> i64 {
    if limit <= Multiplier::ZERO {
        return 0;
    }
    let ratio = effective.to_f64() / limit.to_f64();
    if ratio <= LOW_RISK_RATIO {
        GRACE_LOW_RISK
    } else if ratio <= MEDIUM_RISK_RATIO {
        GRACE_MEDIUM_RISK
    } else {
        // 余量不足 10%：任何一点上调都会越过红线，只能立即硬停。
        0
    }
}

/// 全进程的倍率状态表。
pub struct Registry {
    current: ArcSwap<HashMap<String, Entry>>,
    systemic_until: AtomicI64,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            current: ArcSwap::from_pointee(HashMap::new()),
            systemic_until: AtomicI64::new(0),
        }
    }

    /// 热路径读取：一次原子指针读取（§19.4）。
    pub fn view(&self) -> View {
        View {
            entries: self.current.load_full(),
            systemic_until: self.systemic_until.load(Ordering::Acquire),
        }
    }

    /// 用数据库中的持久化状态与当前账号列表重建整表。
    ///
    /// 账号被删除或改回手动来源时，对应条目一并消失——留着一条永远不会再被
    /// 刷新的旧记录只会让后台显示假状态。
    pub fn seed(&self, accounts: &[Account], snapshots: &[MultiplierSnapshotRow], now: i64) {
        let persisted: HashMap<&str, &MultiplierSnapshotRow> = snapshots
            .iter()
            .map(|row| (row.account_id.as_str(), row))
            .collect();

        let mut entries = HashMap::with_capacity(accounts.len());
        for account in accounts {
            let entry = if account.multiplier_mode == MultiplierMode::Manual {
                Entry::manual(account, now)
            } else {
                match persisted.get(account.id.as_str()) {
                    // 来源改过之后旧快照不再可信，按未刷新处理。
                    Some(row) if row.source == account.multiplier_mode => Entry {
                        upstream: row.multiplier,
                        source: row.source,
                        observed_at: row.observed_at,
                        refreshed_at: row.refreshed_at,
                        stale_since: row.stale_since.or_else(|| {
                            // 重启后无法确认这个值还新不新，按"从重启那一刻
                            // 开始过期"处理，而不是当作刚刷新过。
                            (Status::parse(&row.status) != Status::Known).then_some(now)
                        }),
                        last_error: row.last_error.clone(),
                        peak: None,
                    },
                    _ => Entry::pending(account, now),
                }
            };
            entries.insert(account.id.clone(), entry);
        }
        self.current.store(Arc::new(entries));
    }

    /// 覆盖单个账号的状态。刷新任务每探完一个账号就调用一次。
    fn put(&self, account_id: &str, entry: Entry) {
        // 整表 clone-on-write：账号数量是几十的量级，5 分钟一次的复制可忽略，
        // 换来的是读侧完全无锁。
        let mut next = (*self.current.load_full()).clone();
        next.insert(account_id.to_string(), entry);
        self.current.store(Arc::new(next));
    }

    fn get(&self, account_id: &str) -> Option<Entry> {
        self.current.load().get(account_id).cloned()
    }

    /// 开启或续期"探针侧系统性故障"保护。
    fn set_systemic_failure(&self, until: i64) {
        self.systemic_until.fetch_max(until, Ordering::AcqRel);
    }

    fn clear_systemic_failure(&self) {
        self.systemic_until.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    use crate::domain::{Limits, Protocol, UpstreamType};

    fn multiplier(raw: &str) -> Multiplier {
        Multiplier::parse(raw).unwrap()
    }

    fn account(mode: MultiplierMode, manual: &str) -> Account {
        Account {
            id: "acc".into(),
            group_id: "g1".into(),
            name: "账号A".into(),
            upstream_type: UpstreamType::OpenAiCompatible,
            base_url: "https://api.example.com".into(),
            preferred_protocol: Protocol::OpenAiChat,
            adaptive_protocol: true,
            default_priority: 50,
            calibration: Multiplier::ONE,
            multiplier_mode: mode,
            manual_multiplier: multiplier(manual),
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

    fn view_with(entry: Option<(&str, Entry)>, systemic_until: i64) -> View {
        let mut entries = HashMap::new();
        if let Some((id, entry)) = entry {
            entries.insert(id.to_string(), entry);
        }
        View {
            entries: Arc::new(entries),
            systemic_until,
        }
    }

    fn stale_entry(upstream: &str, since: i64) -> Entry {
        Entry {
            upstream: multiplier(upstream),
            source: MultiplierMode::Sub2Api,
            observed_at: Some(since),
            refreshed_at: since,
            stale_since: Some(since),
            last_error: Some("上游超时".into()),
            peak: None,
        }
    }

    #[test]
    fn manual_multipliers_ignore_refresh_failures_entirely() {
        // 表里甚至没有这个账号，手动来源也必须是"已知"（§11.2）。
        let view = view_with(None, 0);
        let effective = view.effective(&account(MultiplierMode::Manual, "0.5"), Multiplier::ONE, 0);
        assert_eq!(effective.status, Status::Known);
        assert_eq!(effective.value, multiplier("0.5"));
    }

    #[test]
    fn the_grace_period_shrinks_as_the_risk_margin_thins() {
        let limit = Multiplier::ONE;
        // ≤60% 上限 → 60 分钟
        assert_eq!(grace_for(multiplier("0.6"), limit), GRACE_LOW_RISK);
        // 60%–90% → 15 分钟
        assert_eq!(grace_for(multiplier("0.9"), limit), GRACE_MEDIUM_RISK);
        // >90% → 立即硬停
        assert_eq!(grace_for(multiplier("0.95"), limit), 0);
    }

    #[test]
    fn a_stale_multiplier_stays_usable_until_the_grace_period_ends() {
        let account = account(MultiplierMode::Sub2Api, "1");
        let view = view_with(Some(("acc", stale_entry("0.5", 0))), 0);

        // 余量充足：59 分钟后仍可用，只是降权。
        let inside = view.effective(&account, Multiplier::ONE, 59 * 60);
        assert_eq!(inside.status, Status::Stale);
        assert!(inside.status.is_usable());
        assert_eq!(inside.stale_for, Some(59 * 60));

        // 宽限期结束 → 硬停。
        let outside = view.effective(&account, Multiplier::ONE, 61 * 60);
        assert_eq!(outside.status, Status::Unknown);
        assert!(!outside.status.is_usable());
    }

    #[test]
    fn a_thin_margin_hard_stops_the_moment_the_refresh_fails() {
        let account = account(MultiplierMode::Sub2Api, "1");
        // 最后已知值就是上限本身：任何上调都会越线，没有宽限余地。
        let view = view_with(Some(("acc", stale_entry("1", 0))), 0);
        assert_eq!(
            view.effective(&account, Multiplier::ONE, 1).status,
            Status::Unknown
        );
    }

    #[test]
    fn systemic_probe_failure_extends_every_grace_period() {
        let account = account(MultiplierMode::Sub2Api, "1");
        let entry = stale_entry("1", 0);
        // 同一个薄余量账号，在系统性故障保护下改为统一的 60 分钟宽限。
        assert_eq!(
            view_with(Some(("acc", entry.clone())), 0)
                .effective(&account, Multiplier::ONE, 60)
                .status,
            Status::Unknown
        );
        let protected = view_with(Some(("acc", entry)), SYSTEMIC_GRACE);
        assert_eq!(
            protected.effective(&account, Multiplier::ONE, 60).status,
            Status::Stale,
            "探针自己坏了不该把整个网关打死"
        );
        assert!(protected.systemic_failure(60));
    }

    #[test]
    fn calibration_participates_in_the_effective_multiplier() {
        let mut account = account(MultiplierMode::Sub2Api, "1");
        account.calibration = multiplier("0.7");
        let view = view_with(
            Some((
                "acc",
                Entry {
                    stale_since: None,
                    ..stale_entry("0.5", 0)
                },
            )),
            0,
        );
        let effective = view.effective(&account, Multiplier::ONE, 0);
        assert_eq!(effective.status, Status::Known);
        // 0.5 × 0.7 = 0.35，定点乘法向上取整。
        assert_eq!(effective.value, multiplier("0.35"));
    }

    #[test]
    fn a_fresh_automatic_account_starts_its_grace_period_immediately() {
        let registry = Registry::new();
        let account = account(MultiplierMode::NewApi, "0.5");
        registry.seed(std::slice::from_ref(&account), &[], 1_000);

        let view = registry.view();
        let effective = view.effective(&account, Multiplier::ONE, 1_000);
        assert_eq!(effective.status, Status::Stale);
        assert_eq!(effective.value, multiplier("0.5"), "先用手填值顶着");

        // 一直探不到就在宽限期后硬停，而不是永远用手填值假装正常。
        assert_eq!(
            view.effective(&account, Multiplier::ONE, 1_000 + GRACE_LOW_RISK)
                .status,
            Status::Unknown
        );
    }

    #[test]
    fn a_snapshot_from_a_different_source_is_not_trusted_after_a_restart() {
        let registry = Registry::new();
        let account = account(MultiplierMode::NewApi, "0.9");
        // 管理员把来源从 Sub2API 改成了 New API：旧快照不再代表真实倍率。
        registry.seed(
            std::slice::from_ref(&account),
            &[MultiplierSnapshotRow {
                account_id: "acc".into(),
                multiplier: multiplier("0.1"),
                source: MultiplierMode::Sub2Api,
                status: Status::Known.as_str().into(),
                observed_at: Some(0),
                refreshed_at: 0,
                stale_since: None,
                last_error: None,
            }],
            1_000,
        );
        let effective = registry.view().effective(&account, Multiplier::ONE, 1_000);
        assert_eq!(effective.value, multiplier("0.9"));
        assert_eq!(effective.status, Status::Stale);
    }

    #[test]
    fn removing_an_account_drops_its_entry() {
        let registry = Registry::new();
        registry.seed(&[account(MultiplierMode::Sub2Api, "0.5")], &[], 0);
        assert_eq!(registry.view().entries.len(), 1);
        registry.seed(&[], &[], 0);
        assert_eq!(registry.view().entries.len(), 0);
    }
}
