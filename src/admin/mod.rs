//! 管理后台 REST API 与会话（§7.4）。

pub mod resources;
pub mod system;
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

/// 二进制版本号，取自 Cargo.toml。
///
/// `/admin/api/overview` 与 `/health/version` 必须给出同一个字符串：运维用前者
/// 在界面上确认版本、用后者在编排里做探针，两者一旦分叉，升级核对就没有意义。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

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

/// 配置版本头（§7.4），**收发共用一个名字**。
///
/// - 请求方向：写操作带上它读到的版本，对不上就 409。
/// - 响应方向：每个响应回带当前版本，客户端不必再拉一次概览。
///
/// 两个方向刻意用同一个头名（类似 ETag 与 If-Match 的关系），所以这里只有
/// **一个**常量。曾经写成两个同名常量，那是纯粹的陷阱：改掉其中一个不会有
/// 任何编译错误，只会让乐观锁静默失效。
pub const CONFIG_VERSION_HEADER: &str = "x-akhub-config-version";

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

        // 乐观锁（§7.4）：写操作带上它读到的配置版本，与当前版本不一致说明
        // 别人已经改过配置，返回 409 让用户重新加载，而不是静默覆盖。
        // 不带头部表示"我知道自己在做什么"（例如脚本初始化），保持兼容。
        if parts.method != axum::http::Method::GET
            && let Some(raw) = parts.headers.get(CONFIG_VERSION_HEADER)
        {
            let expected = raw
                .to_str()
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .ok_or_else(|| {
                    AdminError::bad_request(format!(
                        "{CONFIG_VERSION_HEADER} 必须是配置版本号（整数）"
                    ))
                })?;
            let current = state.config.current().version;
            if expected != current {
                return Err(AdminError::new(
                    StatusCode::CONFLICT,
                    format!(
                        "config_conflict: 配置已被其他修改更新（你的版本 {expected}，当前 {current}），                         请重新加载后再保存，避免覆盖别人的修改。"
                    ),
                ));
            }
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

/// 构造登录成功时下发的 Cookie（§23.2）。
///
/// `secure` 由请求判定：Akhub 自己不终止 TLS，所以看反向代理给的
/// `X-Forwarded-Proto`。硬加 `Secure` 会让 `127.0.0.1` 直连无法登录，
/// 完全不加又等于在 HTTPS 部署下放弃一层保护，因此按实际情况决定。
pub(crate) fn session_cookie_header(token: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200{secure}")
}

fn cleared_cookie_header() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

/// 这个请求是不是走 HTTPS 进来的（§23.2）。
///
/// 只认反向代理的 `X-Forwarded-Proto`：Akhub 监听的是明文 HTTP，直连时
/// 该头不存在，就不加 `Secure`，本地开发因此仍然能登录。
fn request_is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        // 代理链可能给出 "https, http" 这种列表，取最左边（最初的那一跳）。
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
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
            "/admin/api/groups/{id}/available-models",
            get(r::group_available_models),
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
            "/admin/api/accounts/refresh-multipliers",
            post(r::refresh_all_multipliers),
        )
        .route(
            "/admin/api/accounts/{id}/refresh-multiplier",
            post(r::refresh_account_multiplier),
        )
        .route(
            "/admin/api/accounts/{id}/detect-multiplier-source",
            post(r::detect_account_multiplier_source),
        )
        .route(
            "/admin/api/accounts/{id}/multiplier-groups",
            get(r::account_multiplier_groups),
        )
        .route("/admin/api/accounts/{id}/copy", post(r::copy_account))
        .route("/admin/api/accounts/{id}/test", post(r::test_account))
        // 手动放行一把被误判的 Key（§12.3）：面板上"Key 失效"必须配有出口。
        .route(
            "/admin/api/accounts/{id}/keys/{key_id}/clear-faults",
            post(r::clear_key_faults),
        )
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
        // 写路径的唯一入口：勾选、改名、批量操作都走它，一次请求只调和一遍
        // 目标、只重载一遍配置（逐行接口在几百个模型上会卡住界面）。
        .route(
            "/admin/api/accounts/{id}/models/apply",
            post(r::apply_account_models),
        )
        .route(
            "/admin/api/accounts/{id}/models/update",
            post(r::update_account_model),
        )
        .route(
            "/admin/api/accounts/{id}/models/delete",
            post(r::delete_account_model),
        )
        .route(
            "/admin/api/accounts/{id}/models/merge",
            post(r::merge_account_models),
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
        .route("/admin/api/metrics", get(r::metrics))
        // 版本检查与自更新（顶部版本号点开的面板）。
        .route(
            "/admin/api/system/update",
            get(system::update_status).post(system::run_update),
        )
        .route("/admin/api/system/restart", post(system::restart))
        .route(
            "/admin/api/settings",
            get(r::get_settings).patch(r::update_settings),
        )
        .route(
            "/admin/api/new-api-sites",
            get(r::list_new_api_sites)
                .post(r::save_new_api_site)
                .delete(r::delete_new_api_site),
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

/// 把当前配置版本写到响应头（§7.4）。
///
/// 在 server.rs 装配路由时挂载——那里才有具体的状态可用。写在处理器之后：
/// 写操作会先 bump 版本，客户端拿到的必须是**写完之后**的新值，否则下一次
/// 写又会 409。
pub async fn attach_config_version(
    State(state): State<SharedState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    let version = state.config.current().version;
    if let Ok(value) = axum::http::HeaderValue::from_str(&version.to_string()) {
        response.headers_mut().insert(CONFIG_VERSION_HEADER, value);
    }
    response
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
    headers: HeaderMap,
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

    issue_session(
        &state,
        credentials.username.trim(),
        request_is_https(&headers),
    )
}

/// 管理员登录。
async fn login(
    State(state): State<SharedState>,
    headers: HeaderMap,
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

    issue_session(&state, username, request_is_https(&headers))
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

fn issue_session(state: &SharedState, username: &str, secure: bool) -> AdminResult<Response> {
    let token = state
        .sessions
        .create(username)
        .map_err(AdminError::internal)?;
    Ok((
        [(header::SET_COOKIE, session_cookie_header(&token, secure))],
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
        let cookie = session_cookie_header("tok123", false);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("tok123"));
        // 明文直连（本地 127.0.0.1）不加 Secure，否则本地根本登不上去。
        assert!(!cookie.contains("Secure"));
    }

    /// HTTPS 部署下必须加 `Secure`（§23.2）。
    #[test]
    fn https_requests_get_a_secure_cookie() {
        let cookie = session_cookie_header("tok123", true);
        assert!(cookie.contains("; Secure"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
    }

    /// `X-Forwarded-Proto` 的判定：只认 https，且能吃下代理链的列表。
    #[test]
    fn forwarded_proto_decides_whether_the_cookie_is_secure() {
        let proto = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-proto", HeaderValue::from_str(value).unwrap());
            request_is_https(&headers)
        };
        assert!(!request_is_https(&HeaderMap::new()), "没有该头说明是直连");
        assert!(proto("https"));
        assert!(proto("HTTPS"), "大小写不敏感");
        assert!(proto("https, http"), "取最初那一跳");
        assert!(!proto("http"), "明文反代不能加 Secure");
        assert!(!proto("http, https"));
    }

    #[test]
    fn logout_clears_the_cookie_immediately() {
        assert!(cleared_cookie_header().contains("Max-Age=0"));
    }
}
