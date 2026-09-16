//! 管理后台 REST API 与会话（§7.4）。

pub mod resources;
pub mod ui;

use axum::Router;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::SharedState;
use crate::auth::session;

/// 会话 Cookie 名。
const SESSION_COOKIE: &str = "akhub_session";
/// 写操作必须携带的自定义头。
///
/// 浏览器的跨站表单提交无法附加自定义头，跨站 `fetch` 又会被 CORS 预检拦下，
/// 所以这个头配合 `SameSite=Strict` 就构成了完整的 CSRF 防护（§23.2）。
const CSRF_HEADER: &str = "x-akhub-csrf";

/// 后台接口的统一错误。
#[derive(Debug)]
pub struct AdminError {
    pub status: StatusCode,
    pub message: String,
}

impl AdminError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    /// 把内部错误转成 500，同时把细节留在服务端日志里。
    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "后台接口内部错误");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "内部错误，详见服务端日志",
        )
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (self.status, axum::Json(json!({ "error": self.message }))).into_response()
    }
}

pub type AdminResult<T> = Result<T, AdminError>;

/// 已登录的管理员。作为提取器出现在任何需要鉴权的处理函数签名中。
#[derive(Debug, Clone)]
pub struct Admin {
    pub username: String,
}

impl FromRequestParts<SharedState> for Admin {
    type Rejection = AdminError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let token =
            session_cookie(&parts.headers).ok_or_else(|| AdminError::unauthorized("请先登录"))?;
        let username = state
            .sessions
            .resolve(&token)
            .ok_or_else(|| AdminError::unauthorized("会话已过期，请重新登录"))?;

        // 只对写操作要求 CSRF 头；GET 是安全方法，不改变服务端状态。
        if parts.method != axum::http::Method::GET && !parts.headers.contains_key(CSRF_HEADER) {
            return Err(AdminError::new(
                StatusCode::FORBIDDEN,
                format!("写操作必须携带 {CSRF_HEADER} 请求头"),
            ));
        }
        Ok(Admin { username })
    }
}

/// 从 Cookie 头中取出会话令牌。
fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_string())
}

/// 构造登录成功时下发的 Cookie。
///
/// 不设 `Secure`：Akhub 第一期建议由反向代理终止 HTTPS，也支持 `127.0.0.1`
/// 直连，硬加 `Secure` 会让本地部署无法登录。生产环境请置于 HTTPS 之后。
pub(crate) fn session_cookie_header(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200")
}

fn cleared_cookie_header() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

#[derive(Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
struct SetupStatus {
    needs_setup: bool,
    master_key_from_env: bool,
}

/// 组装 `/admin` 下的全部接口与静态页面。
pub fn router() -> Router<SharedState> {
    use resources as r;

    Router::new()
        .route("/admin/api/setup/status", get(setup_status))
        .route("/admin/api/setup", post(setup))
        .route("/admin/api/auth/password", post(r::change_password))
        .route("/admin/api/auth/login", post(login))
        .route("/admin/api/auth/logout", post(logout))
        .route("/admin/api/overview", get(r::overview))
        .route(
            "/admin/api/groups",
            get(r::list_groups).post(r::create_group),
        )
        .route(
            "/admin/api/groups/{id}",
            get(r::get_group)
                .patch(r::update_group)
                .delete(r::delete_group),
        )
        .route(
            "/admin/api/groups/{id}/regenerate-key",
            post(r::regenerate_key),
        )
        .route(
            "/admin/api/accounts",
            get(r::list_accounts).post(r::create_account),
        )
        .route(
            "/admin/api/accounts/{id}",
            patch_or_delete(r::update_account, r::delete_account),
        )
        .route(
            "/admin/api/accounts/{id}/refresh-multiplier",
            post(r::refresh_account_multiplier),
        )
        .route("/admin/api/accounts/{id}/copy", post(r::copy_account))
        .route("/admin/api/accounts/{id}/test", post(r::test_account))
        .route(
            "/admin/api/accounts/{id}/calibrate",
            post(r::calibrate_account),
        )
        .route(
            "/admin/api/accounts/{id}/calibrations",
            get(r::list_calibrations),
        )
        .route("/admin/api/cost", get(r::cost))
        .route("/admin/api/backup/export", post(r::export_backup))
        .route("/admin/api/backup/import", post(r::import_backup))
        .route(
            "/admin/api/accounts/{id}/models",
            get(r::list_account_models).post(r::add_manual_model),
        )
        .route(
            "/admin/api/accounts/{id}/models/refresh",
            post(r::refresh_account_models),
        )
        .route(
            "/admin/api/accounts/{id}/models/select",
            post(r::select_account_models),
        )
        .route(
            "/admin/api/accounts/{id}/models/sync",
            post(r::sync_account_models),
        )
        .route(
            "/admin/api/accounts/{id}/aliases",
            get(r::list_aliases).put(r::update_aliases),
        )
        .route(
            "/admin/api/logical-models",
            get(r::list_logical_models).post(r::create_logical_model),
        )
        .route(
            "/admin/api/logical-models/{id}",
            patch_or_delete(r::update_logical_model, r::delete_logical_model),
        )
        .route(
            "/admin/api/targets",
            get(r::list_targets).post(r::create_target),
        )
        .route(
            "/admin/api/targets/{id}",
            patch_or_delete(r::update_target, r::delete_target),
        )
        .route("/admin/api/requests", get(r::list_requests))
        .route(
            "/admin/api/settings",
            get(r::get_settings).patch(r::update_settings),
        )
        // 静态资源必须排在 API 之后：matchit 优先匹配静态段，所以
        // `/admin/api/...` 不会被这里的通配捕获。
        .route("/admin", get(|| ui::serve("/")))
        .route("/admin/", get(|| ui::serve("/")))
        .route(
            "/admin/{*path}",
            get(|Path(path): Path<String>| async move { ui::serve(&path).await }),
        )
}

/// 把 PATCH 与 DELETE 装配到同一路径上。
fn patch_or_delete<P, D, TP, TD>(patch: P, delete: D) -> axum::routing::MethodRouter<SharedState>
where
    P: axum::handler::Handler<TP, SharedState>,
    D: axum::handler::Handler<TD, SharedState>,
    TP: 'static,
    TD: 'static,
{
    axum::routing::patch(patch).delete(delete)
}

/// 判断是否需要首次设置。无需登录。
async fn setup_status(State(state): State<SharedState>) -> AdminResult<axum::Json<SetupStatus>> {
    let needs_setup = state
        .store
        .needs_setup()
        .await
        .map_err(AdminError::internal)?;
    Ok(axum::Json(SetupStatus {
        needs_setup,
        master_key_from_env: state.master_key_from_env,
    }))
}

/// 创建首个管理员。只能成功一次。
async fn setup(
    State(state): State<SharedState>,
    axum::Json(credentials): axum::Json<Credentials>,
) -> AdminResult<Response> {
    if credentials.username.trim().is_empty() {
        return Err(AdminError::bad_request("用户名不能为空"));
    }
    if credentials.password.chars().count() < 8 {
        return Err(AdminError::bad_request("密码至少需要 8 个字符"));
    }
    if !state
        .store
        .needs_setup()
        .await
        .map_err(AdminError::internal)?
    {
        return Err(AdminError::conflict("管理员已存在，首次设置只能执行一次"));
    }

    let hash = session::hash_password(&credentials.password).map_err(AdminError::internal)?;
    state
        .store
        .create_admin(credentials.username.trim(), &hash)
        .await
        .map_err(|_| AdminError::conflict("管理员已存在，首次设置只能执行一次"))?;

    issue_session(&state, credentials.username.trim())
}

/// 管理员登录。
async fn login(
    State(state): State<SharedState>,
    axum::Json(credentials): axum::Json<Credentials>,
) -> AdminResult<Response> {
    let username = credentials.username.trim();
    if state.sessions.is_locked(username) {
        return Err(AdminError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "登录失败次数过多，请稍后再试",
        ));
    }

    let stored = state
        .store
        .admin_password_hash(username)
        .await
        .map_err(AdminError::internal)?;
    // 用户名不存在与密码错误返回同一句话，不泄漏哪个环节出错。
    let valid = stored
        .as_deref()
        .map(|hash| session::verify_password(&credentials.password, hash))
        .unwrap_or(false);
    if !valid {
        state.sessions.record_failure(username);
        return Err(AdminError::unauthorized("用户名或密码错误"));
    }

    issue_session(&state, username)
}

/// 退出登录，令牌立即失效。
async fn logout(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_cookie(&headers) {
        state.sessions.revoke(&token);
    }
    (
        [(header::SET_COOKIE, cleared_cookie_header())],
        axum::Json(json!({ "ok": true })),
    )
        .into_response()
}

fn issue_session(state: &SharedState, username: &str) -> AdminResult<Response> {
    let token = state
        .sessions
        .create(username)
        .map_err(AdminError::internal)?;
    Ok((
        [(header::SET_COOKIE, session_cookie_header(&token))],
        axum::Json(json!({ "username": username, "csrf_header": CSRF_HEADER })),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn session_cookie_is_extracted_among_other_cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str("theme=dark; akhub_session=tok123; other=1").unwrap(),
        );
        assert_eq!(session_cookie(&headers).as_deref(), Some("tok123"));
    }

    #[test]
    fn a_missing_session_cookie_yields_none() {
        assert!(session_cookie(&HeaderMap::new()).is_none());

        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("theme=dark"));
        assert!(session_cookie(&headers).is_none());
    }

    #[test]
    fn issued_cookies_are_http_only_and_same_site_strict() {
        let cookie = session_cookie_header("tok123");
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("tok123"));
    }

    #[test]
    fn logout_clears_the_cookie_immediately() {
        assert!(cleared_cookie_header().contains("Max-Age=0"));
    }
}
