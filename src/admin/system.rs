//! 后台的版本检查、自更新与重启：顶部版本号点开的那块面板。
//!
//! 三个接口都要求管理员会话；写操作额外要求 CSRF 头与配置版本，规则由
//! [`super::Admin`] 提取器统一负责，这里只处理业务。

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Admin, AdminError, AdminResult};
use crate::app::SharedState;

/// `?refresh=1` 表示用户手动点了"重新检查"。
///
/// 用字符串而不是 `bool` 解析：`1` / `true` / `yes` 都认，
/// 免得前端换个写法就静默变成走缓存。
#[derive(Debug, Deserialize)]
pub struct RefreshQuery {
    #[serde(default)]
    pub refresh: Option<String>,
}

impl RefreshQuery {
    fn force(&self) -> bool {
        matches!(
            self.refresh.as_deref().map(str::trim),
            Some("1") | Some("true") | Some("yes")
        )
    }
}

/// `GET /admin/api/system/update`：当前版本、最新版本与升级方式。
pub async fn update_status(
    State(state): State<SharedState>,
    Query(query): Query<RefreshQuery>,
    _: Admin,
) -> AdminResult<Json<crate::update::Status>> {
    Ok(Json(state.runtime.update.check(query.force()).await))
}

/// `POST /admin/api/system/update`：下载 → 校验 → 原子替换二进制。
///
/// 只对原生二进制部署有效；容器、源码构建与 Windows 会带着明确的替代命令报错。
pub async fn run_update(State(state): State<SharedState>, _: Admin) -> AdminResult<Json<Value>> {
    let target = crate::update::exe_path();
    // 放到独立任务里跑：管理员关掉页面、反向代理掐断连接都会让请求 future 被丢弃，
    // 不能因此中断已经下了一半的更新。任务本身不随请求取消，结果即使没人接，
    // 也会以 pending_version 的形式留在状态里，下一次打开面板就能看到。
    let updater = Arc::clone(&state);
    let handle = tokio::spawn(async move { updater.runtime.update.install(&target, None).await });
    match handle.await {
        Ok(Ok(outcome)) => Ok(Json(json!({ "outcome": outcome }))),
        Ok(Err(message)) => Err(AdminError::bad_request(message)),
        Err(error) => Err(AdminError::internal(format!("更新任务异常结束：{error}"))),
    }
}

/// `POST /admin/api/system/restart`：优雅关闭自己，交给监督进程拉起新版本。
///
/// 没有监督进程时会真的把服务停掉，所以这里必须先确认 systemd（或运维显式
/// 用 `AKHUB_ALLOW_RESTART=1` 声明）在兜底，否则只回一句怎么手工重启。
pub async fn restart(State(state): State<SharedState>, _: Admin) -> AdminResult<Json<Value>> {
    if !crate::update::can_restart() {
        return Err(AdminError::bad_request(format!(
            "没有检测到会自动拉起本进程的监督进程，拒绝直接退出。请手工重启：{}。\
             容器部署请用 docker compose up -d 重建容器；systemd 部署请确认单元里有 Restart=always。",
            crate::update::restart_command()
        )));
    }

    // 先把响应发出去再关：反过来的话客户端只会看到连接被重置。
    let state = Arc::clone(&state);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        tracing::info!("后台请求重启服务，进入优雅关闭");
        state.runtime.begin_shutdown();
    });
    Ok(Json(json!({ "restarting": true })))
}
