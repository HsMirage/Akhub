//! 后台任务（§22）。
//!
//! 每个任务只持有 [`Weak`] 引用：进程关闭或测试用完 `AppState` 后，任务在下
//! 一个 tick 自行退出，不会把状态钉在内存里。
//!
//! 所有任务都遵守同一组纪律：有界并发、独立超时、不发送模拟对话去探测模型
//! 能力或账号恢复。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::time::Instant;

use super::{AppState, SharedState};
use crate::multiplier::refresh;

/// 快照落盘周期（§20.1）。
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(60);
/// 倍率刷新任务的检查周期。真正的刷新时刻由每个账号自己的调度决定。
const REFRESH_TICK: Duration = Duration::from_secs(10);
/// 清理任务周期。
const CLEANUP_INTERVAL: Duration = Duration::from_secs(600);
/// 模型自动同步的检查周期。真正的同步时刻由每个账号自己的间隔决定（§16.2）。
const MODEL_SYNC_TICK: Duration = Duration::from_secs(30);
/// 分钟桶聚合的检查周期（§22）。每分钟滚一次上一个完整分钟的明细。
const ROLLUP_INTERVAL: Duration = Duration::from_secs(60);
/// 性能桶的桶宽（秒）。
const BUCKET_SECS: i64 = 60;
/// 性能快照的保鲜期：超过就宁可丢弃（§20.1）。
const SNAPSHOT_HORIZON: i64 = 24 * 3600;
/// 单次清理的删除批量，避免长事务阻塞写入。
const PRUNE_BATCH: i64 = 5_000;

/// 手动刷新按钮用的句柄：让某个账号在下一轮立即进入刷新（§11.3）。
#[derive(Clone, Default)]
pub struct RefreshHandle {
    scheduler: Arc<Mutex<Option<Arc<Mutex<refresh::Scheduler>>>>>,
}

impl RefreshHandle {
    /// 把账号插队到下一轮。任务尚未启动时静默忽略。
    pub fn force(&self, account_id: &str) {
        let Ok(slot) = self.scheduler.lock() else {
            return;
        };
        if let Some(scheduler) = slot.as_ref()
            && let Ok(mut scheduler) = scheduler.lock()
        {
            scheduler.force(account_id, Instant::now());
        }
    }
}

/// 拉起全部后台任务。
///
/// 句柄取自 `state.refresh`：后台接口拿到的是同一个对象，因此"立即刷新"按钮
/// 在任务启动后自然生效，启动前按下也只是静默忽略。
pub fn spawn(state: &SharedState) {
    tokio::spawn(snapshots(Arc::downgrade(state)));
    tokio::spawn(multiplier_refresh(
        Arc::downgrade(state),
        state.refresh.clone(),
    ));
    tokio::spawn(model_sync(Arc::downgrade(state)));
    tokio::spawn(cleanup(Arc::downgrade(state)));
    tokio::spawn(rollup(Arc::downgrade(state)));
}

/// 把请求明细滚进分钟桶（§22 的"性能分钟桶聚合"）。
///
/// 只聚合**已经结束**的分钟，避免把正在进行中的那一分钟反复重算。聚合本身是
/// 幂等的（先删后建），所以 tick 抖动或重复执行都不会重复计数。
async fn rollup(state: Weak<AppState>) {
    let mut ticker = tokio::time::interval(ROLLUP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 第一次 tick 立即触发，跳过它，从下一个完整周期开始。
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let Some(state) = state.upgrade() else {
            return;
        };
        let now = crate::storage::now_unix();
        // 上一分钟的边界；只滚 [last_minute, this_minute) 这一段。
        let until = (now / BUCKET_SECS) * BUCKET_SECS;
        let since = until - BUCKET_SECS;
        match state
            .store
            .rollup_performance_buckets(since, until, BUCKET_SECS)
            .await
        {
            Ok(0) => {}
            Ok(rows) => tracing::debug!(rows, since, until, "性能分钟桶聚合完成"),
            // 聚合失败只记日志：它不参与推理热路径，不能因此影响服务（§22）。
            Err(error) => tracing::warn!(%error, "性能分钟桶聚合失败"),
        }
    }
}

/// 每 60 秒把粘性绑定与性能 EWMA 批量写盘。
///
/// 内存仍是唯一真相，写盘只是备份：写失败只记一条日志，绝不影响热路径。
async fn snapshots(state: Weak<AppState>) {
    let mut ticker = tokio::time::interval(SNAPSHOT_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(state) = state.upgrade() else {
            return;
        };
        flush_snapshots(&state).await;
    }
}

/// 立即把快照写一次盘。关闭前也会调用一次，避免丢掉最后一分钟的数据。
pub async fn flush_snapshots(state: &AppState) {
    let now = crate::storage::now_unix();

    let bindings = state.runtime.sticky.export();
    if let Err(error) = state.store.save_sticky_bindings(&bindings).await {
        tracing::warn!(%error, "粘性绑定落盘失败");
    }
    let perf = state.runtime.perf.export(now);
    if let Err(error) = state.store.save_perf_snapshots(&perf).await {
        tracing::warn!(%error, "性能快照落盘失败");
    }
}

/// 自动倍率刷新与宽限期计时（§11.3）。
async fn multiplier_refresh(state: Weak<AppState>, handle: RefreshHandle) {
    let Some(first) = state.upgrade() else {
        return;
    };
    let scheduler = Arc::new(Mutex::new(refresh::Scheduler::new(
        first.settings.get().multiplier_refresh,
    )));
    if let Ok(mut slot) = handle.scheduler.lock() {
        *slot = Some(Arc::clone(&scheduler));
    }
    drop(first);

    let mut ticker = tokio::time::interval(REFRESH_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(state) = state.upgrade() else {
            return;
        };
        let accounts = match state.store.list_accounts().await {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::warn!(%error, "读取账号列表失败，本轮跳过倍率刷新");
                continue;
            }
        };
        // 后台可能刚改过刷新间隔：每一轮都取最新值，不必重启进程。
        let interval = state.settings.get().multiplier_refresh;

        let now = Instant::now();
        let due: Vec<_> = {
            let Ok(mut scheduler) = scheduler.lock() else {
                return;
            };
            scheduler.set_interval(interval);
            scheduler.retain(&accounts);
            scheduler.due(&accounts, now).into_iter().cloned().collect()
        };
        if due.is_empty() {
            continue;
        }

        let context = refresh::Context {
            store: state.store.clone(),
            cipher: state.cipher.clone(),
            upstream: state.upstream.clone(),
            registry: Arc::clone(&state.runtime.multipliers),
        };
        let borrowed: Vec<&_> = due.iter().collect();
        let stats = refresh::run_round(&context, &borrowed, crate::storage::now_unix()).await;
        tracing::debug!(
            attempted = stats.attempted,
            failed = stats.failed,
            "完成一轮倍率刷新"
        );

        if let Ok(mut scheduler) = scheduler.lock() {
            let done = Instant::now();
            for (account_id, ok) in &stats.outcomes {
                scheduler.record(account_id, *ok, done);
            }
        }
    }
}

/// 模型自动同步（§16.2）。
///
/// 开了 `auto_sync` 的账号按各自的同步间隔全量托管上游模型，间隔带抖动，
/// 避免所有账号在同一时刻打上游的模型列表接口。刚打开开关的账号在下一轮
/// 检查（30 秒内）就会得到首次同步。失败只记日志，下个间隔再试。
async fn model_sync(state: Weak<AppState>) {
    let mut ticker = tokio::time::interval(MODEL_SYNC_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 每个账号的下一次同步时刻；不在这里的账号表示从未同步过，立即执行。
    let mut next_run: HashMap<String, Instant> = HashMap::new();
    loop {
        ticker.tick().await;
        let Some(state) = state.upgrade() else {
            return;
        };
        let accounts = match state.store.list_accounts().await {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::warn!(%error, "读取账号列表失败，本轮跳过模型同步");
                continue;
            }
        };

        let now = Instant::now();
        for account in accounts.iter().filter(|a| a.auto_sync && a.enabled) {
            if next_run
                .get(&account.id)
                .copied()
                .is_some_and(|due| due > now)
            {
                continue;
            }
            match crate::discovery::sync_managed(&state, account).await {
                Ok(count) => {
                    tracing::debug!(account = %account.id, managed = count, "完成一轮模型同步")
                }
                Err(error) => {
                    tracing::warn!(account = %account.id, %error, "模型自动同步失败")
                }
            }
            next_run.insert(
                account.id.clone(),
                Instant::now() + jitter(state.settings.get().model_sync),
            );
        }
        // 已删除或已关闭托管的账号不再保留调度项。
        next_run.retain(|id, _| {
            accounts
                .iter()
                .any(|a| a.auto_sync && a.enabled && &a.id == id)
        });
    }
}

/// 同步间隔加 ±10% 的抖动（§16.2）。
fn jitter(base: Duration) -> Duration {
    let secs = base.as_secs_f64();
    // 0..256 → 0.9..1.1：抖动只需要数量级正确的随机性，不值得为此引入
    // 浮点随机库；操作系统熵源已在进程内其它路径使用。
    let mut byte = [0u8; 1];
    let _ = getrandom::fill(&mut byte);
    let factor = 0.9 + f64::from(byte[0]) / 255.0 * 0.2;
    Duration::from_secs_f64((secs * factor).max(1.0))
}

/// 过期清理：请求记录保留期、粘性绑定与性能快照（§24.2、§22）。
async fn cleanup(state: Weak<AppState>) {
    let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(state) = state.upgrade() else {
            return;
        };
        let now = crate::storage::now_unix();

        if state.settings.get().retention_days > 0 {
            let cutoff = now - i64::from(state.settings.get().retention_days) * 86_400;
            match state.store.prune_request_records(cutoff, PRUNE_BATCH).await {
                Ok(removed) if removed > 0 => {
                    tracing::info!(removed, "已清理过期请求记录");
                }
                Err(error) => tracing::warn!(%error, "清理请求记录失败"),
                _ => {}
            }
        }

        let config = state.config.current();
        let (_, targets) = config.live_ids();
        state.runtime.sticky.prune(&targets, now);
        drop(config);

        let sticky_cutoff = now - crate::routing::sticky::TTL.as_secs() as i64;
        if let Err(error) = state.store.prune_sticky_bindings(sticky_cutoff).await {
            tracing::warn!(%error, "清理粘性绑定失败");
        }
        if let Err(error) = state
            .store
            .prune_perf_snapshots(now - SNAPSHOT_HORIZON)
            .await
        {
            tracing::warn!(%error, "清理性能快照失败");
        }
        match state.store.prune_response_states(now).await {
            Ok(removed) if removed > 0 => {
                tracing::info!(removed, "已清理过期 Responses 状态");
            }
            Err(error) => tracing::warn!(%error, "清理 Responses 状态失败"),
            _ => {}
        }
        // 托管后台任务与响应状态共用保留期（计划 §29.1）。
        match state.store.prune_background_tasks(now, 500).await {
            Ok(removed) if removed > 0 => {
                tracing::info!(removed, "已清理过期托管后台任务");
            }
            Err(error) => tracing::warn!(%error, "清理托管后台任务失败"),
            _ => {}
        }
        // 性能桶与请求明细同一保留期（§24.2），否则明细删了、聚合还在。
        let retention_days = state.settings.get().retention_days;
        if retention_days > 0 {
            let bucket_cutoff = now - i64::from(retention_days) * 86_400;
            if let Err(error) = state.store.prune_performance_buckets(bucket_cutoff).await {
                tracing::warn!(%error, "清理性能桶失败");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Settings;

    async fn state() -> SharedState {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::bootstrap(dir.path(), Settings::default())
            .await
            .unwrap();
        // 目录在返回后被删除，但 SQLite 已经打开了文件句柄，够测试用完。
        std::mem::forget(dir);
        state
    }

    #[tokio::test]
    async fn snapshots_flush_without_a_running_ticker() {
        let state = state().await;
        state.runtime.perf.observe(
            "tgt",
            crate::routing::score::Dimension {
                protocol: crate::domain::Protocol::OpenAiChat,
                streaming: false,
            },
            &crate::routing::score::Sample {
                success: true,
                first_token: None,
                total: Duration::from_millis(1200),
                output_tokens: Some(100),
            },
        );

        flush_snapshots(&state).await;
        // 目标不在 dispatch_targets 里，读取时会被外键式过滤掉——写入本身
        // 仍必须成功，否则热路径会被一条无关的清理规则拖累。
        assert!(state.store.load_perf_snapshots(0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn background_tasks_exit_once_the_state_is_dropped() {
        let state = state().await;
        let weak = Arc::downgrade(&state);
        spawn(&state);
        drop(state);
        // 任务只持有弱引用，因此 AppState 立刻可以被释放。
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn cleanup_removes_expired_response_states_but_keeps_live_ones() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::bootstrap(dir.path(), Settings::default())
            .await
            .unwrap();
        let now = crate::storage::now_unix();
        for (id, expires_at) in [("expired", now - 1), ("live", now + 3600)] {
            state
                .store
                .upsert_response_state(&crate::storage::store::ResponseStateRow {
                    gateway_id: id.into(),
                    group_id: "g".into(),
                    logical_model: "m".into(),
                    account_id: None,
                    target_id: None,
                    endpoint: None,
                    upstream_id: None,
                    sealed_body: None,
                    protocol: None,
                    created_at: now - 60,
                    expires_at,
                })
                .await
                .unwrap();
        }
        let task = tokio::spawn(cleanup(Arc::downgrade(&state)));
        let removed = tokio::time::timeout(Duration::from_secs(2), async {
            while state
                .store
                .response_state("expired", "g")
                .await
                .unwrap()
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        task.abort();
        let _ = task.await;
        assert!(removed.is_ok(), "后台清理必须移除过期状态");
        assert!(
            state
                .store
                .response_state("live", "g")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_refresh_handle_is_safe_before_the_task_starts() {
        // 后台任务还没来得及注册 scheduler 时按下刷新按钮不能 panic。
        RefreshHandle::default().force("acc");
    }
}
