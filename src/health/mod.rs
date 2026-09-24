//! 熔断、冷却、半开、并发与限流（§12、§17.1）。
//!
//! 这里的状态是**动态安全状态**：它不跟随配置版本，每次真正发请求前都要重新
//! 读取（§21）。所有计数用原子值或极短临界区的互斥量，不存在全局大锁（§19.4）。
//!
//! 时间统一使用 [`tokio::time::Instant`]，测试因此可以用 `tokio::time::pause`
//! 精确推进冷却与限流窗口，而不必真的睡上几分钟。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::domain::Limits;

/// RPM / TPM 的统计窗口长度。
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// 熔断判定的观察窗口：§12.3 的"60 秒内连续或高比例"。
const FAULT_WINDOW: Duration = Duration::from_secs(60);
/// 连续失败到这个数就直接冷却，不必等比例条件成立。
const CONSECUTIVE_TRIP: u32 = 5;
/// 比例条件生效所需的最小样本，避免 1/1 失败就熔断。
const RATIO_MIN_SAMPLES: f64 = 10.0;
/// 窗口内失败比例达到这个值即冷却。
const RATIO_TRIP: f64 = 0.5;
/// 冷却基础时长，按级别指数增长。
const COOLDOWN_BASE: Duration = Duration::from_secs(5);
/// 冷却上限。再长就该由管理员处理，而不是让目标无限期消失。
const COOLDOWN_CAP: Duration = Duration::from_secs(300);
/// 「上游说凭据不对」要连续确认几次才真正硬停这把 Key（§12.3）。
///
/// 上游对 403 的用法很杂：分组被停用、权限不足、WAF 拦截与凭据无效都回 403，
/// 只看状态码分不出来。**任何**失效判定都必须能被一次真实成功推翻，否则就会
/// 自锁——这把 Key 已经被排除在抽签之外，那个"成功"永远不会到来。计数让偶发
/// 的一次误判只触发本次换 Key，账号不会被永久钉死。
const KEY_INVALID_CONFIRMATIONS: u32 = 2;
/// 硬停的自证窗口：过了这么久允许放行一次真实请求证明自己（§12.3 的半开）。
///
/// §12.3 写的是"修改凭据或手动测试成功后恢复"，两条都是**人工**动作；而上游
/// 把分组恢复、把 WAF 规则撤掉这类事没有任何人会去点一下。没有这条自动恢复
/// 路径，面板会一直显示一个已经不存在的故障（"Key 失效，但 Key 是好的"）。
const KEY_INVALID_PROBE_AFTER: Duration = Duration::from_secs(600);
/// 未配置最大并发时使用的"事实上不限"额度。
///
/// 用一个很大的常数而不是 `Option<Semaphore>`：上限可以被管理员随时改成有限
/// 值，统一走同一条准入路径才不会出现两套语义。
const UNLIMITED: u32 = 1 << 20;

/// 账号、Key 与目标三级的预算，必须同时满足。
///
/// 三级是**逐级收紧**的关系：账号限额是所有 Key 共享的总闸门，Key 覆盖在它
/// 之内再收紧，目标覆盖再收紧一次（§4.2.1、§17.1）。
#[derive(Debug, Clone, Copy, Default)]
pub struct AdmissionLimits {
    pub account: Limits,
    /// Key 级覆盖。单 Key 账号与老库迁移后都是默认值（无覆盖）。
    pub key: Limits,
    pub target: Limits,
}

impl From<Limits> for AdmissionLimits {
    /// 单个 [\`Limits\`] 会被当作**有效预算**：账号与 Key 两级都不再单独收紧，
    /// 目标级承担全部限制。
    ///
    /// 这条语义必须保住：探针、后台测试与单 Key 路径都只提供一个 Limits，把它们
    /// 折叠进 Key 级会让账号总额度悄悄失效（§4.2.1、§17.1）。
    fn from(effective: Limits) -> Self {
        Self {
            account: Limits::default(),
            key: Limits::default(),
            target: effective,
        }
    }
}

/// 这次准入是谁发起的：账号、账号内的哪把 Key、以及目标。
///
/// Key 用 `Option` 表达"这次调用不绑定具体凭据"（探针、后台测试等），
/// 那时 Key 级状态完全不参与——这正是单 Key 账号行为的零成本兼容路径。
#[derive(Debug, Clone, Copy, Default)]
pub struct Caller<'a> {
    pub account_id: &'a str,
    /// 账号内的哪把 Key，填**凭据摘要**（不是行 ID）。
    ///
    /// 摘要由 [\`crate::credential::credential_id\`] 与账号一起拼成动态状态表
    /// 的归类键。用摘要而不是行 ID：换标签、重新粘贴同一把 Key 都不该丢掉
    /// 熔断与额度状态（§4.2.1）。
    pub key_id: Option<&'a str>,
    pub target_id: &'a str,
}

/// 一次排队要等的并发名额。
///
/// **只有配置了上限的那一级才有信号量**（`None` = 不限）：不限的级别不该成为
/// 排队的原因，也不该跟别的级别互相挤占。三级各有自己的额度，所以"某把 Key
/// 的并发满了"不会连累另一把 Key（§4.2.1）。
#[derive(Debug, Clone)]
pub struct Capacity {
    pub account: Option<Arc<Semaphore>>,
    pub key: Option<Arc<Semaphore>>,
    pub target: Option<Arc<Semaphore>>,
}

#[derive(Debug)]
pub struct CapacityPermit {
    _account: Option<OwnedSemaphorePermit>,
    _key: Option<OwnedSemaphorePermit>,
    _target: Option<OwnedSemaphorePermit>,
}

impl Capacity {
    /// 这一级是否完全不限（没有信号量）。
    pub fn is_unbounded(&self) -> bool {
        self.account.is_none() && self.key.is_none() && self.target.is_none()
    }

    fn try_acquire(self) -> Result<CapacityPermit, Unavailable> {
        // 顺序无关正确性：任何一步失败时，已拿到的名额随局部变量析构归还。
        let target = match &self.target {
            Some(target) => Some(
                Arc::clone(target)
                    .try_acquire_owned()
                    .map_err(|_| Unavailable::ConcurrencyFull)?,
            ),
            None => None,
        };
        let key = match &self.key {
            Some(key) => Some(
                Arc::clone(key)
                    .try_acquire_owned()
                    .map_err(|_| Unavailable::ConcurrencyFull)?,
            ),
            None => None,
        };
        let account = match &self.account {
            Some(account) => Some(
                Arc::clone(account)
                    .try_acquire_owned()
                    .map_err(|_| Unavailable::ConcurrencyFull)?,
            ),
            None => None,
        };
        Ok(CapacityPermit {
            _account: account,
            _key: key,
            _target: target,
        })
    }

    pub async fn acquire(self) -> Option<CapacityPermit> {
        // 不在等待繁忙目标时占住账号名额，其他模型仍可使用空闲账号容量。
        let target = match &self.target {
            Some(target) => Some(Arc::clone(target).acquire_owned().await.ok()?),
            None => None,
        };
        let key = match &self.key {
            Some(key) => Some(Arc::clone(key).acquire_owned().await.ok()?),
            None => None,
        };
        let account = match &self.account {
            Some(account) => Some(Arc::clone(account).acquire_owned().await.ok()?),
            None => None,
        };
        Some(CapacityPermit {
            _account: account,
            _key: key,
            _target: target,
        })
    }
}

/// 目标当前不可用的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// 上游明确证明凭据失效（401，或正文点名凭据的 403）。
    ///
    /// 范围是**那把 Key**而不是整个账号：账号里其他 Key 照常服务（§4.2.1）。
    /// 也不是"换凭据前一直硬停"：连续两次确认才生效，且约 10 分钟后会自动
    /// 放行一次自证——没有这条出口，被排除出抽签的 Key 永远等不到那个成功。
    KeyInvalid,
    /// 这个账号一把可用的 Key 都没有（从未配置或全部被删）。
    ///
    /// 与 `KeyInvalid` 分开：前者是"上游拒绝了这把凭据"，这里是"管理员还
    /// 没填凭据"，后台的提示文案与处置动作完全不同。
    NoKey,
    /// 额度耗尽，等待恢复时间或半开试运行。
    QuotaExhausted,
    /// 正在冷却，且当前没有空出的半开名额。
    Cooling,
    /// 并发已满。这是"忙"，不是"坏"——可以排队（§13.6）。
    ConcurrencyFull,
    /// RPM 或 TPM 已达上限。同样是"忙"。
    RateLimited,
}

impl Unavailable {
    /// 只有临时容量不足才允许排队；鉴权失败与额度耗尽绝不排队（§13.6）。
    pub fn is_queueable(self) -> bool {
        matches!(self, Self::ConcurrencyFull | Self::RateLimited)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::KeyInvalid => "key_invalid",
            Self::NoKey => "no_key",
            Self::QuotaExhausted => "quota_exhausted",
            Self::Cooling => "cooldown",
            Self::ConcurrencyFull => "concurrency_full",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// 一次尝试的结果，决定动态状态如何变化（§12.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// 上游明确证明这把凭据失效（§12.3）。连续两次确认才真正硬停。
    KeyInvalid,
    /// 明确的额度不足或账号封禁，可带恢复时间。
    QuotaExhausted {
        retry_after: Option<Duration>,
    },
    /// 429：优先影响"账号 + 模型"，尊重 `Retry-After`。
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// 连接错误、5xx 或损坏响应。孤立出现只触发本次切换。
    Fault,
    /// 下游请求本身的问题，与目标健康无关，不进任何统计。
    Neutral,
}

/// 目标对外展示的运行状态（§12.2 中的动态部分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetStatus {
    Active,
    Cooldown,
    HalfOpen,
    QuotaExhausted,
    KeyInvalid,
    NoKey,
}

impl TargetStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Cooldown => "cooldown",
            Self::HalfOpen => "half_open",
            Self::QuotaExhausted => "quota_exhausted",
            Self::KeyInvalid => "key_invalid",
            Self::NoKey => "no_key",
        }
    }
}

/// 动态状态表的内存上限（§19.4）。
///
/// 正常路径由 \`retain\` 按"当前配置里还存在的账号/目标"清理；这是兜底。
const MAX_TRACKED: usize = 5_000;

/// 所有账号与目标的动态状态。
#[derive(Default)]
pub struct Registry {
    accounts: RwLock<HashMap<String, Arc<AccountState>>>,
    /// Key 级状态，键是 [`crate::credential::credential_id`]（账号 + 凭据摘要）。
    ///
    /// 与账号、目标并列而不是嵌进账号：热路径只经过读锁拿一个 `Arc`，
    /// 不必先拿账号条目再进它的内部锁（§19.4）。
    keys: RwLock<HashMap<String, Arc<KeyState>>>,
    targets: RwLock<HashMap<String, Arc<TargetState>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取出（必要时创建）账号状态。
    pub fn account(&self, account_id: &str) -> Arc<AccountState> {
        get_or_insert(&self.accounts, account_id, AccountState::new)
    }

    /// 取出（必要时创建）Key 状态。
    pub fn key(&self, credential_id: &str) -> Arc<KeyState> {
        get_or_insert(&self.keys, credential_id, KeyState::new)
    }

    /// 取出（必要时创建）目标状态。
    pub fn target(&self, target_id: &str) -> Arc<TargetState> {
        get_or_insert(&self.targets, target_id, TargetState::new)
    }

    /// 丢弃已经不在配置里的账号、Key 与目标状态，避免内存随改配置无限增长。
    ///
    /// Key 的存活集合来自凭据快照而不是配置快照：摘要变了（换了真正的凭据）
    /// 或 Key 被删掉时，旧状态必须一起消失——**换掉一把坏 Key 就该重新开始**。
    pub fn retain(&self, live_accounts: &[String], live_targets: &[String]) {
        self.retain_with_keys(live_accounts, live_targets, None);
    }

    /// 带 Key 集合的状态清理。
    ///
    /// `live_key_ids` 为 `None` 时**不动** Key 状态：调用方手里没有凭据快照
    /// （例如只重载了配置），此时清空会把所有 Key 的熔断记错。传空的 `Some`
    /// 才表示"确实一把 Key 都没有了"。
    pub fn retain_with_keys(
        &self,
        live_accounts: &[String],
        live_targets: &[String],
        live_key_ids: Option<&[String]>,
    ) {
        if let Ok(mut accounts) = self.accounts.write() {
            accounts.retain(|id, _| live_accounts.iter().any(|live| live == id));
        }
        if let Ok(mut targets) = self.targets.write() {
            targets.retain(|id, _| live_targets.iter().any(|live| live == id));
        }
        if let Some(live) = live_key_ids
            && let Ok(mut keys) = self.keys.write()
        {
            keys.retain(|id, _| live.iter().any(|candidate| candidate == id));
        }
    }

    /// 管理员改过凭据或手动测试成功后解除硬停（§12.3）。
    ///
    /// 作用于账号的**每一把 Key**：权限改在账号页上，管理员期待的是"这个账号
    /// 现在可以再试一次"，而不是"只有第一把 Key 被放行"。
    pub fn clear_account_faults(&self, account_id: &str) {
        let prefix = format!("{account_id}:");
        let affected: Vec<Arc<KeyState>> = match self.keys.read() {
            Ok(guard) => guard
                .iter()
                .filter(|(id, _)| id.starts_with(&prefix))
                .map(|(_, state)| Arc::clone(state))
                .collect(),
            Err(_) => Vec::new(),
        };
        for state in affected {
            state.reset();
        }
        // 账号级额度熔断同样要清：New API 的站点级额度是按账号计的。
        let account = self.account(account_id);
        if let Ok(mut quota) = account.quota.lock() {
            quota.reset();
        }
    }

    /// 只清掉**某一把 Key** 的失效硬停与额度熔断（§12.3）。
    ///
    /// 与 [`Self::clear_account_faults`] 的区别是作用域：某把 Key 被上游误判、
    /// 或上游改完配置又改回来时，管理员应当能只放行这一把，而不必把同账号其他
    /// Key 的熔断状态一起抹掉（§4.2.1 的"逐把独立"）。
    pub fn clear_key_faults(&self, account_id: &str, credential_digest: &str) {
        let id = crate::credential::credential_id(account_id, credential_digest);
        self.key(&id).reset();
    }

    /// 清空全部账号、Key 与目标状态。备份恢复后调用：账号与目标可能整个换了
    /// 一批，旧的熔断与额度计数不再成立（§23.5）。
    pub fn clear_all(&self) {
        if let Ok(mut accounts) = self.accounts.write() {
            accounts.clear();
        }
        if let Ok(mut keys) = self.keys.write() {
            keys.clear();
        }
        if let Ok(mut targets) = self.targets.write() {
            targets.clear();
        }
    }
}

fn get_or_insert<T>(
    map: &RwLock<HashMap<String, Arc<T>>>,
    key: &str,
    make: impl FnOnce() -> T,
) -> Arc<T> {
    // 绝大多数请求走读锁这一条路；写锁只在目标第一次出现时短暂持有。
    if let Ok(guard) = map.read()
        && let Some(found) = guard.get(key)
    {
        return Arc::clone(found);
    }
    let mut guard = crate::sync::write(map);
    if guard.len() >= MAX_TRACKED && !guard.contains_key(key) {
        // 兜底淘汰：这些条目本该由 retain 清掉。丢掉一个已有目标的熔断状态
        // 意味着它下次会被当成"健康"重新试一次——比无界增长可接受，但要留痕。
        if let Some(victim) = guard.keys().next().cloned() {
            guard.remove(&victim);
            tracing::warn!(
                limit = MAX_TRACKED,
                evicted = %victim,
                "动态状态表达到上限，已淘汰一个条目（retain 可能漏了）"
            );
        }
    }
    Arc::clone(
        guard
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(make())),
    )
}

/// 账号级状态：只有**所有 Key 共享**的那部分（§4.2.1 的不变量 C）。
///
/// 账号总并发 / RPM / TPM 与站点级额度属于这里；凭据级的失效与额度在
/// [`KeyState`] 里。单 Key 账号的表现与改造前完全一致——那时账号与 Key
/// 的作用域恰好重合。
pub struct AccountState {
    quota: Mutex<Circuit>,
    budget: Budget,
}

impl AccountState {
    fn new() -> Self {
        Self {
            quota: Mutex::new(Circuit::default()),
            budget: Budget::new(),
        }
    }

    /// 当前空闲的账号级并发名额，供后台与诊断使用。
    pub fn available(&self) -> usize {
        self.budget.available()
    }

    /// 管理员给这个账号配的并发上限（`None` 表示不限）。
    pub fn configured(&self) -> Option<u32> {
        self.budget.configured()
    }

    /// 把账号级并发名额校准到配置值（§17.1）。
    ///
    /// 热路径上的准入会自己校准；选 Key 与排队路径需要**提前**看到真实名额，
    /// 否则"账号已经满载"会被误判成"有空位"。
    pub fn reconcile_capacity(&self, configured: Option<u32>) {
        self.budget.reconcile_capacity(configured);
    }

    /// 账号级并发额度（`None` 表示不限），供后台与诊断使用。
    pub fn concurrency_limit(&self) -> Option<u32> {
        (self.budget.capacity() != UNLIMITED).then(|| self.budget.capacity())
    }

    /// 账号级额度是否处于耗尽等待中（§12.3）。供后台的账号健康摘要使用。
    pub fn quota_exhausted(&self) -> bool {
        crate::sync::lock(&self.quota).is_cooling(Instant::now())
    }

    /// 不改变半开状态的资格检查。真正占用半开试运行名额由 `try_enter` 完成。
    fn check(&self, now: Instant) -> Result<(), Unavailable> {
        match crate::sync::lock(&self.quota).phase(now) {
            Phase::Cooling | Phase::HalfOpenTaken => Err(Unavailable::QuotaExhausted),
            Phase::Closed | Phase::HalfOpenAvailable => Ok(()),
        }
    }

    /// 真正准入时占用账号级半开试运行名额。
    fn try_enter(&self, now: Instant) -> Result<bool, Unavailable> {
        crate::sync::lock(&self.quota)
            .try_enter(now)
            .map_err(|_| Unavailable::QuotaExhausted)
    }
}

/// Key 级状态：凭据失效、凭据额度与 Key 级限额（§4.2.1）。
///
/// **401/403 与额度耗尽落在这里而不是账号上**：一把 Key 被上游封了不该让
/// 同账号的其他九把一起停摆——那会把 Key 池的全部价值抵消掉。
pub struct KeyState {
    /// 「上游说凭据不对」的连续确认次数；一次真实成功即清零（§12.3）。
    invalid_confirmations: AtomicU32,
    /// 硬停的开始时刻。存时刻而不只存一个 bool：自证窗口要按它计算，窗口到期
    /// 后放行一次真实请求，让它证明自己到底还行不行。
    invalid_since: Mutex<Option<Instant>>,
    quota: Mutex<Circuit>,
    budget: Budget,
}

impl KeyState {
    fn new() -> Self {
        Self {
            invalid_confirmations: AtomicU32::new(0),
            invalid_since: Mutex::new(None),
            quota: Mutex::new(Circuit::default()),
            budget: Budget::new(),
        }
    }

    /// 这把 Key 是否已被上游判定为失效（§12.3）。
    ///
    /// 自证窗口到期后**不再算失效**：放行一次真实请求去证明它。证明不了会在
    /// 确认计数上重新硬停，证明得了就自动恢复。
    pub fn key_invalid(&self) -> bool {
        self.invalid_at().is_some()
    }

    /// 硬停开始时刻；`None` 表示当前没有硬停（或窗口已到，等待一次试运行）。
    fn invalid_at(&self) -> Option<Instant> {
        let since = (*crate::sync::lock(&self.invalid_since))?;
        (Instant::now().saturating_duration_since(since) < KEY_INVALID_PROBE_AFTER).then_some(since)
    }

    /// 是否**曾经**被判定失效且还没被一次成功推翻（不受自证窗口影响）。
    ///
    /// 面板据此说明"这个标记是怎么来的"：窗口到期不等于问题消失，只是允许它
    /// 再试一次。
    pub fn auth_proves_invalid(&self) -> bool {
        crate::sync::lock(&self.invalid_since).is_some()
    }

    /// 管理员给这把 Key 配的并发上限（`None` 表示不限）。
    pub fn configured(&self) -> Option<u32> {
        self.budget.configured()
    }

    /// 这把 Key 的额度是否处于耗尽等待中（§12.3）。
    pub fn quota_exhausted(&self) -> bool {
        crate::sync::lock(&self.quota).is_cooling(Instant::now())
    }

    /// 当前在途请求数，供后台逐 Key 展示。
    pub fn inflight(&self) -> u32 {
        self.budget.inflight.load(Ordering::Relaxed)
    }

    /// 记一次「上游说凭据不对」（§12.3）。连续达到阈值才真正硬停。
    fn confirm_invalid(&self, now: Instant) {
        let confirmations = self
            .invalid_confirmations
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        if confirmations >= KEY_INVALID_CONFIRMATIONS {
            *crate::sync::lock(&self.invalid_since) = Some(now);
        }
    }

    /// 一次真实成功：清掉硬停与确认计数。这是 Key 从"坏"回到"好"的自动路径。
    fn note_success(&self) {
        self.invalid_confirmations.store(0, Ordering::Release);
        *crate::sync::lock(&self.invalid_since) = None;
    }

    /// 解除硬停并把额度熔断与半开占用一起复位。
    ///
    /// 管理员改过凭据、手动测试成功或显式清除标记后调用。半开标记必须一起清：
    /// 留在 `HalfOpenTaken` 上会让这把 Key 在冷却结束后仍然拒绝新请求。
    pub fn reset(&self) {
        self.note_success();
        if let Ok(mut quota) = self.quota.lock() {
            quota.reset();
        }
    }

    /// 逐 Key 的运行状态，供后台的 Key 徽标使用。
    pub fn status(&self) -> TargetStatus {
        if self.key_invalid() {
            return TargetStatus::KeyInvalid;
        }
        let now = Instant::now();
        let circuit = crate::sync::lock(&self.quota);
        match circuit.phase(now) {
            Phase::Closed => TargetStatus::Active,
            Phase::Cooling => TargetStatus::QuotaExhausted,
            Phase::HalfOpenAvailable | Phase::HalfOpenTaken => TargetStatus::HalfOpen,
        }
    }

    /// 冷却剩余秒数，供后台展示与 `Retry-After`。
    pub fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        crate::sync::lock(&self.quota).cooldown_remaining(now)
    }

    /// 先让并发名额追上配置，再做资格检查。
    ///
    /// **选 Key 的时候必须走这一条**，不能只调 [`Self::check`]：容量是在
    /// 准入路径上校准的，而选 Key 发生在准入之前，不校准就会拿着"1<<20 个名额"
    /// 的信号量做判断，把配了上限的 Key 当成永远有空（§17.1）。
    pub fn reconcile_and_check(&self, budget: Limits, now: Instant) -> Result<(), Unavailable> {
        self.budget.reconcile_capacity(budget.max_concurrency);
        self.check(budget, now)
    }

    /// 不消耗额度的资格检查（§9.1）。
    ///
    /// 只看**这一把 Key** 的额度；账号总额度由 [`TargetState::check`] 在同一轮
    /// 检查里负责，调用方必须两级都过（§4.2.1）。
    pub fn check(&self, budget: Limits, now: Instant) -> Result<(), Unavailable> {
        if self.key_invalid() {
            return Err(Unavailable::KeyInvalid);
        }
        match crate::sync::lock(&self.quota).phase(now) {
            Phase::Cooling | Phase::HalfOpenTaken => return Err(Unavailable::QuotaExhausted),
            Phase::Closed | Phase::HalfOpenAvailable => {}
        }
        self.budget.check(budget, now)
    }

    /// 真正准入时占用 Key 级半开试运行名额。
    fn try_enter(&self, now: Instant) -> Result<bool, Unavailable> {
        if self.key_invalid() {
            return Err(Unavailable::KeyInvalid);
        }
        crate::sync::lock(&self.quota)
            .try_enter(now)
            .map_err(|_| Unavailable::QuotaExhausted)
    }
}

/// 目标级状态：熔断窗口、并发额度与限流窗口。
pub struct TargetState {
    circuit: Mutex<Circuit>,
    budget: Budget,
}

struct Budget {
    permits: Arc<Semaphore>,
    /// 串行化容量重校准，避免并发请求重复增减 Semaphore 名额。
    capacity_lock: Mutex<()>,
    /// `permits` 当前的额定容量，用于在管理员改上限后校准。
    capacity: AtomicU32,
    inflight: AtomicU32,
    rpm: Mutex<SlidingWindow>,
    tpm: Mutex<SlidingWindow>,
}

impl TargetState {
    fn new() -> Self {
        Self {
            circuit: Mutex::new(Circuit::default()),
            budget: Budget::new(),
        }
    }

    /// 当前在途请求数，供后台展示。
    pub fn inflight(&self) -> u32 {
        self.budget.inflight.load(Ordering::Relaxed)
    }

    /// 管理员给这个目标配的并发上限（`None` 表示不限）。
    pub fn configured(&self) -> Option<u32> {
        self.budget.configured()
    }

    /// 把这个目标的并发名额校准到配置值（§17.1）。
    ///
    /// 准入路径会自己校准；选 Key 与排队路径需要**提前**看到真实名额，否则
    /// "这个目标已经满载"会被误判成"有空位"。
    pub fn reconcile_capacity(&self, configured: Option<u32>) {
        self.budget.reconcile_capacity(configured);
    }

    /// 用于后台展示的运行状态。
    ///
    /// 严重程度按"最影响可用性的先说"排序：凭据失效 > 凭据额度 > 账号额度 >
    /// 目标熔断（§6.9 的既有口径，只是多了一层凭据）。
    pub fn status(&self, account: &AccountState, key: Option<&KeyState>) -> TargetStatus {
        if let Some(key) = key {
            if key.key_invalid() {
                return TargetStatus::KeyInvalid;
            }
            let now = Instant::now();
            if crate::sync::lock(&key.quota).is_cooling(now) {
                return TargetStatus::QuotaExhausted;
            }
        }
        let now = Instant::now();
        if crate::sync::lock(&account.quota).is_cooling(now) {
            return TargetStatus::QuotaExhausted;
        }
        let circuit = crate::sync::lock(&self.circuit);
        match circuit.phase(now) {
            Phase::Closed => TargetStatus::Active,
            Phase::Cooling => TargetStatus::Cooldown,
            Phase::HalfOpenAvailable | Phase::HalfOpenTaken => TargetStatus::HalfOpen,
        }
    }

    /// 冷却剩余秒数，供后台展示与 `Retry-After`。
    pub fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        crate::sync::lock(&self.circuit).cooldown_remaining(now)
    }

    /// 不消耗任何额度的资格检查，用于 §9.1 的硬性过滤。
    ///
    /// 顺序是"坏不坏"优先于"忙不忙"：一个既熔断又满载的目标必须报熔断，
    /// 否则调用方会把它当成"忙"去排队等一个永远不会好的目标。
    fn check(
        &self,
        account: &AccountState,
        key: Option<&KeyState>,
        limits: AdmissionLimits,
        now: Instant,
    ) -> Result<(), Unavailable> {
        // 从粗到细：账号级熔断（站点额度）先拦，再拦凭据，最后是目标。
        // 顺序影响错误码的可读性——账号整体不可用时不该报成某把 Key 的问题。
        account.check(now)?;
        // `None` 表示这次调用不绑定具体凭据（探针、后台测试），跳过 Key 级判断。
        if let Some(key) = key {
            key.check(limits.key, now)?;
        }
        match crate::sync::lock(&self.circuit).phase(now) {
            Phase::Cooling | Phase::HalfOpenTaken => return Err(Unavailable::Cooling),
            Phase::Closed | Phase::HalfOpenAvailable => {}
        }
        account.budget.check(limits.account, now)?;
        self.budget.check(limits.target, now)
    }
}

impl Budget {
    fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(UNLIMITED as usize)),
            capacity_lock: Mutex::new(()),
            capacity: AtomicU32::new(UNLIMITED),
            inflight: AtomicU32::new(0),
            rpm: Mutex::new(SlidingWindow::new(RATE_WINDOW)),
            tpm: Mutex::new(SlidingWindow::new(RATE_WINDOW)),
        }
    }

    /// 当前空闲的并发名额。
    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }

    /// 管理员配的并发上限（`None` 表示不限）。
    pub fn configured(&self) -> Option<u32> {
        let current = self.capacity.load(Ordering::Acquire);
        (current != UNLIMITED).then_some(current)
    }

    /// 立即取一个并发名额，供测试直接验证预算的收发。
    #[cfg(test)]
    fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.permits).try_acquire_owned().ok()
    }

    fn check(&self, limits: Limits, now: Instant) -> Result<(), Unavailable> {
        if let Some(rpm) = limits.rpm
            && crate::sync::lock(&self.rpm).estimate(now) >= f64::from(rpm)
        {
            return Err(Unavailable::RateLimited);
        }
        if let Some(tpm) = limits.tpm
            && crate::sync::lock(&self.tpm).estimate(now) >= f64::from(tpm)
        {
            return Err(Unavailable::RateLimited);
        }
        if self.permits.available_permits() == 0 {
            return Err(Unavailable::ConcurrencyFull);
        }
        Ok(())
    }

    /// 额定容量，供诊断与后台展示。
    pub fn capacity(&self) -> u32 {
        self.capacity.load(Ordering::Acquire)
    }

    /// 让 Semaphore 的容量追上要用的最大并发。
    ///
    /// 缩容时只能回收当前空闲的名额，在途请求归还后由下一次准入继续回收；
    /// 这样永远不会超过旧上限，也总会收敛到新上限。
    ///
    /// \`None\` 表示**调用方对并发没有意见**（读路径与默认端口常常如此），
    /// 此时保持现状。把它当成"重置为不限"会抹掉已经校准好的上限，满载就会
    /// 被看成有空位（§13.6、§17.1）。
    fn reconcile_capacity(&self, wanted: Option<u32>) {
        let Some(wanted) = wanted else {
            return;
        };
        let wanted = wanted.max(1);
        let _guard = crate::sync::lock(&self.capacity_lock);
        let current = self.capacity.load(Ordering::Acquire);
        if wanted == current {
            return;
        }
        if wanted > current {
            self.permits.add_permits((wanted - current) as usize);
            self.capacity.store(wanted, Ordering::Release);
        } else {
            let forgotten = self.permits.forget_permits((current - wanted) as usize);
            self.capacity
                .store(current - forgotten as u32, Ordering::Release);
        }
    }

    fn cancel(&self, reservation: RateReservation, now: Instant) {
        if reservation.rpm {
            crate::sync::lock(&self.rpm).refund_at(1, reservation.at, now);
        }
        if let Some(tokens) = reservation.tokens {
            crate::sync::lock(&self.tpm).refund_at(tokens, reservation.at, now);
        }
    }

    /// 把取消时**已经拿走**的并发名额补回额定容量。
    ///
    /// [`Self::cancel`] 只退限流计数，退不了信号量：信号量名额是靠 permit 析构
    /// 归还的。于是"预扣 → 取消"这条路径会把额定容量越削越小，最终所有同级请求
    /// 都拿不到名额。取消意味着这次预扣整个作废，容量必须回到配置值（§17.1）。
    fn restore_rate(&self, limits: Limits) {
        if limits.max_concurrency.is_none() {
            return;
        }
        let wanted = limits.max_concurrency.unwrap_or(UNLIMITED).max(1);
        let _guard = crate::sync::lock(&self.capacity_lock);
        let current = self.capacity.load(Ordering::Acquire);
        if current < wanted {
            self.permits.add_permits((wanted - current) as usize);
            self.capacity.store(wanted, Ordering::Release);
        }
    }

    fn settle(&self, reservation: RateReservation, actual: Option<u64>, now: Instant) {
        if let (Some(reserved), Some(actual)) = (reservation.tokens, actual) {
            let mut tpm = crate::sync::lock(&self.tpm);
            if actual < reserved {
                tpm.refund_at(reserved - actual, reservation.at, now);
            } else {
                tpm.try_consume(actual - reserved, f64::INFINITY, now);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct RateReservation {
    at: Instant,
    rpm: bool,
    tokens: Option<u64>,
}

impl RateReservation {
    /// 未占用任何额度的空预留，便于三级扣减统一走同一套退回逻辑。
    fn none() -> Self {
        Self {
            at: Instant::now(),
            rpm: false,
            tokens: None,
        }
    }
}

/// 一次尝试在三级上分别预扣的额度。
#[derive(Clone, Copy)]
struct RateReservations {
    account: RateReservation,
    key: RateReservation,
    target: RateReservation,
}

/// 一次已获准的尝试。析构即释放并发名额。
pub struct Admission {
    account: Arc<AccountState>,
    /// 本次尝试绑定的 Key。`None` 表示这次调用不带凭据语义（探针等）。
    key: Option<Arc<KeyState>>,
    target: Arc<TargetState>,
    /// 名额随本结构体一同释放，不需要显式归还。
    _permit: CapacityPermit,
    /// 本次是否占用了账号级额度熔断的半开试运行名额。
    account_half_open: bool,
    /// 本次是否占用了该 Key 额度熔断的半开试运行名额。
    key_half_open: bool,
    /// 本次是否占用了目标半开试运行名额。
    half_open: bool,
    reservations: RateReservations,
    /// 本次准入用的三级额度。取消时据此把并发容量补回配置值。
    limits: AdmissionLimits,
    settled: bool,
}

impl Admission {
    /// 本次是否是熔断后的半开试运行。
    pub fn is_half_open(&self) -> bool {
        self.half_open
    }

    /// 绑定到本次尝试的 Key 状态。
    pub fn key_state(&self) -> Option<&Arc<KeyState>> {
        self.key.as_ref()
    }

    fn release_account_half_open(&self) {
        if self.account_half_open {
            crate::sync::lock(&self.account.quota).release_half_open();
        }
    }

    fn release_key_half_open(&self) {
        if self.key_half_open
            && let Some(key) = &self.key
        {
            crate::sync::lock(&key.quota).release_half_open();
        }
    }

    /// 请求尚未发送到上游（例如倍率终检失败），完整退回本次预留额度。
    pub fn cancel_before_upstream(mut self) {
        self.settled = true;
        let now = Instant::now();
        self.account.budget.cancel(self.reservations.account, now);
        if let Some(key) = &self.key {
            key.budget.cancel(self.reservations.key, now);
        }
        self.target.budget.cancel(self.reservations.target, now);
        // 并发名额随 permit 析构归还，但额定容量还要补回去，否则反复的
        // "预扣 → 取消"会把这一级的容量越削越小（§17.1）。
        self.account.budget.restore_rate(self.limits.account);
        if let Some(key) = &self.key {
            key.budget.restore_rate(self.limits.key);
        }
        self.target.budget.restore_rate(self.limits.target);
        self.release_account_half_open();
        self.release_key_half_open();
        crate::sync::lock(&self.target.circuit).undo_half_open(self.half_open);
    }

    /// 上报结果并释放半开名额。
    ///
    /// `actual_tokens` 已知时按真实用量归还预留差额；未知（没有 tokenizer 且
    /// 上游没回 usage）时保留保守估算，宁可少发也不要超限（§17.2）。
    ///
    /// 凭据级结果（失效、额度耗尽）只作用于**这一把 Key**：账号里其他 Key 继续
    /// 服务，这正是 Key 池相对"一 Key 一账号"的额外价值（§4.2.1）。
    pub fn settle(mut self, outcome: Outcome, actual_tokens: Option<u64>) {
        self.settled = true;
        let now = Instant::now();

        self.account
            .budget
            .settle(self.reservations.account, actual_tokens, now);
        if let Some(key) = &self.key {
            key.budget.settle(self.reservations.key, actual_tokens, now);
        }
        self.target
            .budget
            .settle(self.reservations.target, actual_tokens, now);

        match outcome {
            Outcome::Neutral => {
                self.release_account_half_open();
                self.release_key_half_open();
                crate::sync::lock(&self.target.circuit).release_half_open();
            }
            Outcome::Success => {
                // 成功一次就清掉凭据的硬停与额度熔断：这是 Key 从"坏"回到
                // "好"的唯一路径。
                if let Some(key) = &self.key {
                    key.note_success();
                    crate::sync::lock(&key.quota).on_success(now);
                }
                crate::sync::lock(&self.account.quota).on_success(now);
                crate::sync::lock(&self.target.circuit).on_success(now);
            }
            Outcome::KeyInvalid => {
                // 三层半开名额都要还回去，否则这个目标会一直卡在"有人正在
                // 试运行"而永远无法恢复。凭据层要显式 undo：它的半开名额是
                // 这次尝试占的，跟着这次失败一起作废。
                if let Some(key) = &self.key {
                    key.confirm_invalid(now);
                    crate::sync::lock(&key.quota).undo_half_open(self.key_half_open);
                }
                self.release_account_half_open();
                crate::sync::lock(&self.target.circuit).release_half_open();
            }
            Outcome::QuotaExhausted { retry_after } => {
                // `trip` 自己会把半开状态清成"冷却中"，不需要再 release。
                match &self.key {
                    Some(key) => crate::sync::lock(&key.quota).trip(now, retry_after),
                    None => crate::sync::lock(&self.account.quota).trip(now, retry_after),
                }
                crate::sync::lock(&self.target.circuit).release_half_open();
            }
            Outcome::RateLimited { retry_after } => {
                self.release_account_half_open();
                self.release_key_half_open();
                let mut circuit = crate::sync::lock(&self.target.circuit);
                match retry_after {
                    // 上游明确说了多久，就照做，不叠加自己的指数退避。
                    Some(wait) => circuit.trip(now, Some(wait)),
                    // 没给恢复时间的孤立 429 只触发本次切换，够多够密才熔断。
                    None => circuit.on_fault(now),
                }
            }
            Outcome::Fault => {
                self.release_account_half_open();
                self.release_key_half_open();
                crate::sync::lock(&self.target.circuit).on_fault(now);
            }
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.target.budget.inflight.fetch_sub(1, Ordering::Relaxed);
        self.account.budget.inflight.fetch_sub(1, Ordering::Relaxed);
        if let Some(key) = &self.key {
            key.budget.inflight.fetch_sub(1, Ordering::Relaxed);
        }
        if !self.settled {
            // 客户端断开或任务被取消：半开名额必须还回去，否则这个目标会
            // 一直卡在"有人正在试运行"而永远无法恢复。
            self.release_account_half_open();
            self.release_key_half_open();
            crate::sync::lock(&self.target.circuit).undo_half_open(self.half_open);
        }
    }
}

impl Registry {
    /// 一次调用在动态状态表里的 Key 归类键。
    ///
    /// 账号前缀必须有：同一个真实 Key 可以出现在两个分组的两条账号记录里，
    /// 那时它们的熔断与额度必须各自独立（§4.2）。
    fn keyed(&self, account_id: &str, credential_digest: Option<&str>) -> Option<Arc<KeyState>> {
        let digest = credential_digest?;
        Some(self.key(&crate::credential::credential_id(account_id, digest)))
    }

    /// 不消耗额度的资格检查（§9.1）。
    pub fn check<'a, C: Into<Caller<'a>>, L: Into<AdmissionLimits>>(
        &self,
        caller: C,
        limits: L,
    ) -> Result<(), Unavailable> {
        let caller = caller.into();
        let limits = limits.into();
        let account = self.account(caller.account_id);
        let key = self.keyed(caller.account_id, caller.key_id);
        let target = self.target(caller.target_id);
        // 与准入、排队看到同一份额度，否则三处会各算各的（§13.6）。
        let limits = effective_limits(limits, &account, key.as_ref(), &target);
        target.check(&account, key.as_deref(), limits, Instant::now())
    }

    /// 立即准入：拿不到名额时不等待，由调用方决定换目标还是排队。
    pub fn try_admit<'a, C: Into<Caller<'a>>, L: Into<AdmissionLimits>>(
        &self,
        caller: C,
        limits: L,
        estimated_tokens: u64,
    ) -> Result<Admission, Unavailable> {
        self.admit(caller.into(), limits.into(), estimated_tokens, None)
    }

    /// 用排队时已经赢到的并发名额准入。
    ///
    /// 名额是 FIFO 排到的，直接带着它进门才能保证等了 30 秒的请求不会在最后
    /// 一步被刚到的新请求插队；倍率与健康终检仍然照做，名额不能绕过它们。
    pub fn admit_with_permit<'a, C: Into<Caller<'a>>, L: Into<AdmissionLimits>>(
        &self,
        caller: C,
        limits: L,
        estimated_tokens: u64,
        permit: CapacityPermit,
    ) -> Result<Admission, Unavailable> {
        self.admit(caller.into(), limits.into(), estimated_tokens, Some(permit))
    }

    fn admit(
        &self,
        caller: Caller<'_>,
        limits: AdmissionLimits,
        estimated_tokens: u64,
        permit: Option<CapacityPermit>,
    ) -> Result<Admission, Unavailable> {
        let account = self.account(caller.account_id);
        let key = self.keyed(caller.account_id, caller.key_id);
        let target = self.target(caller.target_id);
        // 本轮**实际生效**的额度。只在准入、资格检查、排队三处各算一遍会漂移：
        // 某处把 `None` 当"不限"、另一处当成"沿用配置"，就会出现"准入认为满、
        // 排队认为不限"这种组合，请求于是绕过排队（§13.6、§17.1）。
        let effective = effective_limits(limits, &account, key.as_ref(), &target);
        account
            .budget
            .reconcile_capacity(effective.account.max_concurrency);
        if let Some(key) = &key {
            key.budget.reconcile_capacity(effective.key.max_concurrency);
        }
        target
            .budget
            .reconcile_capacity(effective.target.max_concurrency);
        let limits = effective;
        let now = Instant::now();

        // 先看"坏不坏"再看"忙不忙"：一个既熔断又满载的目标必须报熔断，否则
        // 调用方会把它当成"忙"去排队等一个永远不会好的目标。三级依次占用
        // 半开名额，任何一级失败都要把前面占到的还回去。
        let account_half_open = account.try_enter(now)?;
        let key_half_open = match &key {
            Some(key) => match key.try_enter(now) {
                Ok(taken) => taken,
                Err(reason) => {
                    crate::sync::lock(&account.quota).undo_half_open(account_half_open);
                    return Err(reason);
                }
            },
            None => false,
        };
        let half_open = crate::sync::lock(&target.circuit)
            .try_enter(now)
            .map_err(|()| {
                if let Some(key) = &key {
                    crate::sync::lock(&key.quota).undo_half_open(key_half_open);
                }
                crate::sync::lock(&account.quota).undo_half_open(account_half_open);
                Unavailable::Cooling
            })?;

        // 名额先拿，额度后扣：拿不到名额时不能留下已扣的限流计数。
        let permit = match permit {
            Some(permit) => permit,
            None => {
                // 用**本轮实际生效**的额度取名额：只按调用方传的值取，会让
                // "调用方没提并发但管理员配了上限"的目标完全没有名额可抢
                // （§17.1）。
                match self
                    .capacity_of(&account, key.as_ref(), &target, limits)
                    .try_acquire()
                {
                    Ok(permit) => permit,
                    Err(_) => {
                        crate::sync::lock(&target.circuit).undo_half_open(half_open);
                        if let Some(key) = &key {
                            crate::sync::lock(&key.quota).undo_half_open(key_half_open);
                        }
                        crate::sync::lock(&account.quota).undo_half_open(account_half_open);
                        return Err(Unavailable::ConcurrencyFull);
                    }
                }
            }
        };

        // 额度逐级扣减。任何一级不够就把前面已扣的完整退回。
        let mut reservations = RateReservations {
            account: RateReservation::none(),
            key: RateReservation::none(),
            target: RateReservation::none(),
        };
        let consumed = (|| -> Result<(), Unavailable> {
            reservations.account =
                consume_rate(&account.budget, limits.account, estimated_tokens, now)?;
            if let Some(key) = &key {
                reservations.key = consume_rate(&key.budget, limits.key, estimated_tokens, now)?;
            }
            match consume_rate(&target.budget, limits.target, estimated_tokens, now) {
                Ok(reservation) => {
                    reservations.target = reservation;
                    Ok(())
                }
                Err(reason) => Err(reason),
            }
        })();
        if let Err(reason) = consumed {
            account.budget.cancel(reservations.account, now);
            if let Some(key) = &key {
                key.budget.cancel(reservations.key, now);
            }
            target.budget.cancel(reservations.target, now);
            crate::sync::lock(&target.circuit).undo_half_open(half_open);
            if let Some(key) = &key {
                crate::sync::lock(&key.quota).undo_half_open(key_half_open);
            }
            crate::sync::lock(&account.quota).undo_half_open(account_half_open);
            return Err(reason);
        }

        target.budget.inflight.fetch_add(1, Ordering::Relaxed);
        account.budget.inflight.fetch_add(1, Ordering::Relaxed);
        if let Some(key) = &key {
            key.budget.inflight.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Admission {
            account,
            key,
            target,
            _permit: permit,
            account_half_open,
            key_half_open,
            half_open,
            reservations,
            limits,
            settled: false,
        })
    }

    /// 排队必须同时等待账号共享容量、Key 容量与目标局部容量（§4.2.1）。
    pub fn capacity<L: Into<AdmissionLimits>>(&self, caller: Caller<'_>, limits: L) -> Capacity {
        let account = self.account(caller.account_id);
        let key = self.keyed(caller.account_id, caller.key_id);
        let target = self.target(caller.target_id);
        // 用**实际生效**的额度决定等哪一级：只看调用方传的值会漏掉真正满载的
        // 那一级，等待者于是立刻被"有空位"的假象唤醒（§13.6、§17.1）。
        let limits = effective_limits(limits.into(), &account, key.as_ref(), &target);
        // 不限的级别不参与排队：只有配了上限的那一级才会被等。
        self.capacity_of(&account, key.as_ref(), &target, limits)
    }

    /// 一个候选在"Key 还没选定"时可以等的容量。
    ///
    /// 排队阶段还不知道最终会用哪把 Key。等待**任意一把** Key 的名额释放，
    /// 否则"池里两把都满"会一直卡在目标名额上，等一个永远不会到来的唤醒。
    ///
    /// 这里挑"最容易空出来"的那把：先看谁还有空闲名额，再看谁的上限最大。
    /// 单靠一个信号量无法覆盖池里的每一把，所以调用方必须把这种情况当作"限流"
    /// 做短轮询——唤醒可能来自没被选中的那一把（§13.6）。
    pub fn capacity_any_key(
        &self,
        caller: Caller<'_>,
        limits: AdmissionLimits,
        keys: &[Arc<crate::credential::Credential>],
    ) -> Capacity {
        let account = self.account(caller.account_id);
        let target = self.target(caller.target_id);
        let key_budget = keys
            .iter()
            .filter(|key| key.enabled && key.limits.max_concurrency.is_some())
            // 已经校准过的那把优先；同为满时取上限最大的（空出来的机会最多）。
            .max_by_key(|key| {
                let state = self.keyed(&key.account_id, Some(&key.credential_digest));
                let free = state.as_ref().map(|s| s.budget.available()).unwrap_or(0);
                (free > 0, key.limits.max_concurrency.unwrap_or(0), free)
            })
            .and_then(|key| self.keyed(&key.account_id, Some(&key.credential_digest)))
            .map(|state| {
                // 校准放在这里而不是选 Key 路径上：等待方要知道"这把 Key 有几个
                // 名额"，否则信号量的额定容量还停在"不限"，等它永远不会醒。
                state.budget.reconcile_capacity(limits.key.max_concurrency);
                Arc::clone(&state.budget.permits)
            });
        Capacity {
            account: limits
                .account
                .max_concurrency
                .map(|_| Arc::clone(&account.budget.permits)),
            key: key_budget,
            target: limits
                .target
                .max_concurrency
                .map(|_| Arc::clone(&target.budget.permits)),
        }
    }

    /// 三级各自的名额；**只有配了上限的那一级才有名额可等**。
    fn capacity_of(
        &self,
        account: &Arc<AccountState>,
        key: Option<&Arc<KeyState>>,
        target: &Arc<TargetState>,
        limits: AdmissionLimits,
    ) -> Capacity {
        let bounded = |limits: Limits, budget: &Budget| {
            limits.max_concurrency.map(|_| Arc::clone(&budget.permits))
        };
        Capacity {
            account: bounded(limits.account, &account.budget),
            key: key.and_then(|key| bounded(limits.key, &key.budget)),
            target: bounded(limits.target, &target.budget),
        }
    }
}

/// 把"调用方要求的额度"与"三级各自已经配好的额度"合成**本轮实际生效**的额度。
///
/// 三处必须看到同一份结果：
///
/// * 准入（[\`Registry::admit\`]）用它决定扣多少、开哪个信号量；
/// * 排队（[\`Registry::capacity\`]）用它决定等哪一级的名额；
/// * 资格检查（[\`Registry::check\`]）用它判断忙不忙。
///
/// 口径是"逐级继承"：调用方没提并发就用该级已经校准好的配置值。把 `None` 当成
/// "不限"会让满载的一级被当成有空位；把 `Some` 当成"只按它算"又会让排队的
/// 唤醒源漏掉真正满载的那一级（§13.6、§17.1）。
fn effective_limits(
    requested: AdmissionLimits,
    account: &AccountState,
    key: Option<&Arc<KeyState>>,
    target: &TargetState,
) -> AdmissionLimits {
    AdmissionLimits {
        account: requested.account.with_configured(account.configured()),
        key: requested
            .key
            .with_configured(key.and_then(|key| key.configured())),
        target: requested.target.with_configured(target.configured()),
    }
}

/// 扣减 RPM 与 TPM 额度。任一项不足时把已扣的另一项退回。
fn consume_rate(
    target: &Budget,
    limits: Limits,
    estimated_tokens: u64,
    now: Instant,
) -> Result<RateReservation, Unavailable> {
    if let Some(rpm) = limits.rpm
        && !crate::sync::lock(&target.rpm).try_consume(1, f64::from(rpm), now)
    {
        return Err(Unavailable::RateLimited);
    }
    // TPM 未配置时完全跳过 Token 估算，避免无意义开销（§17.2）。
    if let Some(tpm) = limits.tpm
        && !crate::sync::lock(&target.tpm).try_consume(estimated_tokens, f64::from(tpm), now)
    {
        if limits.rpm.is_some() {
            crate::sync::lock(&target.rpm).refund(1, now);
        }
        return Err(Unavailable::RateLimited);
    }
    Ok(RateReservation {
        at: now,
        rpm: limits.rpm.is_some(),
        tokens: limits.tpm.map(|_| estimated_tokens),
    })
}

/// 熔断器的当前相位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Closed,
    Cooling,
    /// 冷却已到期，半开名额还没人占。
    HalfOpenAvailable,
    /// 已经有一个真实请求在试运行，其余请求继续按冷却处理。
    HalfOpenTaken,
}

/// 冷却 / 半开状态机。账号额度与目标故障共用同一套实现。
#[derive(Debug)]
struct Circuit {
    cooldown_until: Option<Instant>,
    /// 指数冷却的级别。成功一次即清零。
    level: u32,
    half_open_taken: bool,
    consecutive: u32,
    failures: SlidingWindow,
    totals: SlidingWindow,
}

impl Default for Circuit {
    fn default() -> Self {
        Self {
            cooldown_until: None,
            level: 0,
            half_open_taken: false,
            consecutive: 0,
            failures: SlidingWindow::new(FAULT_WINDOW),
            totals: SlidingWindow::new(FAULT_WINDOW),
        }
    }
}

impl Circuit {
    fn phase(&self, now: Instant) -> Phase {
        match self.cooldown_until {
            None => Phase::Closed,
            Some(until) if now < until => Phase::Cooling,
            Some(_) if self.half_open_taken => Phase::HalfOpenTaken,
            Some(_) => Phase::HalfOpenAvailable,
        }
    }

    fn is_cooling(&self, now: Instant) -> bool {
        !matches!(self.phase(now), Phase::Closed)
    }

    fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        self.cooldown_until
            .filter(|until| *until > now)
            .map(|until| until - now)
    }

    /// 尝试进入。返回值表示本次是否占用了半开试运行名额。
    fn try_enter(&mut self, now: Instant) -> Result<bool, ()> {
        match self.phase(now) {
            Phase::Closed => Ok(false),
            // 冷却结束后只放行一个真实用户请求（§12.3）。
            Phase::HalfOpenAvailable => {
                self.half_open_taken = true;
                Ok(true)
            }
            Phase::Cooling | Phase::HalfOpenTaken => Err(()),
        }
    }

    /// 准入在后续步骤失败时回退半开占用。
    fn undo_half_open(&mut self, taken: bool) {
        if taken {
            self.half_open_taken = false;
        }
    }

    fn release_half_open(&mut self) {
        self.half_open_taken = false;
    }

    fn on_success(&mut self, now: Instant) {
        self.totals.try_consume(1, f64::INFINITY, now);
        self.consecutive = 0;
        self.cooldown_until = None;
        self.half_open_taken = false;
        self.level = 0;
    }

    /// 额度耗尽或账号封禁：直接进入冷却，可带上游给出的恢复时间。
    fn trip(&mut self, now: Instant, retry_after: Option<Duration>) {
        let wait = retry_after.unwrap_or_else(|| self.next_backoff());
        if retry_after.is_none() {
            self.level = self.level.saturating_add(1);
        }
        self.cooldown_until = Some(now + wait);
        self.half_open_taken = false;
    }

    /// 记一次故障。半开试运行失败会直接延长冷却；否则按 §12.3 的阈值判定。
    fn on_fault(&mut self, now: Instant) {
        self.totals.try_consume(1, f64::INFINITY, now);
        self.failures.try_consume(1, f64::INFINITY, now);
        self.consecutive = self.consecutive.saturating_add(1);

        if self.half_open_taken {
            self.half_open_taken = false;
            self.level = self.level.saturating_add(1);
            self.cooldown_until = Some(now + self.backoff_for(self.level));
            return;
        }

        let failures = self.failures.estimate(now);
        let totals = self.totals.estimate(now).max(failures);
        let tripped = self.consecutive >= CONSECUTIVE_TRIP
            || (totals >= RATIO_MIN_SAMPLES && failures / totals >= RATIO_TRIP);
        if tripped {
            self.level = self.level.saturating_add(1);
            self.cooldown_until = Some(now + self.backoff_for(self.level));
            self.consecutive = 0;
        }
    }

    fn next_backoff(&self) -> Duration {
        self.backoff_for(self.level.saturating_add(1))
    }

    fn backoff_for(&self, level: u32) -> Duration {
        COOLDOWN_BASE
            .saturating_mul(1u32 << level.min(6))
            .min(COOLDOWN_CAP)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// 滑动窗口计数器。
///
/// 用"当前窗口 + 上一窗口按剩余比例加权"近似真实滑动窗口——这是限流器里
/// 的标准做法：既没有固定窗口在边界处放行两倍流量的问题，也不必为每个请求
/// 保存时间戳。
#[derive(Debug)]
struct SlidingWindow {
    span: Duration,
    start: Option<Instant>,
    current: u64,
    previous: u64,
}

impl SlidingWindow {
    fn new(span: Duration) -> Self {
        Self {
            span,
            start: None,
            current: 0,
            previous: 0,
        }
    }

    fn roll(&mut self, now: Instant) {
        let Some(start) = self.start else {
            self.start = Some(now);
            return;
        };
        let elapsed = now.saturating_duration_since(start);
        if elapsed < self.span {
            return;
        }
        // 停顿超过两个窗口时上一窗口也已经完全过期，直接清零。
        if elapsed < self.span * 2 {
            self.previous = self.current;
        } else {
            self.previous = 0;
        }
        self.current = 0;
        let remainder = elapsed.as_nanos() % self.span.as_nanos();
        self.start = Some(now - Duration::from_nanos(remainder as u64));
    }

    fn estimate(&self, now: Instant) -> f64 {
        let Some(start) = self.start else {
            return 0.0;
        };
        let elapsed = now.saturating_duration_since(start);
        if elapsed >= self.span * 2 {
            return 0.0;
        }
        if elapsed >= self.span {
            // 尚未 roll：当前窗口整体变成"上一窗口"，按剩余比例加权。
            let weight =
                1.0 - (elapsed.as_secs_f64() - self.span.as_secs_f64()) / self.span.as_secs_f64();
            return self.current as f64 * weight.clamp(0.0, 1.0);
        }
        let weight = 1.0 - elapsed.as_secs_f64() / self.span.as_secs_f64();
        self.previous as f64 * weight + self.current as f64
    }

    fn try_consume(&mut self, amount: u64, limit: f64, now: Instant) -> bool {
        self.roll(now);
        if self.estimate(now) + amount as f64 > limit {
            return false;
        }
        self.current = self.current.saturating_add(amount);
        true
    }

    fn refund(&mut self, amount: u64, now: Instant) {
        self.roll(now);
        self.current = self.current.saturating_sub(amount);
    }

    fn refund_at(&mut self, amount: u64, reserved_at: Instant, now: Instant) {
        self.roll(now);
        let start = self.start.expect("roll 初始化窗口");
        if reserved_at >= start {
            self.current = self.current.saturating_sub(amount);
        } else if start.duration_since(reserved_at) <= self.span {
            self.previous = self.previous.saturating_sub(amount);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(concurrency: Option<u32>) -> Limits {
        Limits {
            max_concurrency: concurrency,
            ..Limits::default()
        }
    }

    /// 一个不带 Key 语义的调用者（账号 + 目标）。
    ///
    /// 单 Key 账号与探针路径都是这个形状：Key 级状态不参与，行为与引入 Key 池
    /// 之前完全一致（§4.2.1）。
    fn caller<'a>(account_id: &'a str, target_id: &'a str) -> Caller<'a> {
        Caller {
            account_id,
            key_id: None,
            target_id,
        }
    }

    /// 一个绑定到具体 Key 的调用者（账号 + Key 归类键 + 目标）。
    fn caller_with_key<'a>(account_id: &'a str, key_id: &'a str, target_id: &'a str) -> Caller<'a> {
        Caller {
            account_id,
            key_id: Some(key_id),
            target_id,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn isolated_faults_only_switch_but_a_run_of_them_trips_the_breaker() {
        let registry = Registry::new();
        // 孤立错误不熔断：连续 4 次仍然可以继续尝试（§12.3）。
        for _ in 0..CONSECUTIVE_TRIP - 1 {
            let admission = registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .unwrap();
            admission.settle(Outcome::Fault, None);
            assert!(
                registry
                    .check(caller("acc", "tgt"), Limits::default())
                    .is_ok()
            );
        }

        let admission = registry
            .try_admit(caller("acc", "tgt"), Limits::default(), 0)
            .unwrap();
        admission.settle(Outcome::Fault, None);
        assert_eq!(
            registry.check(caller("acc", "tgt"), Limits::default()),
            Err(Unavailable::Cooling)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_high_failure_ratio_trips_even_without_a_consecutive_run() {
        let registry = Registry::new();
        // 成功与失败交替：连续计数永远到不了 5，但比例条件成立。
        let mut tripped_after = None;
        for pair in 1..=20 {
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Success, None);
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Fault, None);
            if registry.check(caller("acc", "tgt"), Limits::default()) == Err(Unavailable::Cooling)
            {
                tripped_after = Some(pair);
                break;
            }
        }
        // 稳定 50% 失败率必须熔断，否则每两次请求就要重试一次；但要攒够最小
        // 样本数才动手，1/2 失败不能算数。
        assert_eq!(
            tripped_after,
            Some(RATIO_MIN_SAMPLES as usize / 2),
            "应当恰好在样本数达到 {RATIO_MIN_SAMPLES} 时熔断"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cooldown_opens_exactly_one_trial_slot_and_success_restores_it() {
        let registry = Registry::new();
        for _ in 0..CONSECUTIVE_TRIP {
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Fault, None);
        }
        assert!(
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .is_err()
        );

        tokio::time::advance(COOLDOWN_BASE * 2).await;
        let trial = registry
            .try_admit(caller("acc", "tgt"), Limits::default(), 0)
            .unwrap();
        assert!(trial.is_half_open());
        // 只放行一个真实请求：第二个仍然被挡在冷却外（§12.3）。
        assert_eq!(
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .err(),
            Some(Unavailable::Cooling)
        );

        trial.settle(Outcome::Success, None);
        assert!(
            registry
                .check(caller("acc", "tgt"), Limits::default())
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_trial_extends_the_cooldown() {
        let registry = Registry::new();
        for _ in 0..CONSECUTIVE_TRIP {
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Fault, None);
        }
        tokio::time::advance(COOLDOWN_BASE * 2).await;
        registry
            .try_admit(caller("acc", "tgt"), Limits::default(), 0)
            .unwrap()
            .settle(Outcome::Fault, None);

        // 第一次冷却是 2^1，失败后升到 2^2；原来的时长已不足以放行。
        tokio::time::advance(COOLDOWN_BASE * 2).await;
        assert_eq!(
            registry.check(caller("acc", "tgt"), Limits::default()),
            Err(Unavailable::Cooling)
        );
        tokio::time::advance(COOLDOWN_BASE * 4).await;
        assert!(
            registry
                .check(caller("acc", "tgt"), Limits::default())
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_admission_returns_the_trial_slot() {
        let registry = Registry::new();
        for _ in 0..CONSECUTIVE_TRIP {
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Fault, None);
        }
        tokio::time::advance(COOLDOWN_BASE * 2).await;

        // 客户端断开：Admission 没有 settle 就被丢弃，名额必须还回去，
        // 否则这个目标会永远停在"有人正在试运行"。
        drop(
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .unwrap(),
        );
        assert!(
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .unwrap()
                .is_half_open()
        );
    }

    /// 401/403 只停**那一把 Key**，账号内其他 Key 继续服务（§4.2.1）。
    ///
    /// 这是相对"一 Key 一账号"时代最重要的行为修正：单 Key 账号里账号与 Key
    /// 的作用域恰好重合，表现不变；多 Key 账号里一把 Key 被封不该让整号停摆。
    #[tokio::test(start_paused = true)]
    async fn an_invalid_key_only_pauses_that_key() {
        let registry = Registry::new();
        // 一次判定只算一次"上游说凭据不对"：连续确认到阈值才硬停（§12.3）。
        for _ in 0..KEY_INVALID_CONFIRMATIONS {
            registry
                .try_admit(
                    caller_with_key("acc", "key-a", "tgt-a"),
                    Limits::default(),
                    0,
                )
                .unwrap()
                .settle(Outcome::KeyInvalid, None);
        }
        // 同一把 Key 下的另一个模型也必须一起停。
        assert_eq!(
            registry.check(caller_with_key("acc", "key-a", "tgt-b"), Limits::default()),
            Err(Unavailable::KeyInvalid)
        );
        assert!(!Unavailable::KeyInvalid.is_queueable(), "鉴权失败绝不排队");
        // 换一把 Key 立刻可用：账号没有整体进入硬停。
        assert!(
            registry
                .check(caller_with_key("acc", "key-b", "tgt-a"), Limits::default())
                .is_ok(),
            "同账号的另一把 Key 不该被牵连"
        );

        // 管理员改过凭据之后，整个账号的 Key 一起解除硬停。
        registry.clear_account_faults("acc");
        assert_eq!(
            registry.check(caller_with_key("acc", "key-a", "tgt-b"), Limits::default()),
            Ok(()),
            "clear_account_faults 必须把该账号下每一把 Key 的硬停都清掉"
        );
    }

    /// 单次 403 不能把好 Key 钉死，硬停也必须能自己到期（§12.3）。
    ///
    /// 这条是"面板显示 Key 失效，但 Key 是好的"的根因回归：上游用 403 表达
    /// 分组停用/权限不足时，判定必须能被一次真实成功推翻，或者到点自动放行
    /// 一次试运行——否则这把 Key 已经被排除在抽签之外，那个"成功"永远不来。
    #[tokio::test(start_paused = true)]
    async fn a_single_denial_does_not_hard_stop_a_key_and_the_stop_expires() {
        let registry = Registry::new();
        let key = |name: &'static str| caller_with_key("acc", name, "tgt");

        // 一次判定：只算一次怀疑，Key 继续服务。
        registry
            .try_admit(key("key-a"), Limits::default(), 0)
            .unwrap()
            .settle(Outcome::KeyInvalid, None);
        assert!(
            registry.check(key("key-a"), Limits::default()).is_ok(),
            "偶发一次 403 不该把 Key 钉死"
        );

        // 再来一次：确认到位，硬停。
        registry
            .try_admit(key("key-a"), Limits::default(), 0)
            .unwrap()
            .settle(Outcome::KeyInvalid, None);
        assert_eq!(
            registry.check(key("key-a"), Limits::default()),
            Err(Unavailable::KeyInvalid)
        );

        // 自证窗口到期：放行一次真实请求去证明自己。
        tokio::time::advance(KEY_INVALID_PROBE_AFTER).await;
        assert!(
            registry.check(key("key-a"), Limits::default()).is_ok(),
            "窗口到期后必须允许一次自证，否则故障再也没有出口"
        );

        // 证明不了：确认计数仍在，立刻重新硬停。
        registry
            .try_admit(key("key-a"), Limits::default(), 0)
            .unwrap()
            .settle(Outcome::KeyInvalid, None);
        assert_eq!(
            registry.check(key("key-a"), Limits::default()),
            Err(Unavailable::KeyInvalid)
        );

        // 证明得了：自动恢复，且此后的偶发判定不叠加旧账。
        tokio::time::advance(KEY_INVALID_PROBE_AFTER).await;
        registry
            .try_admit(key("key-a"), Limits::default(), 0)
            .unwrap()
            .settle(Outcome::Success, None);
        assert!(
            registry.check(key("key-a"), Limits::default()).is_ok(),
            "一次真实成功必须清掉硬停（§12.3）"
        );
        registry
            .try_admit(key("key-a"), Limits::default(), 0)
            .unwrap()
            .settle(Outcome::KeyInvalid, None);
        assert!(
            registry.check(key("key-a"), Limits::default()).is_ok(),
            "计数必须从零开始，不能接着恢复前的旧账"
        );
    }

    /// 管理员可以只放行一把 Key，不必动同账号其他 Key 的熔断（§4.2.1）。
    #[tokio::test(start_paused = true)]
    async fn clearing_one_keys_faults_leaves_its_siblings_alone() {
        let registry = Registry::new();
        let key = |name: &'static str| caller_with_key("acc", name, "tgt");
        for name in ["key-a", "key-b"] {
            for _ in 0..KEY_INVALID_CONFIRMATIONS {
                registry
                    .try_admit(key(name), Limits::default(), 0)
                    .unwrap()
                    .settle(Outcome::KeyInvalid, None);
            }
        }

        registry.clear_key_faults("acc", "key-a");
        assert!(
            registry.check(key("key-a"), Limits::default()).is_ok(),
            "被清除的那把必须立刻可用"
        );
        assert_eq!(
            registry.check(key("key-b"), Limits::default()),
            Err(Unavailable::KeyInvalid),
            "同账号另一把 Key 的硬停不该被连带清掉"
        );
    }

    /// 一把 Key 额度耗尽同样只影响它自己（§4.2.1）。
    #[tokio::test(start_paused = true)]
    async fn an_exhausted_key_leaves_the_others_alone() {
        let registry = Registry::new();
        registry
            .try_admit(caller_with_key("acc", "key-a", "tgt"), Limits::default(), 0)
            .unwrap()
            .settle(Outcome::QuotaExhausted { retry_after: None }, None);

        assert_eq!(
            registry.check(caller_with_key("acc", "key-a", "tgt"), Limits::default()),
            Err(Unavailable::QuotaExhausted)
        );
        assert!(
            registry
                .check(caller_with_key("acc", "key-b", "tgt"), Limits::default())
                .is_ok()
        );
    }

    /// 预算缩容之后，已发出的名额不能再被重复发放。
    ///
    /// 这是 Key 池最容易踩坏的一条：账号里每把 Key 都有自己的信号量，缩容必须
    /// 只回收空闲名额，否则"把并发从 100 改成 1"会变成"再发一轮 99 个名额"。
    #[tokio::test(start_paused = true)]
    async fn shrinking_a_budget_never_hands_out_the_same_slot_twice() {
        let budget = Budget::new();
        let limits = Limits {
            max_concurrency: Some(1),
            ..Limits::default()
        };
        budget.reconcile_capacity(limits.max_concurrency);
        let held = budget.try_acquire().unwrap();
        assert_eq!(budget.available(), 0);
        // 反复用同一个上限做校准：不能再放出名额。
        for _ in 0..3 {
            budget.reconcile_capacity(limits.max_concurrency);
            assert_eq!(budget.available(), 0, "重复校准不能凭空放名额");
        }
        drop(held);
        assert_eq!(budget.available(), 1);
    }

    /// Key 级并发上限独立生效：一把 Key 满了，另一把照常接（§17.1）。
    ///
    /// 三级门限是"账号 → Key → 目标"逐级收紧。这里刻意让**目标**宽松、只让 Key
    /// 收紧，才能验证账号内多把 Key 之间额度互不侵占；三个维度混在一个用例里
    /// 会先被上一层挡住，测不出 Key 级的独立性。
    #[tokio::test(start_paused = true)]
    async fn key_level_concurrency_is_enforced_per_key() {
        let registry = Registry::new();
        let key_slots = |slots: u32| AdmissionLimits {
            key: Limits {
                max_concurrency: Some(slots),
                ..Limits::default()
            },
            ..AdmissionLimits::default()
        };
        let _held = registry
            .try_admit(caller_with_key("acc", "key-a", "tgt-1"), key_slots(1), 0)
            .unwrap();

        // 同一把 Key、另一个目标：Key 级额度是跨目标的，仍然满。
        assert_eq!(
            registry
                .try_admit(caller_with_key("acc", "key-a", "tgt-2"), key_slots(1), 0)
                .err(),
            Some(Unavailable::ConcurrencyFull),
            "同一把 Key 的并发额度在它的所有目标之间共享"
        );
        // 换一把 Key、同一个目标：这把 Key 有自己的额度。
        assert!(
            registry
                .try_admit(caller_with_key("acc", "key-b", "tgt-2"), key_slots(1), 0)
                .is_ok(),
            "另一把 Key 必须有自己的并发额度"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn quota_exhaustion_honours_the_upstream_recovery_time() {
        let registry = Registry::new();
        registry
            .try_admit(caller("acc", "tgt"), Limits::default(), 0)
            .unwrap()
            .settle(
                Outcome::QuotaExhausted {
                    retry_after: Some(Duration::from_secs(30)),
                },
                None,
            );
        assert_eq!(
            registry.check(caller("acc", "tgt"), Limits::default()),
            Err(Unavailable::QuotaExhausted)
        );

        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(
            registry
                .check(caller("acc", "tgt"), Limits::default())
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn quota_half_open_is_reserved_only_by_real_admission() {
        let registry = Registry::new();
        registry
            .try_admit(caller("acc", "tgt"), Limits::default(), 0)
            .unwrap()
            .settle(
                Outcome::QuotaExhausted {
                    retry_after: Some(Duration::from_secs(30)),
                },
                None,
            );
        tokio::time::advance(Duration::from_secs(31)).await;

        // 资格检查不能提前占用账号级半开名额，否则后面的真实准入会被自己挡住。
        assert!(
            registry
                .check(caller("acc", "tgt"), Limits::default())
                .is_ok()
        );
        let trial = registry
            .try_admit(caller("acc", "tgt"), Limits::default(), 0)
            .expect("半开恢复应允许一个真实试运行请求");
        assert_eq!(
            registry.check(caller("acc", "tgt"), Limits::default()),
            Err(Unavailable::QuotaExhausted)
        );

        // 取消试运行后，账号级和目标级半开名额都必须归还。
        drop(trial);
        assert!(
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_before_upstream_refunds_rpm_and_tpm() {
        let registry = Registry::new();
        let limits = Limits {
            rpm: Some(1),
            tpm: Some(100),
            ..Limits::default()
        };
        registry
            .try_admit(caller("acc", "tgt"), limits, 50)
            .unwrap()
            .cancel_before_upstream();

        // 终检失败或请求被取消在发往上游前发生时，不应吞掉本次限流预算。
        assert!(registry.try_admit(caller("acc", "tgt"), limits, 50).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_with_retry_after_cools_for_exactly_that_long() {
        let registry = Registry::new();
        registry
            .try_admit(caller("acc", "tgt"), Limits::default(), 0)
            .unwrap()
            .settle(
                Outcome::RateLimited {
                    retry_after: Some(Duration::from_secs(12)),
                },
                None,
            );
        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(
            registry.check(caller("acc", "tgt"), Limits::default()),
            Err(Unavailable::Cooling)
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            registry
                .check(caller("acc", "tgt"), Limits::default())
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_isolated_429_without_retry_after_only_switches() {
        let registry = Registry::new();
        registry
            .try_admit(caller("acc", "tgt"), Limits::default(), 0)
            .unwrap()
            .settle(Outcome::RateLimited { retry_after: None }, None);
        assert!(
            registry
                .check(caller("acc", "tgt"), Limits::default())
                .is_ok(),
            "一次没带恢复时间的 429 只该触发本次切换"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_is_capped_and_released_on_drop() {
        let registry = Registry::new();
        let first = registry
            .try_admit(caller("acc", "tgt"), limits(Some(1)), 0)
            .unwrap();
        assert_eq!(
            registry
                .try_admit(caller("acc", "tgt"), limits(Some(1)), 0)
                .err(),
            Some(Unavailable::ConcurrencyFull)
        );
        assert!(
            Unavailable::ConcurrencyFull.is_queueable(),
            "并发满是「忙」不是「坏」，应当允许排队"
        );

        drop(first);
        assert!(
            registry
                .try_admit(caller("acc", "tgt"), limits(Some(1)), 0)
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn raising_and_lowering_the_concurrency_cap_takes_effect() {
        let registry = Registry::new();
        let held = registry
            .try_admit(caller("acc", "tgt"), limits(Some(1)), 0)
            .unwrap();
        // 提高上限立即生效。
        let second = registry
            .try_admit(caller("acc", "tgt"), limits(Some(3)), 0)
            .unwrap();
        let third = registry
            .try_admit(caller("acc", "tgt"), limits(Some(3)), 0)
            .unwrap();
        assert_eq!(
            registry
                .try_admit(caller("acc", "tgt"), limits(Some(3)), 0)
                .err(),
            Some(Unavailable::ConcurrencyFull)
        );

        // 降低上限：在途请求不被打断，归还后名额才被真正收回。
        drop(second);
        drop(third);
        assert!(
            registry
                .try_admit(caller("acc", "tgt"), limits(Some(1)), 0)
                .is_err()
        );
        drop(held);
        assert!(
            registry
                .try_admit(caller("acc", "tgt"), limits(Some(1)), 0)
                .is_ok()
        );
    }

    /// 目标已配满时，**准入**必须拒绝（不能等只读的资格检查来拦）。
    ///
    /// 准入是热路径，它自己就是那道闸门：调用方传 `Limits::default()`（"对并发
    /// 没意见"）时，必须沿用已经校准好的上限，而不是把它当成"不限"放行。
    #[tokio::test(start_paused = true)]
    async fn admission_honours_an_already_calibrated_target_cap() {
        let registry = Registry::new();
        let one_slot = Limits {
            max_concurrency: Some(1),
            ..Limits::default()
        };
        let _held = registry
            .try_admit(caller("acc", "tgt"), one_slot, 0)
            .unwrap();

        assert_eq!(
            registry
                .try_admit(caller("acc", "tgt"), Limits::default(), 0)
                .err(),
            Some(Unavailable::ConcurrencyFull),
            "调用方没提并发时也要沿用已校准的上限"
        );
    }

    /// 目标级满载必须被资格检查看到（§13.6）。
    ///
    /// 这条曾经被打破：资格检查路径上不校准容量，于是"目标已经满载"在检查看来
    /// 是"1<<20 个名额全空"，排队与限流一起失效。
    #[tokio::test(start_paused = true)]
    async fn a_full_target_reports_concurrency_full() {
        let registry = Registry::new();
        let one_slot = Limits {
            max_concurrency: Some(1),
            ..Limits::default()
        };
        let _held = registry
            .try_admit(caller("acc", "tgt"), one_slot, 0)
            .unwrap();

        assert_eq!(
            registry.check(caller("acc", "tgt"), one_slot),
            Err(Unavailable::ConcurrencyFull),
            "占住唯一名额之后，检查必须报满载"
        );
        assert!(
            Unavailable::ConcurrencyFull.is_queueable(),
            "满载是\"忙\"，必须可排队"
        );
    }

    /// 并发准入时容量校准不能重复发放名额。
    ///
    /// 校准是"把额定容量改到配置值"，多线程同时进来也必须收敛到同一个数字；
    /// 这是 Key 池里每把 Key 各有一个信号量之后最容易踩坏的地方（§17.1）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_capacity_reconciliation_does_not_duplicate_permits() {
        let registry = Arc::new(Registry::new());
        assert!(
            registry
                .try_admit(caller("acc", "tgt"), limits(Some(1)), 0)
                .is_ok()
        );

        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                // 每一次都真的走准入路径：校准正是发生在这里。
                registry
                    .try_admit(caller("acc", "tgt"), limits(Some(8)), 0)
                    .map(|admission| admission.cancel_before_upstream())
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }

        assert_eq!(
            registry
                .capacity(caller("acc", "tgt"), limits(Some(8)))
                .target
                .expect("target 名额应当存在")
                .available_permits(),
            8,
            "反复校准必须恰好收敛到配置的 8，而不是越加越多"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rpm_blocks_within_the_window_and_recovers_after_it() {
        let registry = Registry::new();
        let capped = Limits {
            rpm: Some(2),
            ..Limits::default()
        };
        for _ in 0..2 {
            registry
                .try_admit(caller("acc", "tgt"), capped, 0)
                .unwrap()
                .settle(Outcome::Success, None);
        }
        assert_eq!(
            registry.try_admit(caller("acc", "tgt"), capped, 0).err(),
            Some(Unavailable::RateLimited)
        );

        tokio::time::advance(RATE_WINDOW * 2 + Duration::from_secs(1)).await;
        assert!(registry.try_admit(caller("acc", "tgt"), capped, 0).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn tpm_reservations_are_refunded_by_the_real_usage() {
        let registry = Registry::new();
        let capped = Limits {
            tpm: Some(1_000),
            ..Limits::default()
        };
        // 保守估算预留 900，实际只用了 100：差额必须还回窗口，否则一次
        // 大幅高估就能把整分钟的额度锁死。
        registry
            .try_admit(caller("acc", "tgt"), capped, 900)
            .unwrap()
            .settle(Outcome::Success, Some(100));
        assert!(
            registry
                .try_admit(caller("acc", "tgt"), capped, 800)
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_admission_does_not_consume_rate_budget() {
        let registry = Registry::new();
        let capped = Limits {
            rpm: Some(10),
            max_concurrency: Some(1),
            ..Limits::default()
        };
        let held = registry.try_admit(caller("acc", "tgt"), capped, 0).unwrap();
        for _ in 0..20 {
            assert_eq!(
                registry.try_admit(caller("acc", "tgt"), capped, 0).err(),
                Some(Unavailable::ConcurrencyFull)
            );
        }
        drop(held);
        // 被并发挡回的 20 次不能悄悄吃掉 RPM 额度。
        for _ in 0..9 {
            registry
                .try_admit(caller("acc", "tgt"), capped, 0)
                .unwrap()
                .settle(Outcome::Success, None);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stale_entries_are_dropped_when_the_configuration_changes() {
        let registry = Registry::new();
        registry
            .try_admit(caller("acc", "tgt"), Limits::default(), 0)
            .unwrap()
            .settle(Outcome::Fault, None);
        registry.retain(&[], &[]);
        assert_eq!(registry.targets.read().unwrap().len(), 0);
        assert_eq!(registry.accounts.read().unwrap().len(), 0);
    }
}
