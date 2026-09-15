//! 下游入口路由与请求生命周期（§7.1、§8）。

pub mod error;
pub mod models;
pub mod passthrough;
pub mod responses;
pub mod settle;
pub mod stream;
pub mod translate;

use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::app::SharedState;
use crate::auth;
use crate::upstream::Endpoint;

/// 组装 `/v1` 下的全部公开接口。
pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(responses_create))
        // Responses 的查询、删除与取消（§15.1、§15.2）。全部走网关 ID。
        .route(
            "/v1/responses/{id}",
            get(responses::retrieve).delete(responses::destroy),
        )
        .route("/v1/responses/{id}/cancel", post(responses::cancel))
        .route(
            "/v1/responses/{id}/input_items",
            get(responses::input_items),
        )
        // 静态段优先于参数段，所以这两个不会被 `{id}` 吃掉。两者都只能原生
        // 转发（§15.4）：走完整调度链路，拿不到原生上游就明确返回不支持。
        .route("/v1/responses/compact", post(responses_compact))
        .route("/v1/responses/input_tokens", post(responses_input_tokens))
        .route("/v1/models", get(models::list))
        .route("/v1/models/{model}", get(models::get))
}

async fn chat_completions(state: State<SharedState>, headers: HeaderMap, body: Body) -> Response {
    handle(state, headers, body, Endpoint::ChatCompletions).await
}

async fn messages(state: State<SharedState>, headers: HeaderMap, body: Body) -> Response {
    handle(state, headers, body, Endpoint::Messages).await
}

async fn count_tokens(state: State<SharedState>, headers: HeaderMap, body: Body) -> Response {
    handle(state, headers, body, Endpoint::CountTokens).await
}

async fn responses_create(state: State<SharedState>, headers: HeaderMap, body: Body) -> Response {
    handle(state, headers, body, Endpoint::Responses).await
}

/// `POST /v1/responses/compact`：只能原生转发，没有等价适配器（§15.4）。
async fn responses_compact(state: State<SharedState>, headers: HeaderMap, body: Body) -> Response {
    handle(state, headers, body, Endpoint::ResponsesCompact).await
}

/// `POST /v1/responses/input_tokens`：只能原生转发（§15.4）。
async fn responses_input_tokens(
    state: State<SharedState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    handle(state, headers, body, Endpoint::ResponsesInputTokens).await
}

/// §8 中描述的请求处理流程（第一期部分）。
async fn handle(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Body,
    endpoint: Endpoint,
) -> Response {
    let started_at = Instant::now();
    let started_unix = crate::storage::now_unix();
    // 请求 ID 在最早期生成，之后每一条日志和响应都带着它。
    let request_id = format!("req_{}", ulid::Ulid::generate());
    let protocol = endpoint.protocol();

    let credential = match auth::extract_credential(&headers) {
        Ok(credential) => credential,
        Err(error) => {
            return error
                .with_protocol(protocol)
                .with_request_id(request_id)
                .into_response();
        }
    };

    // 配置快照只读一次，整个请求生命周期都用同一份（§21）。
    let config = state.config.current();
    let group = match auth::authenticate(&config, &state.key_digest, &credential) {
        Ok(group) => std::sync::Arc::clone(group),
        Err(error) => {
            return error
                .with_protocol(protocol)
                .with_request_id(request_id)
                .into_response();
        }
    };

    let (body, request_bytes) = match passthrough::read_body(
        body,
        state.settings.max_request_bytes,
        protocol,
        &state.data_dir.join(crate::app::TEMP_DIR_NAME),
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(error) => return error.with_request_id(request_id).into_response(),
    };

    let logical_model = match passthrough::extract_model(&body, protocol) {
        Ok(model) => model,
        Err(error) => return error.with_request_id(request_id).into_response(),
    };

    // Responses 状态链在入口一次性处理：准备原生续链与可重放合并体、或
    // 立即报过期（§15.2）。请求体保持客户端原样；合并体挂在计划里，由
    // 被选中的候选决定用哪一条路。
    let mut chain = responses::ChainPlan::new(group.group.id.clone(), logical_model.clone());
    let mut body = body;
    if let Err(error) = responses::resolve_request(&state, &mut chain, &mut body, protocol).await {
        return error.with_request_id(request_id).into_response();
    }

    passthrough::forward(passthrough::Forward {
        state: &state,
        group: &group,
        endpoint,
        request_id: &request_id,
        downstream_headers: &headers,
        body,
        logical_model,
        request_bytes,
        started_at,
        started_unix,
        chain,
    })
    .await
}
