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
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

/// 给所有响应补上最小安全响应头（§23.2）。
///
/// 不设 CSP：管理后台是内嵌单页应用，贸然上严格 CSP 会把它打坏；这里只做
/// 无副作用的加固。
async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers
        .entry(axum::http::header::X_CONTENT_TYPE_OPTIONS)
        .or_insert(axum::http::HeaderValue::from_static("nosniff"));
    headers
        .entry(axum::http::header::X_FRAME_OPTIONS)
        .or_insert(axum::http::HeaderValue::from_static("DENY"));
    headers
        .entry(axum::http::header::REFERRER_POLICY)
        .or_insert(axum::http::HeaderValue::from_static("no-referrer"));
    response
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
    let grace = state.settings.get().shutdown_grace;
    serve_with_shutdown(state, addr, shutdown_signal(grace)).await
}

/// 用外部注入的停止信号启动服务；`serve` 的可测试形态。
///
/// 信号触发后停止接收新请求，等待在途请求完成；超过
/// `AKHUB_SHUTDOWN_GRACE_SECS`（默认 180 秒）仍未收尾就强制结束，避免长流式
/// 请求把关闭流程拖到监督进程来杀（§25.3 第 3、5 步）。
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
    let grace = state.settings.get().shutdown_grace;

    // 宽限期必须从**收到停止信号之后**开始算。早期实现直接用
    // `timeout(grace, serve)` 包住整个服务，结果进程每 180 秒就自行退出一次
    // （systemd 会一直重启）。这里用两个通道把两件事分开：先等信号，再计时。
    let (stopping_tx, stopping_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown_state = std::sync::Arc::clone(&state);
    let graceful = async move {
        shutdown.await;
        // 先置位关闭标志：排队中的请求立刻拿到可重试错误，而不是干等到
        // 宽限期结束被强制切断（§25.3 第 2 步）。
        shutdown_state.runtime.begin_shutdown();
        let _ = stopping_tx.send(());
    };
    let serving = axum::serve(listener, router(state.clone()).into_make_service())
        .with_graceful_shutdown(graceful);
    let hard_stop = async move {
        if stopping_rx.await.is_err() {
            // 信号发送端消失表示服务已经结束，这里永远等下去。
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(grace).await;
    };

    let result = tokio::select! {
        result = serving => result.context("HTTP 服务异常退出"),
        _ = hard_stop => {
            // 宽限期到点：`main` 随本函数返回，运行时析构会中止剩余任务。
            // systemd 的 `TimeoutStopSec` 与容器的 `stop_grace_period` 是更外层
            // 的兜底，这里自己先收口，不让 180 秒的承诺落空。
            tracing::warn!(
                grace_secs = grace.as_secs(),
                "优雅关闭超过宽限期，强制结束剩余在途请求"
            );
            Ok(())
        }
    };

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
