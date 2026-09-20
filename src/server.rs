//! HTTP 服务装配、健康接口与优雅关闭（§24.3、§25.3）。

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::trace::TraceLayer;

use crate::app::SharedState;

/// 装配全部路由。
pub fn router(state: SharedState) -> Router {
    assemble(
        Router::new()
            .merge(crate::gateway::router())
            .merge(crate::admin::router())
            .route("/health/live", get(live))
            .route("/health/ready", get(ready))
            .route("/", get(|| async { Redirect::temporary("/admin") }))
            // 版本端点：容器编排、CI 冒烟与前向代理的健康探针都想在不带
            // 任何凭据的前提下确认"跑的是哪个版本、是否就绪"（§25.1）。
            // 只暴露版本号本身，不含账号、模型或配置细节。
            .route("/health/version", get(version))
            .fallback(not_found),
        state,
    )
}

/// 给路由套上全部公共中间件。
///
/// 单独抽出来是为了让测试能用**同一套**中间件包一个会 panic 的处理器，
/// 验证隔离层真的生效——测 tower-http 自己的行为没有意义，要测的是我们的接线。
fn assemble(router: Router<SharedState>, state: SharedState) -> Router {
    router
        .layer(TraceLayer::new_for_http())
        // 单个请求任务的 panic 必须被隔离（§19.4）。tokio 本来就只终止出错的
        // 那个任务、不会杀进程，但客户端会看到连接被重置、拿不到任何解释。
        // 这一层把它变成一个带稳定错误码的 500，同时记一条日志。
        .layer(CatchPanicLayer::custom(|_| {
            tracing::error!("请求处理任务 panic，已隔离为 500");
            crate::gateway::error::GatewayError::new(
                crate::gateway::error::ErrorCode::InternalError,
                "内部错误，详见服务端日志",
            )
            .into_response()
        }))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state.clone())
        // 每个响应都回带当前配置版本，供后台的乐观锁更新基准（§7.4）。
        .layer(axum::middleware::from_fn_with_state(
            state,
            crate::admin::attach_config_version,
        ))
}

/// 版本端点：不需要凭据，只回一个能让运维确认部署是否生效的版本号。
async fn version() -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        format!(
            "{{\"object\":\"health\",\"status\":\"ok\",\"version\":\"{}\"}}",
            crate::admin::version()
        ),
    )
        .into_response()
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
///
/// 两条路径都会触发关闭：操作系统的 SIGTERM / Ctrl-C，以及后台"重启服务"
/// 按钮置位的 [`crate::app::Runtime::begin_shutdown`]。后者存在的意义是让
/// 自更新之后的进程能自己退出去，交给 systemd 拉起新二进制。
pub async fn serve(state: SharedState, addr: SocketAddr) -> Result<()> {
    let grace = state.settings.get().shutdown_grace;
    let mut requested = state.runtime.subscribe_shutdown();
    let requested = async move {
        loop {
            if *requested.borrow() {
                break;
            }
            if requested.changed().await.is_err() {
                // 发送端没了（正常情况下不会发生）：永远等下去，不影响信号路径。
                std::future::pending::<()>().await;
            }
        }
    };
    serve_with_shutdown(state, addr, async move {
        tokio::select! {
            _ = shutdown_signal(grace) => {}
            _ = requested => tracing::info!("收到后台重启请求，开始优雅关闭"),
        }
    })
    .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    /// 单个请求任务 panic 时，客户端拿到的是带稳定错误码的 500，而不是连接被重置（§19.4）。
    #[tokio::test]
    async fn a_panicking_handler_becomes_a_500_instead_of_a_dropped_connection() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::app::AppState::bootstrap(dir.path(), crate::app::Settings::default())
            .await
            .unwrap();

        // 一个必然 panic 的处理器。显式返回类型是必需的：panic! 的 ! 类型
        // 无法让编译器推断出处理器该返回什么。
        async fn boom() -> Response {
            panic!("故意炸一个请求任务");
        }

        let app = assemble(Router::new().route("/boom", get(boom)), state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let response = reqwest::Client::new()
            .get(format!("http://{addr}/boom"))
            .send()
            .await
            .expect("请求本身要能拿到响应，而不是连接被重置");
        assert_eq!(response.status(), 500);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            body["error"]["code"], "internal_error",
            "隔离后的 500 要用网关的稳定错误码：{body}"
        );
        assert!(
            !body.to_string().contains("故意炸"),
            "不能把 panic 文案回给客户端：{body}"
        );

        // 隔离的意义在于"只死这一个请求"：下一个请求照常。
        let again = reqwest::Client::new()
            .get(format!("http://{addr}/boom"))
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), 500, "服务仍然在跑，能继续回 500");
    }
}
