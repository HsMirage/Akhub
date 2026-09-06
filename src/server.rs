//! HTTP 服务装配、健康接口与优雅关闭（§24.3、§25.3）。

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use tower_http::trace::TraceLayer;

use crate::app::SharedState;

/// 装配全部路由。
pub fn router(state: SharedState) -> Router {
    Router::new()
        .merge(crate::gateway::router())
        .merge(crate::admin::router())
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/", get(|| async { Redirect::temporary("/admin") }))
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// 进程存活。不触碰数据库，永远立即返回。
async fn live() -> StatusCode {
    StatusCode::OK
}

/// 是否可以接收请求：数据库、配置与主密钥都已就绪。
///
/// 不暴露账号、模型、倍率或版本细节（§24.3）。
async fn ready(axum::extract::State(state): axum::extract::State<SharedState>) -> Response {
    match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(state.store.pool())
        .await
    {
        Ok(_) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::warn!(%error, "就绪检查失败");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
        }
    }
}

async fn not_found() -> Response {
    crate::gateway::error::GatewayError::new(
        crate::gateway::error::ErrorCode::ModelNotFound,
        "接口不存在",
    )
    .into_response()
}

/// 启动 HTTP 服务并阻塞到收到停止信号。
pub async fn serve(state: SharedState, addr: SocketAddr) -> Result<()> {
    let grace = state.settings.shutdown_grace;
    serve_with_shutdown(state, addr, shutdown_signal(grace)).await
}

/// 用外部注入的停止信号启动服务；`serve` 的可测试形态。
///
/// 信号触发后停止接收新请求，等待在途请求完成后刷最后一笔快照。
pub async fn serve_with_shutdown(
    state: SharedState,
    addr: SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("监听 {addr} 失败"))?;

    crate::app::tasks::spawn(&state);
    tracing::info!(%addr, "Akhub 已启动，管理后台位于 /admin");
    let result = axum::serve(listener, router(state.clone()).into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP 服务异常退出");

    // 关闭前把最后一分钟的粘性与性能数据刷完，否则重启会白丢一分钟（§22）。
    crate::app::tasks::flush_snapshots(&state).await;
    result
}

/// 等待 Ctrl-C 或 SIGTERM。
///
/// 收到信号后停止接收新请求，给在途请求 `grace` 时间完成——agent 客户端的
/// 长流式响应加上思考时间常常超过一分钟，过短的宽限会直接截断它们（§25.3）。
async fn shutdown_signal(grace: Duration) {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("无法监听 Ctrl-C 信号");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("无法监听 SIGTERM 信号")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!(
        grace_secs = grace.as_secs(),
        "收到停止信号，正在等待在途请求完成"
    );
}
