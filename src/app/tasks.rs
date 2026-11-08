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

use super::supervisor::supervise;
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
/// 性能快照的保鲜期：超过就宁可丢弃（§20.1）。
const SNAPSHOT_HORIZON: i64 = 24 * 3600;
/// 单次清理的删除批量，避免长事务阻塞写入。
const PRUNE_BATCH: i64 = 5_000;
/// 每轮清理最多归还多少个空闲页（4 KiB 一页，约 40 MB）。
const VACUUM_PAGES: i64 = 10_000;

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
///
/// 周期任务都在 [`supervise`] 下运行：一次 panic 只会让它稍后重启，不会让
/// 清理、快照或刷新从此停掉。
pub fn spawn(state: &SharedState) {
    let weak = Arc::downgrade(state);
    supervise("快照落盘", {
        let weak = weak.clone();
        move || snapshots(weak.clone())
    });
    supervise("倍率刷新", {
        let weak = weak.clone();
        let handle = state.refresh.clone();
        move || multiplier_refresh(weak.clone(), handle.clone())
    });
    supervise("模型同步", {
        let weak = weak.clone();
        move || model_sync(weak.clone())
    });
    supervise("过期清理", move || cleanup(weak.clone()));
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
            state.runtime.multipliers.set_refresh_interval(interval);
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
        cleanup_once(&state, crate::storage::now_unix()).await;
    }
}

/// 一轮清理。每类过期数据都**分批**删到删完：一次删几十万行会长时间占着
/// 写锁，而只删一批又会让积压永远追不上。
pub(crate) async fn cleanup_once(state: &AppState, now: i64) {
    // 保留期为 0 表示"不保留历史明细"：记录器已经不再落库，但调成 0 之前
    // 攒下的历史也必须清掉，而不是永远留在库里（§24.2）。
    let retention_days = state.settings.get().retention_days;
    let history_cutoff = now - i64::from(retention_days) * 86_400;

    let store = &state.store;
    prune_all("过期请求记录", || {
        store.prune_request_records(history_cutoff, PRUNE_BATCH)
    })
    .await;

    state.sessions.purge_expired();

    let config = state.config.current();
    let (_, targets) = config.live_ids();
    state.runtime.sticky.prune(&targets, now);
    drop(config);

    let sticky_cutoff = now - crate::routing::sticky::TTL.as_secs() as i64;
    if let Err(error) = store.prune_sticky_bindings(sticky_cutoff).await {
        tracing::warn!(%error, "清理粘性绑定失败");
    }
    if let Err(error) = store.prune_perf_snapshots(now - SNAPSHOT_HORIZON).await {
        tracing::warn!(%error, "清理性能快照失败");
    }
    prune_all("过期 Responses 状态", || {
        store.prune_response_states(now, PRUNE_BATCH)
    })
    .await;
    // 生图任务的定位记录按自己的到期时间清理（§14.9）：它不是保留期
    // 概念，而是"这个任务 ID 还能不能问"。
    prune_all("过期异步生图任务定位", || {
        store.prune_image_tasks(now, PRUNE_BATCH)
    })
    .await;
    // 托管后台任务与响应状态共用保留期（计划 §29.1）。
    prune_all("过期托管后台任务", || {
        store.prune_background_tasks(now, PRUNE_BATCH)
    })
    .await;

    match store.incremental_vacuum(VACUUM_PAGES).await {
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "增量回收空闲页失败"),
    }
}

/// 反复执行一批删除，直到某一批不满。批与批之间让出调度，给热路径的写入
/// 插队的机会。
async fn prune_all<F, Fut>(what: &str, mut batch: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<u64>>,
{
    let mut total = 0u64;
    loop {
        match batch().await {
            Ok(removed) => {
                total += removed;
                if removed < PRUNE_BATCH as u64 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            Err(error) => {
                tracing::warn!(%error, "清理{what}失败");
                break;
            }
        }
    }
    if total > 0 {
        tracing::info!(removed = total, "已清理{what}");
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
                // 后台自身的探针不进入目标质量判断（§9.3）。
                kind: crate::routing::score::SampleKind::Excluded,
                success: true,
                first_token: None,
                total: Duration::from_millis(1200),
                output_tokens: Some(100),
                cache_hit: None,
            },
            crate::storage::now_unix(),
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
                    key_id: None,
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

    /// 保留期设成 0 之后，之前攒下的请求明细也要清掉（§24.2）。
    #[tokio::test]
    async fn zero_retention_still_purges_existing_history() {
        let state = state().await;
        let mut settings = (*state.settings.get()).clone();
        settings.retention_days = 0;
        state.settings.replace(settings);
        let now = crate::storage::now_unix();
        let mut old = crate::app::recorder::tests::record("old");
        old.started_at = now - 3600;
        state.store.insert_request_records(&[old]).await.unwrap();
        cleanup_once(&state, now).await;
        assert!(
            state
                .store
                .list_request_records(10, 0)
                .await
                .unwrap()
                .is_empty(),
            "保留期 0 时历史明细必须被清掉"
        );
    }

    #[tokio::test]
    async fn the_refresh_handle_is_safe_before_the_task_starts() {
        // 后台任务还没来得及注册 scheduler 时按下刷新按钮不能 panic。
        RefreshHandle::default().force("acc");
    }
}
