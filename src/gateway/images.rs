//! 异步生图任务的轮询转发（§14.9）。
//!
//! 下游客户端手里的任务 ID 是**上游**签发的：下单落在哪个账号上，轮询就必须
//! 回到那个账号——别的账号根本不认识这个任务。因此这条路径不走调度链路
//! （没有模型，也没有可切换的候选），而是按定位表直连原账号。
//!
//! 定位表只存"任务 ID → 接单账号"，不存任务结果。查不到、已过期、或不属于
//! 当前分组，一律同样的 404：对外不区分"从来没存在过"和"过期了"（§26.8）。

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::app::SharedState;
use crate::auth;
use crate::config::TargetView;
use crate::domain::Protocol;
use crate::gateway::error::{ErrorCode, GatewayError};
use crate::gateway::passthrough::{self, unavailable_code};
use crate::health;
use crate::storage::store::RequestRecord;
use crate::upstream;

/// 轮询上游的等待上限。
///
/// 轮询是廉价读：客户端本来就会按 Retry-After 再来一次，卡住它不如给一个
/// 明确的失败。这里不跟随请求总超时（600 秒）——那会让一次轮询占用一个连接
/// 十分钟。
const POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// `GET /v1/images/tasks/{task_id}`：把轮询送回当初接单的账号。
pub async fn task_status(
    State(state): State<SharedState>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let started = Instant::now();
    let started_unix = crate::storage::now_unix();
    let request_id = format!("req_{}", ulid::Ulid::generate());
    let protocol = Protocol::OpenAiChat;

    let credential = match auth::extract_credential(&headers) {
        Ok(credential) => credential,
        Err(error) => {
            return error
                .with_protocol(protocol)
                .with_request_id(request_id)
                .into_response();
        }
    };
    let config = state.config.current();
    let group = match auth::authenticate(&config, &state.key_digest, &credential) {
        Ok(group) => group,
        Err(error) => {
            return error
                .with_protocol(protocol)
                .with_request_id(request_id)
                .into_response();
        }
    };

    let row = match state.store.image_task(&task_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return missing_task(&request_id),
        Err(error) => {
            tracing::warn!(%error, request_id, "读取异步生图任务定位失败");
            return GatewayError::new(ErrorCode::InternalError, "读取任务定位失败")
                .with_protocol(protocol)
                .with_request_id(request_id)
                .into_response();
        }
    };
    // 分组不匹配与"查不到"返回同一个答案：任务 ID 不该成为跨组探测工具（§26.8）。
    if row.group_id != group.group.id {
        return missing_task(&request_id);
    }
    let Some(target) = config.target_by_account(&row.account_id).cloned() else {
        // 账号被删、或已经不再被任何分组引用：这个任务无处可问。
        return missing_task(&request_id);
    };

    // 轮询必须用**当初那把 Key**：上游的任务是按凭据隔离的资源，换一把 Key 去
    // 问同一个任务 ID，上游只会当作不存在（§4.2.1 的不变量 A）。
    let credentials = state.runtime.credentials.current();
    let credential = credentials.keys_of(&row.account_id).first().cloned();
    drop(credentials);
    let Some(credential) = credential else {
        return GatewayError::new(
            ErrorCode::UpstreamExhausted,
            format!("账号「{}」没有可用凭据", target.account.name),
        )
        .with_protocol(protocol)
        .with_request_id(request_id)
        .into_response();
    };

    // 与推理请求同一套准入：RPM/TPM/并发与熔断照常生效，不新增独立的限额路径。
    let caller = health::Caller {
        account_id: &target.account.id,
        key_id: Some(credential.credential_digest.as_str()),
        target_id: &target.target.id,
    };
    let limits = health::AdmissionLimits {
        account: target.account.limits,
        key: credential.limits,
        target: target.target.limits,
    };
    let admission = match state.runtime.health.try_admit(caller, limits, 0) {
        Ok(admission) => admission,
        Err(reason) => {
            let code = unavailable_code(reason);
            return GatewayError::new(
                code,
                format!("账号「{}」暂时不可用（{reason:?}）", target.account.name),
            )
            .with_protocol(protocol)
            .with_request_id(request_id)
            .into_response();
        }
    };

    let url = match upstream::build_url_with_suffix(
        &target.account.base_url,
        &format!("v1/images/tasks/{task_id}"),
    ) {
        Ok(url) => url,
        Err(error) => {
            admission.settle(health::Outcome::Neutral, None);
            return GatewayError::new(
                ErrorCode::InternalError,
                format!(
                    "账号「{}」的 Base URL 无法构造端点：{error}",
                    target.account.name
                ),
            )
            .with_protocol(protocol)
            .with_request_id(request_id)
            .into_response();
        }
    };
    if let Err(error) =
        crate::security::url_guard::assert_resolvable(&url, target.account.allow_private_network)
            .await
    {
        admission.settle(health::Outcome::Neutral, None);
        return GatewayError::new(
            ErrorCode::InternalError,
            format!("账号「{}」的目标地址被拒绝：{error}", target.account.name),
        )
        .with_protocol(protocol)
        .with_request_id(request_id)
        .into_response();
    }

    let response = state
        .upstream
        .http_for(target.account.allow_private_network)
        .get(url)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", credential.secret),
        )
        .timeout(POLL_TIMEOUT)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            // 连不上是"坏"：与推理路径一致地计入熔断（§12.1）。
            admission.settle(health::Outcome::Fault, None);
            let code = if error.is_timeout() {
                ErrorCode::UpstreamTimeout
            } else {
                ErrorCode::UpstreamExhausted
            };
            record_poll(
                &state,
                &group.group.id,
                &target,
                &request_id,
                started_unix,
                started,
                None,
                code.status(),
                Some(code),
            );
            return GatewayError::new(
                code,
                format!("账号「{}」的任务查询失败", target.account.name),
            )
            .with_protocol(protocol)
            .with_request_id(request_id)
            .into_response();
        }
    };

    let upstream_status = response.status();
    let upstream_headers = response.headers().clone();
    let bytes =
        match passthrough::read_upstream_body(response, passthrough::MAX_UPSTREAM_BODY_BYTES).await
        {
            Ok(bytes) => bytes,
            Err(reason) => {
                admission.settle(health::Outcome::Fault, None);
                record_poll(
                    &state,
                    &group.group.id,
                    &target,
                    &request_id,
                    started_unix,
                    started,
                    Some(upstream_status.as_u16()),
                    StatusCode::BAD_GATEWAY,
                    Some(ErrorCode::UpstreamProtocolError),
                );
                return GatewayError::new(
                    ErrorCode::UpstreamProtocolError,
                    format!(
                        "账号「{}」的任务查询响应读取失败：{reason}",
                        target.account.name
                    ),
                )
                .with_protocol(protocol)
                .with_request_id(request_id)
                .into_response();
            }
        };

    // 4xx 不是账号的错（任务过期、参数不对），5xx 才算故障（§12.1）。
    let outcome = if upstream_status.is_server_error() {
        health::Outcome::Fault
    } else {
        health::Outcome::Success
    };
    admission.settle(outcome, None);
    record_poll(
        &state,
        &group.group.id,
        &target,
        &request_id,
        started_unix,
        started,
        Some(upstream_status.as_u16()),
        upstream_status,
        None,
    );

    // 原样回传：状态码、正文与 Retry-After 都是上游对"这个任务现在怎么样"的
    // 权威回答，网关不加工（§14.9 与图片端点同一口径）。
    let mut builder = Response::builder().status(upstream_status);
    if let Some(value) = upstream_headers.get(header::CONTENT_TYPE) {
        builder = builder.header(header::CONTENT_TYPE, value.as_bytes());
    }
    if let Some(value) = upstream_headers.get(header::RETRY_AFTER) {
        builder = builder.header(header::RETRY_AFTER, value.as_bytes());
    }
    builder
        .header("x-akhub-request-id", request_id)
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// 查不到任务时的统一答案。
fn missing_task(request_id: &str) -> Response {
    GatewayError::new(
        ErrorCode::ImageTaskNotFound,
        "任务不存在、已过期，或不属于当前分组",
    )
    .with_protocol(Protocol::OpenAiChat)
    .with_request_id(request_id)
    .into_response()
}

/// 记一条轮询的请求记录（§24.1）。
///
/// 轮询没有逻辑模型：它是对某个已存在任务的查询，硬填一个模型名反而是假信息。
#[allow(clippy::too_many_arguments)]
fn record_poll(
    state: &SharedState,
    group_id: &str,
    target: &TargetView,
    request_id: &str,
    started_unix: i64,
    started: Instant,
    upstream_status: Option<u16>,
    http_status: StatusCode,
    error_code: Option<ErrorCode>,
) {
    state.recorder.record(RequestRecord {
        request_id: request_id.to_string(),
        started_at: started_unix,
        duration_ms: started.elapsed().as_millis() as i64,
        protocol: Protocol::OpenAiChat,
        streaming: false,
        group_id: Some(group_id.to_string()),
        logical_model: None,
        target_id: Some(target.target.id.clone()),
        account_id: Some(target.account.id.clone()),
        upstream_model: Some(target.target.upstream_model.clone()),
        request_bytes: 0,
        upstream_status: upstream_status.map(i64::from),
        http_status: http_status.as_u16() as i64,
        error_code: error_code.map(|code| code.as_str().to_string()),
        endpoint: Some(upstream::Endpoint::ImagesTasks.as_str().to_string()),
        degraded: None,
        effective_multiplier: None,
        cheapest_multiplier: None,
        dearest_multiplier: None,
        attempts: 1,
        queued_ms: 0,
        sticky_hit: false,
        first_token_ms: None,
        input_tokens: None,
        output_tokens: None,
        config_version: Some(state.config.current().version as i64),
        sticky_wait_ms: None,
        sticky_freshness: None,
        sticky_origin: None,
        output_tps: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        multiplier_source: None,
        quota_status: None,
        filter_summary: None,
        selected_layer: None,
        attempts_detail: Vec::new(),
    });
}
