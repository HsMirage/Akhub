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

/// 普通图片入口的 202 已经是接单承诺。只查询同一账号、同一凭据的任务，
/// 任何后续失败都交由调用方终止本次请求，绝不能重新 POST 或换号生成。
pub(crate) async fn complete_job(
    state: &SharedState,
    target: &TargetView,
    api_key: &str,
    response: reqwest::Response,
    remaining: Duration,
    request_id: &str,
) -> Result<serde_json::Value, GatewayError> {
    let redactor = crate::security::redact::ErrorRedactor::new(
        &target.account.name,
        &target.account.base_url,
        api_key,
    );
    let result = tokio::time::timeout(remaining, async {
        let mut wait = poll_delay(response.headers());
        let mut value = read_job(response).await?;
        let kind = value
            .get("object")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let path = match kind.as_str() {
            "image.generation.job" => "v1/images/generations",
            "image.generation.task" => "v1/images/tasks",
            _ => return Err(job_error("图片入口返回了无法识别的接单对象")),
        };
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 256
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .ok_or_else(|| job_error("生图任务缺少合法的任务编号"))?
            .to_owned();
        let url = upstream::build_url_with_segments(&target.account.base_url, path, [id.as_str()])
            .map_err(|_| job_error("无法构造生图任务查询地址"))?;
        tracing::info!(
            request_id,
            account = target.account.name,
            task_id = id,
            "图片请求已接单，等待同一任务完成"
        );
        loop {
            match value.get("status").and_then(|v| v.as_str()) {
                Some("succeeded") => return completed_image(&value),
                Some("completed") if kind == "image.generation.task" => {
                    return completed_image(
                        value
                            .get("result")
                            .ok_or_else(|| job_error("任务缺少图片结果"))?,
                    );
                }
                Some("failed" | "blocked" | "cancelled") => {
                    let detail = passthrough::upstream_error_message(value.to_string().as_bytes())
                        .unwrap_or_else(|| "生图任务未成功完成".into());
                    tracing::warn!(request_id, account = target.account.name, %detail, "生图任务失败");
                    return Err(job_error(detail));
                }
                Some("processing") => {}
                _ => return Err(job_error("生图任务返回了未知状态")),
            }
            tokio::time::sleep(wait).await;
            crate::security::url_guard::assert_resolvable(
                &url,
                target.account.allow_private_network,
            )
            .await
            .map_err(|_| job_error("生图任务查询地址被拒绝"))?;
            let response = match state
                .upstream
                .http_for(target.account.allow_private_network)
                .get(url.clone())
                .bearer_auth(api_key)
                .timeout(POLL_TIMEOUT)
                .send()
                .await
            {
                Ok(response) => response,
                // 查询可重试，但始终只查同一个任务；总截止时间覆盖连接与正文。
                Err(_) => {
                    wait = Duration::from_secs(2);
                    continue;
                }
            };
            wait = poll_delay(response.headers());
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                continue;
            }
            if !status.is_success() {
                let bytes = passthrough::read_upstream_body(response, passthrough::MAX_UPSTREAM_BODY_BYTES).await.unwrap_or_default();
                let detail = passthrough::upstream_error_message(&bytes)
                    .unwrap_or_else(|| format!("生图任务查询返回 {}", status.as_u16()));
                return Err(job_error(detail));
            }
            value = read_job(response).await?;
            if value.get("id").and_then(|v| v.as_str()) != Some(id.as_str())
                || value.get("object").and_then(|v| v.as_str()) != Some(kind.as_str())
            {
                return Err(job_error("生图任务查询返回了不匹配的任务"));
            }
        }
    })
    .await;
    result
        .unwrap_or_else(|_| {
            Err(GatewayError::new(
                ErrorCode::UpstreamTimeout,
                "图片任务已接单，但等待结果超时；未重复下单",
            ))
        })
        .map_err(|error| {
            let detail = redactor.message(&error.message);
            error
                .with_public_message(detail)
                .with_request_id(request_id)
        })
}

fn job_error(message: impl Into<String>) -> GatewayError {
    GatewayError::new(ErrorCode::UpstreamProtocolError, message)
}

fn poll_delay(headers: &HeaderMap) -> Duration {
    Duration::from_secs(
        headers
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2)
            .clamp(1, 30),
    )
}

async fn read_job(response: reqwest::Response) -> Result<serde_json::Value, GatewayError> {
    let bytes = passthrough::read_upstream_body(response, passthrough::MAX_UPSTREAM_BODY_BYTES)
        .await
        .map_err(|_| job_error("无法读取生图任务响应"))?;
    serde_json::from_slice(&bytes).map_err(|_| job_error("生图任务返回的正文不是 JSON"))
}

/// 去掉供应商任务元数据，只返回标准图片响应；不访问或下载图片 URL。
fn completed_image(job: &serde_json::Value) -> Result<serde_json::Value, GatewayError> {
    let data = job
        .get("data")
        .and_then(|v| v.as_array())
        .filter(|items| !items.is_empty())
        .ok_or_else(|| job_error("任务成功但没有图片"))?;
    let mut images = Vec::with_capacity(data.len());
    for item in data {
        let mut image = serde_json::Map::new();
        for field in ["url", "b64_json", "revised_prompt"] {
            if let Some(value) = item
                .get(field)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                image.insert(field.into(), serde_json::json!(value));
            }
        }
        if !image.contains_key("url") && !image.contains_key("b64_json") {
            return Err(job_error("任务成功但图片数据不完整"));
        }
        images.push(serde_json::Value::Object(image));
    }
    let mut result = serde_json::json!({"created": job.get("created").and_then(|v| v.as_i64())
        .unwrap_or_else(crate::storage::now_unix), "data":images});
    if let Some(usage) = job.get("usage") {
        result["usage"] = usage.clone();
    }
    Ok(result)
}

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

    // 分组是查询条件的一部分："查不到"、"已过期"与"不是本组的任务"在这里
    // 就是同一件事，任务 ID 不会成为跨组探测工具（§26.8）。过期也在这里判，
    // 不依赖 600 秒一轮的后台清理（§14.9）。
    let row = match state
        .store
        .image_task(&group.group.id, &task_id, started_unix)
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return missing_task(&state, &group.group.id, &request_id, started_unix, started);
        }
        Err(error) => {
            tracing::warn!(%error, request_id, "读取异步生图任务定位失败");
            return GatewayError::new(ErrorCode::InternalError, "读取任务定位失败")
                .with_protocol(protocol)
                .with_request_id(request_id)
                .into_response();
        }
    };
    let Some(target) = config.target_by_account(&row.account_id).cloned() else {
        // 账号被删、或已经不再被任何分组引用：这个任务无处可问。
        return missing_task(&state, &group.group.id, &request_id, started_unix, started);
    };

    // 轮询必须用**当初那把 Key**：上游的任务是按凭据隔离的资源，换一把 Key 去
    // 问同一个任务 ID，上游只会当作不存在（§4.2.1 的不变量 A）。
    //
    // 摘要对不上（老记录没存、或那把 Key 已被删）时退回账号的第一把 Key：
    // 多 Key 账号上这可能问出"没这个任务"，但直接拒绝会让"上游根本不按 Key
    // 隔离任务"的站点整条不可用。两者相权，先试一次更划算。
    let credentials = state.runtime.credentials.current();
    // 两处都必须只要**启用中**的 Key：管理员停用一把泄露的 Key 之后，此前由它
    // 接单的任务不能继续拿它发请求（§4.2.1 的不变量 A）。这条直连路径绕过了
    // 调度器的 select_key，所以过滤要在这里自己做。
    let credential = row
        .key_digest
        .as_deref()
        .and_then(|digest| credentials.enabled_by_digest(&row.account_id, digest))
        .or_else(|| credentials.first_enabled(&row.account_id))
        .cloned();
    drop(credentials);
    if credential.is_none() {
        tracing::warn!(
            request_id,
            account = target.account.name,
            "账号没有可用凭据，异步生图任务无法轮询"
        );
    }
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

    // 任务 ID 是**上游签发、下游可控**的字符串，必须整段编码后再拼 URL：
    // 直接插进路径会让 `..%2F..%2F...` 这类值被 WHATWG 折叠成另一个上游路由，
    // 网关会带着本账号的凭据去请求它（§23.4）。
    let url = match upstream::build_url_with_segments(
        &target.account.base_url,
        "v1/images/tasks",
        [task_id.as_str()],
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
                Some(&target),
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
                    Some(&target),
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
        Some(&target),
        &request_id,
        started_unix,
        started,
        Some(upstream_status.as_u16()),
        upstream_status,
        None,
    );

    if !upstream_status.is_success() {
        let redactor = crate::security::redact::ErrorRedactor::new(
            &target.account.name,
            &target.account.base_url,
            credential.secret.as_ref(),
        );
        let mut response = crate::gateway::error::upstream_response(
            upstream_status,
            protocol,
            &request_id,
            redactor.payload(&serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)),
        );
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        if let Some(value) = upstream_headers.get(header::RETRY_AFTER) {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, value.clone());
        }
        return response;
    }
    let redactor = crate::security::redact::ErrorRedactor::new(
        &target.account.name,
        &target.account.base_url,
        credential.secret.as_ref(),
    );
    let bytes = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(mut value) => {
            if crate::gateway::error::sanitize_failure(&mut value, &redactor) {
                axum::body::Bytes::from(value.to_string())
            } else {
                bytes
            }
        }
        Err(_) => {
            return crate::gateway::error::upstream_response(
                StatusCode::BAD_GATEWAY,
                protocol,
                &request_id,
                redactor.message("上游任务查询响应不是合法 JSON"),
            );
        }
    };
    // 原样回传：状态码、正文与 Retry-After 都是上游对"这个任务现在怎么样"的
    // 权威回答，网关不加工（§14.9 与图片端点同一口径）。
    //
    // 但**必须显式 no-store**：同一个 URL 的答案随任务进度变化，任何中间层
    // 缓存住它，客户端就会永远看到"处理中"。上游自己带 cache-control 时以它为
    // 准；没带也不能让缓存替我们决定。
    let mut builder = Response::builder().status(upstream_status);
    if let Some(value) = upstream_headers.get(header::CONTENT_TYPE) {
        builder = builder.header(header::CONTENT_TYPE, value.as_bytes());
    }
    if let Some(value) = upstream_headers.get(header::RETRY_AFTER) {
        builder = builder.header(header::RETRY_AFTER, value.as_bytes());
    }
    match upstream_headers.get(header::CACHE_CONTROL) {
        Some(value) => builder = builder.header(header::CACHE_CONTROL, value.as_bytes()),
        None => builder = builder.header(header::CACHE_CONTROL, "no-store"),
    }
    builder
        .header("x-akhub-request-id", request_id)
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// 查不到任务时的统一答案。
///
/// 一样落一条请求记录：客户端"一直 404"是运维真会遇到的现场，记录里没有它
/// 就只能靠猜（§24.1）。这时还没有目标，账号/目标留空——不编造。
fn missing_task(
    state: &SharedState,
    group_id: &str,
    request_id: &str,
    started_unix: i64,
    started: Instant,
) -> Response {
    record_poll(
        state,
        group_id,
        None,
        request_id,
        started_unix,
        started,
        None,
        ErrorCode::ImageTaskNotFound.status(),
        Some(ErrorCode::ImageTaskNotFound),
    );
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
    target: Option<&TargetView>,
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
        target_id: target.map(|target| target.target.id.clone()),
        account_id: target.map(|target| target.account.id.clone()),
        upstream_model: target.map(|target| target.target.upstream_model.clone()),
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
        filter_details: None,
        selected_layer: None,
        attempts_detail: Vec::new(),
    });
}
