//! 熔断、冷却、半开、并发与限流（§12、§17.1）。
//!
//! 这里的状态是**动态安全状态**：它不跟随配置版本，每次真正发请求前都要重新
//! 读取（§21）。所有计数用原子值或极短临界区的互斥量，不存在全局大锁（§19.4）。
//!
//! 时间统一使用 [`tokio::time::Instant`]，测试因此可以用 `tokio::time::pause`
//! 精确推进冷却与限流窗口，而不必真的睡上几分钟。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
/// 未配置最大并发时使用的"事实上不限"额度。
///
/// 用一个很大的常数而不是 `Option<Semaphore>`：上限可以被管理员随时改成有限
/// 值，统一走同一条准入路径才不会出现两套语义。
const UNLIMITED: u32 = 1 << 20;

/// 账号共享预算与目标局部预算必须同时满足。
#[derive(Debug, Clone, Copy, Default)]
pub struct AdmissionLimits {
    pub account: Limits,
    pub target: Limits,
}

impl From<Limits> for AdmissionLimits {
    fn from(target: Limits) -> Self {
        Self {
            account: Limits::default(),
            target,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Capacity {
    pub account: Arc<Semaphore>,
    pub target: Arc<Semaphore>,
}

#[derive(Debug)]
pub struct CapacityPermit {
    _account: OwnedSemaphorePermit,
    _target: OwnedSemaphorePermit,
}

impl Capacity {
    fn try_acquire(self) -> Result<CapacityPermit, Unavailable> {
        let target = self
            .target
            .try_acquire_owned()
            .map_err(|_| Unavailable::ConcurrencyFull)?;
        let account = self
            .account
            .try_acquire_owned()
            .map_err(|_| Unavailable::ConcurrencyFull)?;
        Ok(CapacityPermit {
            _account: account,
            _target: target,
        })
    }

    pub async fn acquire(self) -> Option<CapacityPermit> {
        // 不在等待繁忙目标时占住账号名额，其他模型仍可使用空闲账号容量。
        let target = self.target.acquire_owned().await.ok()?;
        let account = self.account.acquire_owned().await.ok()?;
        Some(CapacityPermit {
            _account: account,
            _target: target,
        })
    }
}

/// 目标当前不可用的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// 401/403 明确证明 Key 失效，换凭据前一直硬停（§12.3）。
    KeyInvalid,
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
    /// 401/403：整个账号立即暂停。
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
}

impl TargetStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Cooldown => "cooldown",
            Self::HalfOpen => "half_open",
            Self::QuotaExhausted => "quota_exhausted",
            Self::KeyInvalid => "key_invalid",
        }
    }
}

/// 所有账号与目标的动态状态。
#[derive(Default)]
pub struct Registry {
    accounts: RwLock<HashMap<String, Arc<AccountState>>>,
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

    /// 取出（必要时创建）目标状态。
    pub fn target(&self, target_id: &str) -> Arc<TargetState> {
        get_or_insert(&self.targets, target_id, TargetState::new)
    }

    /// 丢弃已经不在配置里的账号与目标状态，避免内存随改配置无限增长。
    pub fn retain(&self, live_accounts: &[String], live_targets: &[String]) {
        if let Ok(mut accounts) = self.accounts.write() {
            accounts.retain(|id, _| live_accounts.iter().any(|live| live == id));
        }
        if let Ok(mut targets) = self.targets.write() {
            targets.retain(|id, _| live_targets.iter().any(|live| live == id));
        }
    }

    /// 管理员改过凭据或手动测试成功后解除账号硬停（§12.3）。
    pub fn clear_account_faults(&self, account_id: &str) {
        let account = self.account(account_id);
        account.key_invalid.store(false, Ordering::Release);
        if let Ok(mut quota) = account.quota.lock() {
            quota.reset();
        }
    }

    /// 清空全部账号与目标状态。备份恢复后调用：账号与目标可能整个换了一批，
    /// 旧的熔断与额度计数不再成立（§23.5）。
    pub fn clear_all(&self) {
        if let Ok(mut accounts) = self.accounts.write() {
            accounts.clear();
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
    let mut guard = map.write().expect("动态状态表被毒化");
    Arc::clone(
        guard
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(make())),
    )
}

/// 账号级状态：Key 失效与额度耗尽影响该 Key 下的所有模型（§12.1）。
pub struct AccountState {
    key_invalid: AtomicBool,
    quota: Mutex<Circuit>,
    budget: Budget,
}

impl AccountState {
    fn new() -> Self {
        Self {
            key_invalid: AtomicBool::new(false),
            quota: Mutex::new(Circuit::default()),
            budget: Budget::new(),
        }
    }

    /// 不改变半开状态的资格检查。真正占用半开试运行名额由 `try_enter` 完成。
    fn check(&self, now: Instant) -> Result<(), Unavailable> {
        if self.key_invalid.load(Ordering::Acquire) {
            return Err(Unavailable::KeyInvalid);
        }
        match self.quota.lock().expect("额度状态被毒化").phase(now) {
            Phase::Cooling | Phase::HalfOpenTaken => Err(Unavailable::QuotaExhausted),
            Phase::Closed | Phase::HalfOpenAvailable => Ok(()),
        }
    }

    /// 真正准入时占用账号级半开试运行名额。
    fn try_enter(&self, now: Instant) -> Result<bool, Unavailable> {
        if self.key_invalid.load(Ordering::Acquire) {
            return Err(Unavailable::KeyInvalid);
        }
        self.quota
            .lock()
            .expect("额度状态被毒化")
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

    /// 用于后台展示的运行状态。
    pub fn status(&self, account: &AccountState) -> TargetStatus {
        if account.key_invalid.load(Ordering::Acquire) {
            return TargetStatus::KeyInvalid;
        }
        let now = Instant::now();
        if account
            .quota
            .lock()
            .expect("额度状态被毒化")
            .is_cooling(now)
        {
            return TargetStatus::QuotaExhausted;
        }
        let circuit = self.circuit.lock().expect("熔断状态被毒化");
        match circuit.phase(now) {
            Phase::Closed => TargetStatus::Active,
            Phase::Cooling => TargetStatus::Cooldown,
            Phase::HalfOpenAvailable | Phase::HalfOpenTaken => TargetStatus::HalfOpen,
        }
    }

    /// 冷却剩余秒数，供后台展示与 `Retry-After`。
    pub fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
        self.circuit
            .lock()
            .expect("熔断状态被毒化")
            .cooldown_remaining(now)
    }

    /// 不消耗任何额度的资格检查，用于 §9.1 的硬性过滤。
    fn check(
        &self,
        account: &AccountState,
        limits: AdmissionLimits,
        now: Instant,
    ) -> Result<(), Unavailable> {
        account.check(now)?;
        match self.circuit.lock().expect("熔断状态被毒化").phase(now) {
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

    fn check(&self, limits: Limits, now: Instant) -> Result<(), Unavailable> {
        if let Some(rpm) = limits.rpm
            && self.rpm.lock().expect("RPM 窗口被毒化").estimate(now) >= f64::from(rpm)
        {
            return Err(Unavailable::RateLimited);
        }
        if let Some(tpm) = limits.tpm
            && self.tpm.lock().expect("TPM 窗口被毒化").estimate(now) >= f64::from(tpm)
        {
            return Err(Unavailable::RateLimited);
        }
        if self.permits.available_permits() == 0 {
            return Err(Unavailable::ConcurrencyFull);
        }
        Ok(())
    }

    /// 让 Semaphore 的容量追上管理员配置的最大并发。
    ///
    /// 缩容时只能回收当前空闲的名额，在途请求归还后由下一次准入继续回收；
    /// 这样永远不会超过旧上限，也总会收敛到新上限。
    fn reconcile_capacity(&self, limits: Limits) {
        let wanted = limits.max_concurrency.unwrap_or(UNLIMITED).max(1);
        let _guard = self.capacity_lock.lock().expect("并发容量状态被毒化");
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
            self.rpm
                .lock()
                .expect("RPM 窗口被毒化")
                .refund_at(1, reservation.at, now);
        }
        if let Some(tokens) = reservation.tokens {
            self.tpm
                .lock()
                .expect("TPM 窗口被毒化")
                .refund_at(tokens, reservation.at, now);
        }
    }

    fn settle(&self, reservation: RateReservation, actual: Option<u64>, now: Instant) {
        if let (Some(reserved), Some(actual)) = (reservation.tokens, actual) {
            let mut tpm = self.tpm.lock().expect("TPM 窗口被毒化");
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

/// 一次已获准的尝试。析构即释放并发名额。
pub struct Admission {
    account: Arc<AccountState>,
    target: Arc<TargetState>,
    /// 名额随本结构体一同释放，不需要显式归还。
    _permit: CapacityPermit,
    /// 本次是否占用了账号级额度熔断的半开试运行名额。
    account_half_open: bool,
    /// 本次是否占用了半开试运行名额。
    half_open: bool,
    account_rate: RateReservation,
    target_rate: RateReservation,
    settled: bool,
}

impl Admission {
    /// 本次是否是熔断后的半开试运行。
    pub fn is_half_open(&self) -> bool {
        self.half_open
    }

    fn release_account_half_open(&self) {
        if self.account_half_open {
            self.account
                .quota
                .lock()
                .expect("额度状态被毒化")
                .release_half_open();
        }
    }

    /// 请求尚未发送到上游（例如倍率终检失败），完整退回本次预留额度。
    pub fn cancel_before_upstream(mut self) {
        self.settled = true;
        let now = Instant::now();
        self.account.budget.cancel(self.account_rate, now);
        self.target.budget.cancel(self.target_rate, now);
        self.release_account_half_open();
        self.target
            .circuit
            .lock()
            .expect("熔断状态被毒化")
            .undo_half_open(self.half_open);
    }

    /// 上报结果并释放半开名额。
    ///
    /// `actual_tokens` 已知时按真实用量归还预留差额；未知（没有 tokenizer 且
    /// 上游没回 usage）时保留保守估算，宁可少发也不要超限（§17.2）。
    pub fn settle(mut self, outcome: Outcome, actual_tokens: Option<u64>) {
        self.settled = true;
        let now = Instant::now();

        self.account
            .budget
            .settle(self.account_rate, actual_tokens, now);
        self.target
            .budget
            .settle(self.target_rate, actual_tokens, now);

        match outcome {
            Outcome::Neutral => {
                self.release_account_half_open();
                self.target
                    .circuit
                    .lock()
                    .expect("熔断状态被毒化")
                    .release_half_open();
            }
            Outcome::Success => {
                self.account.key_invalid.store(false, Ordering::Release);
                self.account
                    .quota
                    .lock()
                    .expect("额度状态被毒化")
                    .on_success(now);
                self.target
                    .circuit
                    .lock()
                    .expect("熔断状态被毒化")
                    .on_success(now);
            }
            Outcome::KeyInvalid => {
                self.account.key_invalid.store(true, Ordering::Release);
                self.release_account_half_open();
                self.target
                    .circuit
                    .lock()
                    .expect("熔断状态被毒化")
                    .release_half_open();
            }
            Outcome::QuotaExhausted { retry_after } => {
                self.account
                    .quota
                    .lock()
                    .expect("额度状态被毒化")
                    .trip(now, retry_after);
                self.target
                    .circuit
                    .lock()
                    .expect("熔断状态被毒化")
                    .release_half_open();
            }
            Outcome::RateLimited { retry_after } => {
                self.release_account_half_open();
                let mut circuit = self.target.circuit.lock().expect("熔断状态被毒化");
                match retry_after {
                    // 上游明确说了多久，就照做，不叠加自己的指数退避。
                    Some(wait) => circuit.trip(now, Some(wait)),
                    // 没给恢复时间的孤立 429 只触发本次切换，够多够密才熔断。
                    None => circuit.on_fault(now),
                }
            }
            Outcome::Fault => {
                self.release_account_half_open();
                self.target
                    .circuit
                    .lock()
                    .expect("熔断状态被毒化")
                    .on_fault(now);
            }
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.target.budget.inflight.fetch_sub(1, Ordering::Relaxed);
        self.account.budget.inflight.fetch_sub(1, Ordering::Relaxed);
        if !self.settled {
            // 客户端断开或任务被取消：半开名额必须还回去，否则这个目标会
            // 一直卡在"有人正在试运行"而永远无法恢复。
            self.release_account_half_open();
            self.target
                .circuit
                .lock()
                .expect("熔断状态被毒化")
                .undo_half_open(self.half_open);
        }
    }
}

impl Registry {
    /// 不消耗额度的资格检查（§9.1）。
    pub fn check<L: Into<AdmissionLimits>>(
        &self,
        account_id: &str,
        target_id: &str,
        limits: L,
    ) -> Result<(), Unavailable> {
        let limits = limits.into();
        let account = self.account(account_id);
        let target = self.target(target_id);
        account.budget.reconcile_capacity(limits.account);
        target.budget.reconcile_capacity(limits.target);
        target.check(&account, limits, Instant::now())
    }

    /// 立即准入：拿不到名额时不等待，由调用方决定换目标还是排队。
    pub fn try_admit<L: Into<AdmissionLimits>>(
        &self,
        account_id: &str,
        target_id: &str,
        limits: L,
        estimated_tokens: u64,
    ) -> Result<Admission, Unavailable> {
        self.admit(account_id, target_id, limits.into(), estimated_tokens, None)
    }

    /// 用排队时已经赢到的并发名额准入。
    ///
    /// 名额是 FIFO 排到的，直接带着它进门才能保证等了 30 秒的请求不会在最后
    /// 一步被刚到的新请求插队；倍率与健康终检仍然照做，名额不能绕过它们。
    pub fn admit_with_permit<L: Into<AdmissionLimits>>(
        &self,
        account_id: &str,
        target_id: &str,
        limits: L,
        estimated_tokens: u64,
        permit: CapacityPermit,
    ) -> Result<Admission, Unavailable> {
        self.admit(
            account_id,
            target_id,
            limits.into(),
            estimated_tokens,
            Some(permit),
        )
    }

    fn admit(
        &self,
        account_id: &str,
        target_id: &str,
        limits: AdmissionLimits,
        estimated_tokens: u64,
        permit: Option<CapacityPermit>,
    ) -> Result<Admission, Unavailable> {
        let account = self.account(account_id);
        let target = self.target(target_id);
        account.budget.reconcile_capacity(limits.account);
        target.budget.reconcile_capacity(limits.target);
        let now = Instant::now();

        // 先看"坏不坏"再看"忙不忙"：一个既熔断又满载的目标必须报熔断，否则
        // 调用方会把它当成"忙"去排队等一个永远不会好的目标。
        let account_half_open = account.try_enter(now)?;
        let half_open = target
            .circuit
            .lock()
            .expect("熔断状态被毒化")
            .try_enter(now)
            .map_err(|()| {
                account
                    .quota
                    .lock()
                    .expect("额度状态被毒化")
                    .undo_half_open(account_half_open);
                Unavailable::Cooling
            })?;

        // 名额先拿，额度后扣：拿不到名额时不能留下已扣的限流计数。
        let permit = match permit {
            Some(permit) => permit,
            None => match self.capacity(account_id, target_id).try_acquire() {
                Ok(permit) => permit,
                Err(_) => {
                    target
                        .circuit
                        .lock()
                        .expect("熔断状态被毒化")
                        .undo_half_open(half_open);
                    account
                        .quota
                        .lock()
                        .expect("额度状态被毒化")
                        .undo_half_open(account_half_open);
                    return Err(Unavailable::ConcurrencyFull);
                }
            },
        };

        let reservations = (|| {
            let account_rate =
                consume_rate(&account.budget, limits.account, estimated_tokens, now)?;
            match consume_rate(&target.budget, limits.target, estimated_tokens, now) {
                Ok(target_rate) => Ok((account_rate, target_rate)),
                Err(reason) => {
                    account.budget.cancel(account_rate, now);
                    Err(reason)
                }
            }
        })();
        let (account_rate, target_rate) = match reservations {
            Ok(reservations) => reservations,
            Err(reason) => {
                target
                    .circuit
                    .lock()
                    .expect("熔断状态被毒化")
                    .undo_half_open(half_open);
                account
                    .quota
                    .lock()
                    .expect("额度状态被毒化")
                    .undo_half_open(account_half_open);
                return Err(reason);
            }
        };

        target.budget.inflight.fetch_add(1, Ordering::Relaxed);
        account.budget.inflight.fetch_add(1, Ordering::Relaxed);
        Ok(Admission {
            account,
            target,
            _permit: permit,
            account_half_open,
            half_open,
            account_rate,
            target_rate,
            settled: false,
        })
    }

    /// 排队必须同时等待账号共享容量与目标局部容量。
    pub fn capacity(&self, account_id: &str, target_id: &str) -> Capacity {
        Capacity {
            account: Arc::clone(&self.account(account_id).budget.permits),
            target: Arc::clone(&self.target(target_id).budget.permits),
        }
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
        && !target
            .rpm
            .lock()
            .expect("RPM 窗口被毒化")
            .try_consume(1, f64::from(rpm), now)
    {
        return Err(Unavailable::RateLimited);
    }
    // TPM 未配置时完全跳过 Token 估算，避免无意义开销（§17.2）。
    if let Some(tpm) = limits.tpm
        && !target.tpm.lock().expect("TPM 窗口被毒化").try_consume(
            estimated_tokens,
            f64::from(tpm),
            now,
        )
    {
        if limits.rpm.is_some() {
            target.rpm.lock().expect("RPM 窗口被毒化").refund(1, now);
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

    #[tokio::test(start_paused = true)]
    async fn isolated_faults_only_switch_but_a_run_of_them_trips_the_breaker() {
        let registry = Registry::new();
        // 孤立错误不熔断：连续 4 次仍然可以继续尝试（§12.3）。
        for _ in 0..CONSECUTIVE_TRIP - 1 {
            let admission = registry
                .try_admit("acc", "tgt", Limits::default(), 0)
                .unwrap();
            admission.settle(Outcome::Fault, None);
            assert!(registry.check("acc", "tgt", Limits::default()).is_ok());
        }

        let admission = registry
            .try_admit("acc", "tgt", Limits::default(), 0)
            .unwrap();
        admission.settle(Outcome::Fault, None);
        assert_eq!(
            registry.check("acc", "tgt", Limits::default()),
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
                .try_admit("acc", "tgt", Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Success, None);
            registry
                .try_admit("acc", "tgt", Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Fault, None);
            if registry.check("acc", "tgt", Limits::default()) == Err(Unavailable::Cooling) {
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
                .try_admit("acc", "tgt", Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Fault, None);
        }
        assert!(
            registry
                .try_admit("acc", "tgt", Limits::default(), 0)
                .is_err()
        );

        tokio::time::advance(COOLDOWN_BASE * 2).await;
        let trial = registry
            .try_admit("acc", "tgt", Limits::default(), 0)
            .unwrap();
        assert!(trial.is_half_open());
        // 只放行一个真实请求：第二个仍然被挡在冷却外（§12.3）。
        assert_eq!(
            registry.try_admit("acc", "tgt", Limits::default(), 0).err(),
            Some(Unavailable::Cooling)
        );

        trial.settle(Outcome::Success, None);
        assert!(registry.check("acc", "tgt", Limits::default()).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_trial_extends_the_cooldown() {
        let registry = Registry::new();
        for _ in 0..CONSECUTIVE_TRIP {
            registry
                .try_admit("acc", "tgt", Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Fault, None);
        }
        tokio::time::advance(COOLDOWN_BASE * 2).await;
        registry
            .try_admit("acc", "tgt", Limits::default(), 0)
            .unwrap()
            .settle(Outcome::Fault, None);

        // 第一次冷却是 2^1，失败后升到 2^2；原来的时长已不足以放行。
        tokio::time::advance(COOLDOWN_BASE * 2).await;
        assert_eq!(
            registry.check("acc", "tgt", Limits::default()),
            Err(Unavailable::Cooling)
        );
        tokio::time::advance(COOLDOWN_BASE * 4).await;
        assert!(registry.check("acc", "tgt", Limits::default()).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_admission_returns_the_trial_slot() {
        let registry = Registry::new();
        for _ in 0..CONSECUTIVE_TRIP {
            registry
                .try_admit("acc", "tgt", Limits::default(), 0)
                .unwrap()
                .settle(Outcome::Fault, None);
        }
        tokio::time::advance(COOLDOWN_BASE * 2).await;

        // 客户端断开：Admission 没有 settle 就被丢弃，名额必须还回去，
        // 否则这个目标会永远停在"有人正在试运行"。
        drop(
            registry
                .try_admit("acc", "tgt", Limits::default(), 0)
                .unwrap(),
        );
        assert!(
            registry
                .try_admit("acc", "tgt", Limits::default(), 0)
                .unwrap()
                .is_half_open()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_invalid_key_pauses_every_model_on_the_account() {
        let registry = Registry::new();
        registry
            .try_admit("acc", "tgt-a", Limits::default(), 0)
            .unwrap()
            .settle(Outcome::KeyInvalid, None);

        // 同一把 Key 下的另一个模型也必须一起停（§12.1）。
        assert_eq!(
            registry.check("acc", "tgt-b", Limits::default()),
            Err(Unavailable::KeyInvalid)
        );
        assert!(!Unavailable::KeyInvalid.is_queueable(), "鉴权失败绝不排队");

        registry.clear_account_faults("acc");
        assert!(registry.check("acc", "tgt-b", Limits::default()).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn quota_exhaustion_honours_the_upstream_recovery_time() {
        let registry = Registry::new();
        registry
            .try_admit("acc", "tgt", Limits::default(), 0)
            .unwrap()
            .settle(
                Outcome::QuotaExhausted {
                    retry_after: Some(Duration::from_secs(30)),
                },
                None,
            );
        assert_eq!(
            registry.check("acc", "tgt", Limits::default()),
            Err(Unavailable::QuotaExhausted)
        );

        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(registry.check("acc", "tgt", Limits::default()).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn quota_half_open_is_reserved_only_by_real_admission() {
        let registry = Registry::new();
        registry
            .try_admit("acc", "tgt", Limits::default(), 0)
            .unwrap()
            .settle(
                Outcome::QuotaExhausted {
                    retry_after: Some(Duration::from_secs(30)),
                },
                None,
            );
        tokio::time::advance(Duration::from_secs(31)).await;

        // 资格检查不能提前占用账号级半开名额，否则后面的真实准入会被自己挡住。
        assert!(registry.check("acc", "tgt", Limits::default()).is_ok());
        let trial = registry
            .try_admit("acc", "tgt", Limits::default(), 0)
            .expect("半开恢复应允许一个真实试运行请求");
        assert_eq!(
            registry.check("acc", "tgt", Limits::default()),
            Err(Unavailable::QuotaExhausted)
        );

        // 取消试运行后，账号级和目标级半开名额都必须归还。
        drop(trial);
        assert!(
            registry
                .try_admit("acc", "tgt", Limits::default(), 0)
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
            .try_admit("acc", "tgt", limits, 50)
            .unwrap()
            .cancel_before_upstream();

        // 终检失败或请求被取消在发往上游前发生时，不应吞掉本次限流预算。
        assert!(registry.try_admit("acc", "tgt", limits, 50).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_with_retry_after_cools_for_exactly_that_long() {
        let registry = Registry::new();
        registry
            .try_admit("acc", "tgt", Limits::default(), 0)
            .unwrap()
            .settle(
                Outcome::RateLimited {
                    retry_after: Some(Duration::from_secs(12)),
                },
                None,
            );
        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(
            registry.check("acc", "tgt", Limits::default()),
            Err(Unavailable::Cooling)
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(registry.check("acc", "tgt", Limits::default()).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn an_isolated_429_without_retry_after_only_switches() {
        let registry = Registry::new();
        registry
            .try_admit("acc", "tgt", Limits::default(), 0)
            .unwrap()
            .settle(Outcome::RateLimited { retry_after: None }, None);
        assert!(
            registry.check("acc", "tgt", Limits::default()).is_ok(),
            "一次没带恢复时间的 429 只该触发本次切换"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_is_capped_and_released_on_drop() {
        let registry = Registry::new();
        let first = registry
            .try_admit("acc", "tgt", limits(Some(1)), 0)
            .unwrap();
        assert_eq!(
            registry.try_admit("acc", "tgt", limits(Some(1)), 0).err(),
            Some(Unavailable::ConcurrencyFull)
        );
        assert!(
            Unavailable::ConcurrencyFull.is_queueable(),
            "并发满是「忙」不是「坏」，应当允许排队"
        );

        drop(first);
        assert!(registry.try_admit("acc", "tgt", limits(Some(1)), 0).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn raising_and_lowering_the_concurrency_cap_takes_effect() {
        let registry = Registry::new();
        let held = registry
            .try_admit("acc", "tgt", limits(Some(1)), 0)
            .unwrap();
        // 提高上限立即生效。
        let second = registry
            .try_admit("acc", "tgt", limits(Some(3)), 0)
            .unwrap();
        let third = registry
            .try_admit("acc", "tgt", limits(Some(3)), 0)
            .unwrap();
        assert_eq!(
            registry.try_admit("acc", "tgt", limits(Some(3)), 0).err(),
            Some(Unavailable::ConcurrencyFull)
        );

        // 降低上限：在途请求不被打断，归还后名额才被真正收回。
        drop(second);
        drop(third);
        assert!(
            registry
                .try_admit("acc", "tgt", limits(Some(1)), 0)
                .is_err()
        );
        drop(held);
        assert!(registry.try_admit("acc", "tgt", limits(Some(1)), 0).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_capacity_reconciliation_does_not_duplicate_permits() {
        let registry = Arc::new(Registry::new());
        assert!(registry.check("acc", "tgt", limits(Some(1))).is_ok());

        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                registry.check("acc", "tgt", limits(Some(8)))
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }

        assert_eq!(
            registry.capacity("acc", "tgt").target.available_permits(),
            8
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
                .try_admit("acc", "tgt", capped, 0)
                .unwrap()
                .settle(Outcome::Success, None);
        }
        assert_eq!(
            registry.try_admit("acc", "tgt", capped, 0).err(),
            Some(Unavailable::RateLimited)
        );

        tokio::time::advance(RATE_WINDOW * 2 + Duration::from_secs(1)).await;
        assert!(registry.try_admit("acc", "tgt", capped, 0).is_ok());
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
            .try_admit("acc", "tgt", capped, 900)
            .unwrap()
            .settle(Outcome::Success, Some(100));
        assert!(registry.try_admit("acc", "tgt", capped, 800).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_admission_does_not_consume_rate_budget() {
        let registry = Registry::new();
        let capped = Limits {
            rpm: Some(10),
            max_concurrency: Some(1),
            ..Limits::default()
        };
        let held = registry.try_admit("acc", "tgt", capped, 0).unwrap();
        for _ in 0..20 {
            assert_eq!(
                registry.try_admit("acc", "tgt", capped, 0).err(),
                Some(Unavailable::ConcurrencyFull)
            );
        }
        drop(held);
        // 被并发挡回的 20 次不能悄悄吃掉 RPM 额度。
        for _ in 0..9 {
            registry
                .try_admit("acc", "tgt", capped, 0)
                .unwrap()
                .settle(Outcome::Success, None);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stale_entries_are_dropped_when_the_configuration_changes() {
        let registry = Registry::new();
        registry
            .try_admit("acc", "tgt", Limits::default(), 0)
            .unwrap()
            .settle(Outcome::Fault, None);
        registry.retain(&[], &[]);
        assert_eq!(registry.targets.read().unwrap().len(), 0);
        assert_eq!(registry.accounts.read().unwrap().len(), 0);
    }
}
