//! 网关托管的后台任务（计划 §29.1）。
//!
//! 第一期的口径是"只代理原生后台，上游不支持就明确拒绝"。这里补上第二期的
//! 自托管路径，并且明确修掉参考实现（LiteLLM `polling_via_cache`）的三处缺陷：
//!
//! 1. **取消是真的**：任务句柄登记在内存表里，取消时先 `abort()` 上游连接
//!    （流式路径 drop 掉 `reqwest::Response` 就会断开），再把状态写成
//!    `cancelled`；不像参考实现那样只改一个状态字段、上游继续跑。
//! 2. **重启不骗人**：状态与执行者都在 SQLite。启动时扫描 `queued`/`running`
//!    的遗留任务，统一标记 `interrupted` 并写明原因；客户端拿到的是明确失败，
//!    绝不会看到永远的 `in_progress`。
//! 3. **不依赖 Redis**：单实例 + SQLite 即可工作。
//!
//! 触发条件由分组开关 `allow_managed_background` 控制：默认关闭，只有管理员
//! 明确打开、且目标上游确实不支持原生后台时，才会走这里。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::storage::store::BackgroundTaskRow;

/// 任务 ID 前缀：与原生代理的 `resp_akh_*` 明确区分（计划 §29.1）。
pub const BACKGROUND_PREFIX: &str = "bg_akh_";

/// 任务状态。
pub mod status {
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
    /// 网关重启导致的中断：客户端据此知道任务不会再有进展。
    pub const INTERRUPTED: &str = "interrupted";
}

/// 生成一个托管任务 ID。
pub fn new_id() -> String {
    format!("{BACKGROUND_PREFIX}{}", ulid::Ulid::generate())
}

/// 判断一个 ID 是不是网关托管的任务。
pub fn is_managed(id: &str) -> bool {
    id.starts_with(BACKGROUND_PREFIX)
}

/// 在跑任务的取消句柄表。
///
/// 只保存"能中断上游连接"的句柄；任务结束时移除。进程重启后这张表是空的，
/// 这正是为什么重启时必须把遗留任务标成 `interrupted`——没有任何东西能恢复
/// 一个已经不在进程里的连接。
#[derive(Default)]
pub struct RunningTasks {
    handles: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl RunningTasks {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个正在执行的任务。
    pub async fn register(&self, id: &str, handle: tokio::task::JoinHandle<()>) {
        let mut handles = self.handles.lock().await;
        // 同一 ID 重复登记时先把旧任务中止，避免两个执行者同时推进一条任务。
        if let Some(previous) = handles.insert(id.to_string(), handle) {
            previous.abort();
        }
    }

    /// 任务自然结束时移除登记。
    pub async fn finish(&self, id: &str) {
        self.handles.lock().await.remove(id);
    }

    /// **真正取消**：中止任务，从而 drop 掉上游响应、断开连接。
    ///
    /// 返回是否确实有一个在跑的任务被中止。任务已经结束或从未登记时返回
    /// `false`，调用方据此区分"取消了一个在跑的"与"只是改了个状态"。
    pub async fn cancel(&self, id: &str) -> bool {
        let handle = self.handles.lock().await.remove(id);
        match handle {
            Some(handle) => {
                handle.abort();
                true
            }
            None => false,
        }
    }

    /// 当前在跑的任务数，供概览与测试使用。
    pub async fn len(&self) -> usize {
        self.handles.lock().await.len()
    }

    /// 是否没有在跑的任务。
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// 启动时处理遗留任务（计划 §29.1 的"重启不骗人"）。
///
/// 返回被标记为 `interrupted` 的任务数。判定依据是心跳：超过
/// `stale_after` 没有心跳的 `queued`/`running` 任务不可能是活的。
///
/// 只依赖 `Store`，所以可以在 `AppState` 组装完成之前调用。
pub async fn recover_stale(
    store: &crate::storage::Store,
    stale_after: Duration,
) -> anyhow::Result<u64> {
    let now = crate::storage::now_unix();
    let stale_before = now - stale_after.as_secs() as i64;
    let affected = store.stall_background_tasks(stale_before, now).await?;
    if affected > 0 {
        tracing::warn!(
            count = affected,
            "发现上次进程遗留的托管后台任务，已标记为 interrupted（不会再有进展）"
        );
    }
    Ok(affected)
}

/// 托管任务对外可见的状态（OpenAI Response 风格）。
pub fn public_status(status: &str) -> &'static str {
    match status {
        status::QUEUED => "queued",
        status::RUNNING => "in_progress",
        status::COMPLETED => "completed",
        status::CANCELLED => "cancelled",
        // 失败与中断对客户端都表现为 failed，但 error_code 区分原因。
        _ => "failed",
    }
}

/// 把一条任务记录转成客户端可见的响应对象。
///
/// 只暴露网关自己的 ID；输出正文在 `sealed_output` 里（加密），由调用方解密
/// 后填入 `output`。取消过的任务带上"上游可能已计费"的提示（计划 §29.1）。
pub fn public_object(task: &BackgroundTaskRow, gateway_url_id: &str) -> serde_json::Value {
    let mut object = serde_json::json!({
        "id": gateway_url_id,
        "object": "response",
        "model": task.logical_model,
        "status": public_status(&task.status),
        "created_at": task.created_at,
        "background": true,
        "output": [],
    });
    if let Some(map) = object.as_object_mut() {
        if let Some(code) = task.error_code.as_deref() {
            map.insert("error".into(), serde_json::json!({"code": code}));
        }
        if task.status == status::CANCELLED {
            map.insert(
                "cancellation_note".into(),
                serde_json::json!("已中断上游连接；取消前上游可能已经产生费用"),
            );
        }
    }
    object
}

/// 托管任务的执行器：拿到一个已登记的任务后跑到底。
///
/// 这里只负责把"执行"拆成可测试的形状——真正发上游请求、把输出写回
/// `sealed_output`，以及结束时的状态落库都由 `run_task` 完成。
pub struct TaskRunner {
    pub state: Arc<crate::app::AppState>,
}

impl TaskRunner {
    pub fn new(state: Arc<crate::app::AppState>) -> Self {
        Self { state }
    }

    /// 把任务推进到终态并落库；被取消时 `JoinHandle::abort` 会打断这个 future。
    ///
    /// `fetch_output` 由调用方注入：它负责真正调用上游并返回（上游响应 ID,
    /// 输出 JSON）。这样测试可以注入一个可控的实现，而不用真的打网络。
    pub async fn run<F, Fut>(
        &self,
        task_id: &str,
        group_id: &str,
        fetch_output: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<(Option<String>, serde_json::Value)>>,
    {
        // 进入 running 并写下第一次心跳。
        self.heartbeat(task_id, status::RUNNING, None).await?;

        match fetch_output().await {
            Ok((upstream_id, output)) => {
                let sealed = self.state.cipher.seal(output.to_string().as_bytes()).ok();
                self.finish_task(
                    task_id,
                    group_id,
                    status::COMPLETED,
                    upstream_id,
                    sealed,
                    None,
                )
                .await?;
                Ok(())
            }
            Err(error) => {
                let code = format!("{error:#}");
                self.finish_task(
                    task_id,
                    group_id,
                    status::FAILED,
                    None,
                    None,
                    Some(truncate(&code, 200)),
                )
                .await?;
                Err(error)
            }
        }
    }

    /// 更新任务状态与心跳。
    pub async fn heartbeat(
        &self,
        id: &str,
        status: &str,
        error_code: Option<String>,
    ) -> anyhow::Result<()> {
        let Some(mut task) = self.state.store.background_task_all_groups(id).await? else {
            anyhow::bail!("托管任务不存在：{id}");
        };
        task.status = status.to_string();
        task.heartbeat_at = crate::storage::now_unix();
        task.error_code = error_code;
        self.state.store.upsert_background_task(&task).await?;
        Ok(())
    }

    async fn finish_task(
        &self,
        id: &str,
        _group_id: &str,
        status: &str,
        upstream_id: Option<String>,
        sealed_output: Option<Vec<u8>>,
        error_code: Option<String>,
    ) -> anyhow::Result<()> {
        let Some(mut task) = self.state.store.background_task_all_groups(id).await? else {
            return Ok(());
        };
        let now = crate::storage::now_unix();
        task.status = status.to_string();
        task.heartbeat_at = now;
        task.finished_at = Some(now);
        task.error_code = error_code;
        if upstream_id.is_some() {
            task.upstream_id = upstream_id;
        }
        if sealed_output.is_some() {
            task.sealed_output = sealed_output;
        }
        self.state.store.upsert_background_task(&task).await?;
        Ok(())
    }
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}
