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

    /// 真正执行一次托管请求（计划 §29.1）。
    ///
    /// 复用已有的转发路径：去掉 `background`、强制非流式，然后走
    /// `passthrough::forward`。这样调度、限额、熔断、倍率与跨协议转换全都沿用
    /// 同一套逻辑，托管任务不需要第二份实现。
    ///
    /// 上游返回后把响应整体存进 `sealed_output`；被取消时这个 future 会被
    /// `abort()` 打断，正在进行的上游请求随之断开（"真取消"）。
    pub async fn execute(&self, request: ManagedRequest) {
        let task_id = request.task_id.clone();
        let group_id = request.group.group.id.clone();
        let state = Arc::clone(&request.state);

        // 立刻进 running 并写下心跳：否则查询会一直显示 queued，看不出任务到底
        // 有没有开始跑——这正是"重启不骗人"要避免的那类假状态。
        if let Err(error) = self.heartbeat(&task_id, status::RUNNING, None).await {
            tracing::warn!(%error, task = %task_id, "托管任务进入 running 失败");
        }

        let mut body = request.body.clone();
        if let Some(object) = body.as_object_mut() {
            // 内部调用必须是一次普通的非流式请求：托管任务自己管生命周期。
            object.remove("background");
            object.remove("stream");
            object.remove("stream_options");
        }

        // 托管任务必须有自己的超时：上游挂起时不能让任务永远停在 running，
        // 客户端至少能通过查询看到明确的失败（计划 §29.1）。
        let task_timeout = state.settings.get().request_timeout;
        let outcome = tokio::time::timeout(task_timeout, async {
            // 用不走托管分流的入口：类型上切断 `forward → 托管 → spawn` 的
            // Send 推断环，同时避免任务内部再进一次分流。
            let response = crate::gateway::passthrough::forward_no_managed(
                crate::gateway::passthrough::Forward {
                    state: &state,
                    group: &request.group,
                    endpoint: crate::upstream::Endpoint::Responses,
                    request_id: &request.request_id,
                    downstream_headers: &request.downstream_headers,
                    body,
                    raw: request.raw.clone(),
                    logical_model: request.logical_model.clone(),
                    request_bytes: request.request_bytes,
                    started_at: request.started_at,
                    started_unix: request.started_unix,
                    chain: request.chain.clone(),
                },
            )
            .await;
            let status = response.status();
            let payload = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
                .await
                .map_err(|error| anyhow::anyhow!("读取托管响应失败：{error}"))?;
            let value: serde_json::Value = serde_json::from_slice(&payload)
                .map_err(|error| anyhow::anyhow!("托管响应不是合法 JSON：{error}"))?;
            if !status.is_success() {
                let message = value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("上游拒绝了这次托管请求");
                anyhow::bail!("{message}");
            }
            let upstream_id = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            Ok((upstream_id, value))
        })
        .await;

        // 超时与执行失败走同一条失败路径，错误码区分开。
        let outcome = match outcome {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "托管任务超过 {} 秒未完成（上游没有在期限内返回）",
                task_timeout.as_secs()
            )),
        };

        match outcome {
            Ok((upstream_id, output)) => {
                let sealed = state.cipher.seal(output.to_string().as_bytes()).ok();
                let _ = self
                    .commit(&task_id, status::COMPLETED, upstream_id, sealed, None)
                    .await;
            }
            Err(error) => {
                let code = format!("{error:#}");
                tracing::warn!(task = %task_id, %error, "托管后台任务失败");
                let _ = self
                    .commit(
                        &task_id,
                        status::FAILED,
                        None,
                        None,
                        Some(truncate(&code, 200)),
                    )
                    .await;
            }
        }
        let _ = group_id;
    }

    /// 写终态（按 ID 跨组读取后覆盖；只给任务执行器内部使用）。
    async fn commit(
        &self,
        id: &str,
        status_value: &str,
        upstream_id: Option<String>,
        sealed_output: Option<Vec<u8>>,
        error_code: Option<String>,
    ) -> anyhow::Result<()> {
        let Some(mut task) = self.state.store.background_task_all_groups(id).await? else {
            return Ok(());
        };
        let now = crate::storage::now_unix();
        task.status = status_value.to_string();
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

/// 查询一条托管任务，按客户端可见的形状返回（计划 §29.1）。
///
/// 找不到或不属于这个分组时返回 `None`，由调用方决定错误形状。
pub async fn lookup(
    state: &crate::app::SharedState,
    group_id: &str,
    id: &str,
) -> Option<serde_json::Value> {
    let task = state.store.background_task(id, group_id).await.ok()??;
    let mut object = public_object(&task, id);
    // 完成的任务把保存的输出解回来；解密失败时不编造内容。
    if let Some(sealed) = task.sealed_output.as_ref() {
        if let Ok(plaintext) = state.cipher.open(sealed)
            && let Ok(saved) = serde_json::from_slice::<serde_json::Value>(&plaintext)
        {
            if let Some(saved_object) = saved.as_object()
                && let Some(object_map) = object.as_object_mut()
            {
                for (key, value) in saved_object {
                    // ID 与状态永远以网关的为准，其余字段用上游的真实响应。
                    if key == "id" || key == "status" {
                        continue;
                    }
                    object_map.insert(key.clone(), value.clone());
                }
                object_map.insert("id".into(), serde_json::json!(id));
                object_map.insert(
                    "status".into(),
                    serde_json::json!(public_status(&task.status)),
                );
            }
            return Some(object);
        }
        tracing::warn!(task = %id, "托管任务输出解密失败，返回状态而非内容");
    }
    Some(object)
}

/// 取消一条托管任务（计划 §29.1）。
///
/// **真取消**：先中断在跑的任务（从而断开上游连接），再把状态写进库。返回
/// `(object, aborted)`，`aborted` 表示是否确实打断了一个正在执行的连接；
/// 任务本来就已结束时为 `false`，调用方可以在响应里说明这一点。
pub async fn cancel(
    state: &crate::app::SharedState,
    group_id: &str,
    id: &str,
) -> Option<(serde_json::Value, bool)> {
    let task = state.store.background_task(id, group_id).await.ok()??;
    // 已经到终态的任务不再取消，避免把 completed 改写成 cancelled。
    if task.finished_at.is_some() {
        return Some((public_object(&task, id), false));
    }
    let aborted = state.runtime.background.cancel(id).await;
    let mut updated = task.clone();
    updated.status = status::CANCELLED.to_string();
    updated.finished_at = Some(crate::storage::now_unix());
    updated.heartbeat_at = updated.finished_at.unwrap_or_else(crate::storage::now_unix);
    if let Err(error) = state.store.upsert_background_task(&updated).await {
        tracing::warn!(%error, task = %id, "写入取消状态失败");
    }
    Some((public_object(&updated, id), aborted))
}

/// 删除一条托管任务（客户端 DELETE /v1/responses/{id}）。
pub async fn destroy(state: &crate::app::SharedState, group_id: &str, id: &str) -> bool {
    // 先取消在跑的任务，避免删掉记录后上游还在跑。
    state.runtime.background.cancel(id).await;
    state
        .store
        .delete_background_task(id, group_id)
        .await
        .unwrap_or(false)
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

// ------------------------------------------------------------------ 接入转发路径

/// 一次托管请求在后台执行所需的全部**拥有所有权**的输入。
///
/// `passthrough::Forward` 借用调用方的数据，没法直接丢进 `tokio::spawn`；
/// 这里把要用的字段复制一份，任务内部再借用它构造 `Forward`。
pub struct ManagedRequest {
    pub task_id: String,
    pub state: crate::app::SharedState,
    pub group: Arc<crate::config::GroupView>,
    pub request_id: String,
    pub downstream_headers: axum::http::HeaderMap,
    pub body: serde_json::Value,
    pub raw: Option<crate::gateway::passthrough::RawBody>,
    pub logical_model: String,
    pub request_bytes: usize,
    pub started_at: std::time::Instant,
    pub started_unix: i64,
    pub chain: crate::gateway::responses::ChainPlan,
}

/// 判断这次请求要不要走网关托管后台；要的话登记任务并立刻返回任务对象。
///
/// 三个条件同时满足才托管（计划 §29.1）：
/// 1. 入口是 Responses 且请求带 `background: true`；
/// 2. 分组显式打开了 `allow_managed_background`（默认关闭）；
/// 3. 上游**不支持**原生后台——支持的话第一期就走原生代理，不该抢过来。
///
/// 返回 `None` 表示照常走同步转发。
pub async fn maybe_start_managed(
    forward: &crate::gateway::passthrough::Forward<'_>,
) -> Option<axum::response::Response> {
    if forward.endpoint != crate::upstream::Endpoint::Responses {
        return None;
    }
    if !forward
        .body
        .get("background")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    if !forward.group.group.allow_managed_background {
        return None;
    }
    // 原生支持后台就不托管：那条路保真度更高，也让上游自己管生命周期。
    if native_background_available(forward) {
        return None;
    }

    let task_id = new_id();
    let now = crate::storage::now_unix();
    let task = BackgroundTaskRow {
        id: task_id.clone(),
        group_id: forward.group.group.id.clone(),
        logical_model: forward.logical_model.clone(),
        account_id: None,
        target_id: None,
        status: status::QUEUED.to_string(),
        upstream_id: None,
        created_at: now,
        heartbeat_at: now,
        finished_at: None,
        error_code: None,
        sealed_output: None,
        expires_at: now + i64::from(forward.state.settings.get().response_state_days) * 86_400,
    };
    if let Err(error) = forward.state.store.upsert_background_task(&task).await {
        tracing::warn!(%error, "登记托管后台任务失败，退回同步转发");
        return None;
    }

    let request = ManagedRequest {
        task_id: task_id.clone(),
        state: Arc::clone(forward.state),
        group: Arc::clone(forward.group),
        request_id: forward.request_id.to_string(),
        downstream_headers: forward.downstream_headers.clone(),
        body: forward.body.clone(),
        raw: forward.raw.clone(),
        logical_model: forward.logical_model.clone(),
        request_bytes: forward.request_bytes,
        started_at: forward.started_at,
        started_unix: forward.started_unix,
        chain: forward.chain.clone(),
    };

    let runner_state = Arc::clone(forward.state);
    let runner = TaskRunner::new(Arc::clone(&runner_state));
    let spawned_task_id = task_id.clone();
    let job_state = Arc::clone(&runner_state);
    let handle = tokio::spawn(async move {
        runner.execute(request).await;
        job_state.runtime.background.finish(&spawned_task_id).await;
    });
    runner_state
        .runtime
        .background
        .register(&task_id, handle)
        .await;

    let object = public_object(&task, &task_id);
    Some(axum::response::IntoResponse::into_response(axum::Json(
        object,
    )))
}

/// 这次请求的目标里有没有上游原生支持 Responses 后台。
///
/// 判据是端点：账号首选 Responses 协议、且该端点没有被证实缺失时，就认为上游
/// 有机会原生支持——此时不该由网关托管（计划 §29.1 的"优先原生"）。
fn native_background_available(forward: &crate::gateway::passthrough::Forward<'_>) -> bool {
    if forward.chain.pinned.is_some() {
        return true;
    }
    let now = std::time::Instant::now();
    forward
        .group
        .models
        .values()
        .flat_map(|model| model.targets.iter())
        .any(|target| {
            target.account.preferred_protocol == crate::domain::Protocol::OpenAiResponses
                && !forward.state.runtime.evidence.is_unsupported(
                    &target.account.id,
                    crate::upstream::Endpoint::Responses,
                    now,
                )
        })
}
