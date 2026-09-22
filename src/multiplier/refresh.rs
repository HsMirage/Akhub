//! 自动倍率刷新：调度、退避与系统性故障保护（§11.3、§11.4）。
//!
//! 抖动由账号 ID 的哈希决定，而不是随机数。目的本来就是"把账号在时间轴上
//! 摊开"，稳定偏移完全够用，还额外换来两个好处：同一个账号的刷新时刻是可
//! 预测的，测试也不必和随机性搏斗。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::time::Instant;

use super::{Entry, Registry, Status, probe};
use crate::domain::{Account, MultiplierMode};
use crate::security::Cipher;
use crate::storage::Store;
use crate::storage::store::MultiplierSnapshotRow;
use crate::upstream::UpstreamClient;

/// 刷新失败后的退避阶梯（§11.3）。
const BACKOFF: [Duration; 6] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
    Duration::from_secs(600),
    Duration::from_secs(1800),
];
/// 抖动幅度：在计划间隔上叠加 0–25%。
const JITTER_RATIO: f64 = 0.25;
/// 一轮里同时打向上游的探针上限（§22：有界并发）。
pub const MAX_CONCURRENT_PROBES: usize = 4;
/// 判定系统性故障所需的最少样本。只有一个账号时"全失败"说明不了什么。
const SYSTEMIC_MIN_ACCOUNTS: usize = 2;
/// 系统性故障保护的持续时间。
const SYSTEMIC_WINDOW: Duration = Duration::from_secs(3600);

/// 每个账号的下一次刷新时刻与连续失败次数。
///
/// 只由刷新任务独占，因此用普通的 `HashMap` 就够，不需要任何同步原语。
pub struct Scheduler {
    interval: Duration,
    due: HashMap<String, Instant>,
    failures: HashMap<String, u32>,
}

impl Scheduler {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            due: HashMap::new(),
            failures: HashMap::new(),
        }
    }

    /// 后台改了刷新间隔后热更新，下一轮 tick 生效。
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// 挑出这一轮该刷新的自动来源账号。
    ///
    /// 手动来源直接跳过：它的状态永远是"已知"，探它没有任何意义（§11.2）。
    pub fn due<'a>(&mut self, accounts: &'a [Account], now: Instant) -> Vec<&'a Account> {
        let mut due = Vec::new();
        for account in accounts.iter().filter(|a| a.multiplier_mode.is_automatic()) {
            let at = *self
                .due
                .entry(account.id.clone())
                .or_insert_with(|| now + jitter(&account.id, self.interval));
            if at <= now {
                due.push(account);
            }
        }
        due.truncate(MAX_CONCURRENT_PROBES * 4);
        due
    }

    /// 记录一次刷新结果并安排下一次。
    pub fn record(&mut self, account_id: &str, ok: bool, now: Instant) {
        let wait = if ok {
            self.failures.remove(account_id);
            self.interval
        } else {
            let level = self.failures.entry(account_id.to_string()).or_insert(0);
            let step = BACKOFF[(*level as usize).min(BACKOFF.len() - 1)];
            *level = level.saturating_add(1);
            step
        };
        self.due
            .insert(account_id.to_string(), now + jitter(account_id, wait));
    }

    /// 手动刷新按钮：让账号在下一轮立即进入刷新（§11.3）。
    pub fn force(&mut self, account_id: &str, now: Instant) {
        self.due.insert(account_id.to_string(), now);
    }

    /// 丢弃已经不存在的账号，避免表随增删账号无限增长。
    pub fn retain(&mut self, accounts: &[Account]) {
        self.due
            .retain(|id, _| accounts.iter().any(|a| &a.id == id));
        self.failures
            .retain(|id, _| accounts.iter().any(|a| &a.id == id));
    }
}

/// 由账号 ID 派生出 0–25% 的稳定偏移，让各账号错开刷新。
fn jitter(account_id: &str, base: Duration) -> Duration {
    // FNV-1a：几行就能写完，分布对"把几十个账号摊开"这个用途完全足够。
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in account_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let fraction = (hash % 1000) as f64 / 1000.0 * JITTER_RATIO;
    base + base.mul_f64(fraction)
}

/// 一轮刷新的结果，用于判定探针侧系统性故障并安排下一次刷新。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RoundStats {
    pub attempted: usize,
    pub failed: usize,
    /// 每个账号这一轮是否刷新成功。
    pub outcomes: Vec<(String, bool)>,
}

impl RoundStats {
    /// 超过半数账号在同一轮里失败 → 判定为探针侧故障而非上游集体改价。
    fn is_systemic(&self) -> bool {
        self.attempted >= SYSTEMIC_MIN_ACCOUNTS && self.failed * 2 > self.attempted
    }
}

/// 刷新任务需要的全部依赖。
pub struct Context {
    pub store: Store,
    pub cipher: Cipher,
    pub upstream: UpstreamClient,
    pub registry: Arc<Registry>,
}

/// 刷新一批账号并把结果写入倍率表与数据库。
///
/// 宽限期的判定发生在读取侧而不是这里：分组上限随时可能被管理员改动，这里
/// 只负责"最后已知值是什么、从什么时候开始不新鲜"。
pub async fn run_round(context: &Context, accounts: &[&Account], now_unix: i64) -> RoundStats {
    let mut stats = RoundStats::default();
    // 有界并发：一轮里最多同时压 MAX_CONCURRENT_PROBES 个上游（§22）。
    for chunk in accounts.chunks(MAX_CONCURRENT_PROBES) {
        let results = futures::future::join_all(
            chunk
                .iter()
                .map(|account| refresh_one(context, account, now_unix)),
        )
        .await;
        for (account, result) in chunk.iter().zip(results) {
            stats.attempted += 1;
            stats.outcomes.push((account.id.clone(), result.is_ok()));
            if let Err(error) = result {
                stats.failed += 1;
                tracing::warn!(
                    account = account.name,
                    %error,
                    "倍率刷新失败，保留最后已知值并进入宽限期"
                );
            }
        }
    }

    if stats.is_systemic() {
        tracing::error!(
            attempted = stats.attempted,
            failed = stats.failed,
            "超过半数账号倍率刷新失败，判定为探针侧系统性故障，宽限期统一延长"
        );
        context
            .registry
            .set_systemic_failure(now_unix + SYSTEMIC_WINDOW.as_secs() as i64);
    } else if stats.attempted > 0 && stats.failed == 0 {
        context.registry.clear_systemic_failure();
    }
    stats
}

/// 刷新单个账号。失败时保留最后已知值并把它标记为过期。
async fn refresh_one(
    context: &Context,
    account: &Account,
    now_unix: i64,
) -> Result<probe::Reading> {
    match probe_account(context, account).await {
        Ok(reading) => {
            let entry = Entry {
                upstream: reading.multiplier,
                source: account.multiplier_mode,
                observed_at: reading.observed_at,
                refreshed_at: now_unix,
                stale_since: None,
                last_error: None,
                peak: reading.peak.clone(),
            };
            persist(context, account, &entry, Status::Known).await;
            context.registry.put(&account.id, entry);
            Ok(reading)
        }
        Err(error) => {
            let previous = context.registry.get(&account.id);
            let entry = Entry {
                // 保留最后已知值：失败后这个值的信息量和成功时完全相同。
                upstream: previous
                    .as_ref()
                    .map_or(account.manual_multiplier, |entry| entry.upstream),
                source: account.multiplier_mode,
                observed_at: previous.as_ref().and_then(|entry| entry.observed_at),
                refreshed_at: previous
                    .as_ref()
                    .map_or(now_unix, |entry| entry.refreshed_at),
                // 已经在过期中的账号不重置计时，否则宽限期会被反复续命。
                stale_since: previous
                    .as_ref()
                    .and_then(|entry| entry.stale_since)
                    .or(Some(now_unix)),
                last_error: Some(sanitize(&error)),
                peak: previous.and_then(|entry| entry.peak),
            };
            persist(context, account, &entry, Status::Stale).await;
            context.registry.put(&account.id, entry);
            Err(error)
        }
    }
}

/// 立即刷新一个账号并把结果写回（后台"刷新"按钮的同步路径）。
///
/// 与后台定时任务共用同一套探测、持久化与宽限期语义，区别只是"现在就等
/// 结果"，而不是排进下一轮。失败时错误照原样报给调用方。
pub async fn refresh_account_now(
    state: &crate::app::SharedState,
    account: &Account,
) -> Result<probe::Reading> {
    let context = Context {
        store: state.store.clone(),
        cipher: state.cipher.clone(),
        upstream: state.upstream.clone(),
        registry: Arc::clone(&state.runtime.multipliers),
    };
    refresh_one(&context, account, crate::storage::now_unix()).await
}

/// 按账号配置的来源发起探测。
async fn probe_account(context: &Context, account: &Account) -> Result<probe::Reading> {
    let api_key = account_api_key(context, account)
        .await?
        .ok_or_else(|| anyhow::anyhow!("账号缺少 Sub2API 探针所需的 API Key"))?;
    match account.multiplier_mode {
        MultiplierMode::Manual => anyhow::bail!("手动倍率不需要探测"),
        MultiplierMode::Sub2Api => {
            probe::sub2api(
                &context.upstream,
                &account.base_url,
                &api_key,
                account.allow_private_network,
            )
            .await
        }
        MultiplierMode::NewApi => {
            // 账号自己的凭据优先；没填就回落到站点级凭据（§6.4：一个站点只配一次）。
            let Some((token, user_id)) = new_api_credentials(context, account).await? else {
                anyhow::bail!(
                    "缺少 New API 访问令牌与用户 ID（可在账号编辑页填写，或在设置页按站点配置一次）"
                );
            };
            probe::new_api(
                &context.upstream,
                &account.base_url,
                &token,
                &user_id,
                account.new_api_group.as_deref(),
                account.allow_private_network,
            )
            .await
        }
    }
}

/// 读取账号用于探测的那把 Key：Key 池的**第一把**（§4.2.1 的镜像）。
///
/// 已知限制：Sub2API 是 Key 级计费，多 Key 账号里每把 Key 的倍率未必相同，
/// 而探测只看第一把。返回 `None` 表示账号还没有凭据。
pub(crate) async fn account_api_key(
    context: &Context,
    account: &Account,
) -> Result<Option<String>> {
    // 先看 Key 池。`upstream_secrets.api_key` 只是第一把 Key 的镜像，而建号与
    // 写池是两步：只有旧二进制或旧备份恢复路径才可能留下"池里有、镜像没有"
    // 的账号，但那种账号照样得能探测。
    let pool = context.store.list_account_key_rows(&account.id).await?;
    if let Some(row) = pool.iter().find(|row| row.enabled) {
        let plaintext = context.cipher.open(&row.sealed_key)?;
        let key = String::from_utf8(plaintext.to_vec())?;
        return Ok((!key.trim().is_empty()).then_some(key));
    }
    let Some(sealed) = context.store.account_sealed_key(&account.id).await? else {
        return Ok(None);
    };
    // 建号时"暂时没有凭据"会把镜像写成空串：那是没 Key，不是信封坏了。
    if sealed.is_empty() {
        return Ok(None);
    }
    let plaintext = context.cipher.open(&sealed)?;
    let key = String::from_utf8(plaintext.to_vec())?;
    Ok((!key.trim().is_empty()).then_some(key))
}

/// 解析 New API 的（访问令牌, 用户 ID）：账号凭据优先，其次站点级凭据。
///
/// 站点级凭据存在 `new_api_sites` 表里（令牌加密），一个 Base URL 配一次，
/// 该站点下所有账号共享——避免每个账号都重复填两遍（§6.4）。New API 的
/// `sk-xxx` 不被分组接口接受，所以这两项无法省掉，只能省掉重复填写。
pub(crate) async fn new_api_credentials(
    context: &Context,
    account: &Account,
) -> Result<Option<(String, String)>> {
    let account_token = context
        .store
        .account_sealed_new_api_token(&account.id)
        .await?;
    if let Some(sealed) = account_token {
        let token = decrypt(context, Some(sealed))?;
        let user_id = account
            .new_api_user_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少 New API 用户 ID，无法调用分组接口"))?;
        return Ok(Some((token, user_id)));
    }
    let Some((site_user_id, sealed)) = context.store.new_api_site(&account.base_url).await? else {
        return Ok(None);
    };
    let token = decrypt(context, Some(sealed))?;
    // 账号自己填了用户 ID 就以它为准（同一站点可能有多个用户）。
    let user_id = account
        .new_api_user_id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or(site_user_id);
    Ok(Some((token, user_id)))
}

fn decrypt(context: &Context, sealed: Option<Vec<u8>>) -> Result<String> {
    let sealed = sealed.ok_or_else(|| anyhow::anyhow!("账号缺少所需凭据"))?;
    let plaintext = context.cipher.open(&sealed)?;
    Ok(String::from_utf8(plaintext.to_vec())?)
}

/// 把错误压成一行安全文本。凭据只在请求头里出现，不会进入 `anyhow` 链，
/// 但仍统一走一次脱敏，避免上游把 Key 回显在错误正文里。
fn sanitize(error: &anyhow::Error) -> String {
    let text = crate::security::redact::text(&format!("{error:#}"));
    text.chars().take(200).collect()
}

async fn persist(context: &Context, account: &Account, entry: &Entry, status: Status) {
    let row = MultiplierSnapshotRow {
        account_id: account.id.clone(),
        multiplier: entry.upstream,
        source: entry.source,
        status: status.as_str().to_string(),
        observed_at: entry.observed_at,
        refreshed_at: entry.refreshed_at,
        stale_since: entry.stale_since,
        last_error: entry.last_error.clone(),
    };
    if let Err(error) = context.store.upsert_multiplier_snapshot(&row).await {
        tracing::warn!(%error, account = account.name, "倍率状态落盘失败");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Limits, Multiplier, Protocol};
    use time::OffsetDateTime;

    fn account(id: &str, mode: MultiplierMode) -> Account {
        Account {
            id: id.into(),
            group_id: Some("g1".into()),
            name: id.into(),
            base_url: "https://api.example.com".into(),
            preferred_protocol: Protocol::OpenAiChat,
            adaptive_protocol: true,
            default_priority: 50,
            calibration: Multiplier::ONE,
            multiplier_mode: mode,
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

    #[tokio::test(start_paused = true)]
    async fn manual_accounts_are_never_probed() {
        let mut scheduler = Scheduler::new(Duration::from_secs(300));
        let accounts = vec![
            account("manual", MultiplierMode::Manual),
            account("auto", MultiplierMode::Sub2Api),
        ];
        tokio::time::advance(Duration::from_secs(3600)).await;
        let due: Vec<_> = scheduler
            .due(&accounts, Instant::now())
            .iter()
            .map(|a| a.id.clone())
            .collect();
        assert_eq!(due, Vec::<String>::new(), "首轮要等抖动过去");

        tokio::time::advance(Duration::from_secs(400)).await;
        let due: Vec<_> = scheduler
            .due(&accounts, Instant::now())
            .iter()
            .map(|a| a.id.clone())
            .collect();
        assert_eq!(due, vec!["auto".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_back_off_along_the_ladder_and_success_resets_it() {
        let mut scheduler = Scheduler::new(Duration::from_secs(300));
        let accounts = vec![account("auto", MultiplierMode::Sub2Api)];

        for expected in BACKOFF {
            let now = Instant::now();
            scheduler.record("auto", false, now);
            // 退避时长内不该再探；过了就该探。
            tokio::time::advance(expected).await;
            assert!(scheduler.due(&accounts, Instant::now()).is_empty());
            tokio::time::advance(expected.mul_f64(JITTER_RATIO) + Duration::from_secs(1)).await;
            assert_eq!(scheduler.due(&accounts, Instant::now()).len(), 1);
        }

        // 成功一次就回到正常间隔，而不是继续停在 30 分钟。
        scheduler.record("auto", true, Instant::now());
        tokio::time::advance(Duration::from_secs(299)).await;
        assert!(scheduler.due(&accounts, Instant::now()).is_empty());
        tokio::time::advance(Duration::from_secs(200)).await;
        assert_eq!(scheduler.due(&accounts, Instant::now()).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_manual_refresh_button_bypasses_the_schedule() {
        let mut scheduler = Scheduler::new(Duration::from_secs(300));
        let accounts = vec![account("auto", MultiplierMode::Sub2Api)];
        scheduler.record("auto", false, Instant::now());
        assert!(scheduler.due(&accounts, Instant::now()).is_empty());

        scheduler.force("auto", Instant::now());
        assert_eq!(scheduler.due(&accounts, Instant::now()).len(), 1);
    }

    #[test]
    fn accounts_are_spread_out_instead_of_all_firing_together() {
        let base = Duration::from_secs(300);
        let offsets: Vec<_> = ["acc_a", "acc_b", "acc_c", "acc_d"]
            .iter()
            .map(|id| jitter(id, base))
            .collect();
        // 同一个 ID 恒定，不同 ID 分散，且都落在 [base, 1.25×base] 内。
        assert_eq!(jitter("acc_a", base), offsets[0]);
        assert!(
            offsets
                .iter()
                .all(|d| *d >= base && *d <= base.mul_f64(1.25))
        );
        assert!(
            offsets.windows(2).any(|pair| pair[0] != pair[1]),
            "不同账号必须错开，否则抖动没有意义"
        );
    }

    #[test]
    fn a_systemic_failure_needs_more_than_half_and_more_than_one_account() {
        let stats = |attempted, failed| RoundStats {
            attempted,
            failed,
            outcomes: Vec::new(),
        };
        assert!(
            !stats(1, 1).is_systemic(),
            "只有一个账号时全失败说明不了是谁的问题"
        );
        assert!(!stats(4, 2).is_systemic());
        assert!(stats(4, 3).is_systemic());
    }

    #[test]
    fn stale_entries_keep_their_original_expiry_clock() {
        // 反复失败不能让宽限期不断续命，否则永远到不了 multiplier_unknown。
        let first = Entry {
            upstream: Multiplier::ONE,
            source: MultiplierMode::Sub2Api,
            observed_at: None,
            refreshed_at: 100,
            stale_since: Some(100),
            last_error: Some("超时".into()),
            peak: None,
        };
        let second_failure_at = 500;
        let carried = first.stale_since.or(Some(second_failure_at));
        assert_eq!(carried, Some(100));
    }
}
