//! Responses 生命周期验收（§15.1、§15.3、§15.4）。
//!
//! 覆盖三件事：
//! - 原生上游有映射时，查询/取消/输入项真的转发给原账号，而不是本地编造；
//! - 没有原生映射（跨协议、store:false）时，取消必须明确拒绝，绝不伪报成功；
//! - `compact` / `input_tokens` 没有经过等价性验证的实现时返回明确的不支持。

mod common;

use akhub::domain::Protocol;
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use common::{FakeUpstream, TargetSpec, client, spawn_akhub, wire_target};
use serde_json::{Value, json};

/// 假上游：实现完整的 Responses 生命周期端点。
async fn spawn_lifecycle_upstream() -> String {
    async fn create(body: String) -> axum::response::Response {
        let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        axum::Json(json!({
            "id": "resp_up",
            "object": "response",
            "model": request.get("model").cloned().unwrap_or(Value::Null),
            "status": "completed",
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "你好"}]}],
            "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8},
        }))
        .into_response()
    }

    async fn retrieve(
        axum::extract::Path(id): axum::extract::Path<String>,
    ) -> axum::response::Response {
        axum::Json(json!({
            "id": id,
            "object": "response",
            "status": "in_progress",
            "marker": "from_upstream_get",
            "output": [],
        }))
        .into_response()
    }

    async fn cancel(
        axum::extract::Path(id): axum::extract::Path<String>,
    ) -> axum::response::Response {
        axum::Json(json!({
            "id": id,
            "object": "response",
            "status": "cancelled",
            "marker": "from_upstream_cancel",
        }))
        .into_response()
    }

    async fn items(
        axum::extract::Path(_id): axum::extract::Path<String>,
    ) -> axum::response::Response {
        axum::Json(json!({
            "object": "list",
            "data": [{"id": "msg_1", "type": "message", "role": "user"}],
            "first_id": "msg_1",
            "last_id": "msg_1",
            "has_more": false,
        }))
        .into_response()
    }

    async fn delete(
        axum::extract::Path(id): axum::extract::Path<String>,
    ) -> axum::response::Response {
        axum::Json(json!({"id": id, "object": "response", "deleted": true})).into_response()
    }

    /// `POST /v1/responses/compact`：回显模型名，证明请求真的被转发。
    async fn compact(body: String) -> axum::response::Response {
        let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        axum::Json(json!({
            "id": "resp_compact",
            "object": "response.compaction",
            "marker": "from_upstream_compact",
            "model": request.get("model").cloned().unwrap_or(Value::Null),
            "output": [{"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "压缩后的历史"}]}],
            "usage": {"input_tokens": 9, "output_tokens": 1, "total_tokens": 10},
        }))
        .into_response()
    }

    /// `POST /v1/responses/input_tokens`：回显模型名与固定计数。
    async fn input_tokens(body: String) -> axum::response::Response {
        let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        axum::Json(json!({
            "object": "response.input_tokens",
            "marker": "from_upstream_input_tokens",
            "input_tokens": 42,
            "model": request.get("model").cloned().unwrap_or(Value::Null),
        }))
        .into_response()
    }

    let app = Router::new()
        .route("/v1/responses", post(create))
        .route("/v1/responses/compact", post(compact))
        .route("/v1/responses/input_tokens", post(input_tokens))
        .route("/v1/responses/{id}", get(retrieve).delete(delete))
        .route("/v1/responses/{id}/cancel", post(cancel))
        .route("/v1/responses/{id}/input_items", get(items));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// 建一轮 Responses 会话，返回网关 ID。
async fn first_turn(akhub: &common::Akhub, model: &str) -> String {
    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": model, "input": "你好"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let id = body["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("resp_akh_"), "必须是网关 ID：{id}");
    assert!(
        !body.to_string().contains("resp_up"),
        "上游 ID 泄漏：{body}"
    );
    id
}

/// 原生映射存在时，查询必须转发到上游（用 marker 证明不是本地回放）。
#[tokio::test]
async fn a_native_retrieve_is_proxied_to_the_original_upstream() {
    let upstream = spawn_lifecycle_upstream().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream,
            Protocol::OpenAiResponses,
            "gpt-5",
            "gpt-5",
            50,
        ),
    )
    .await;
    let id = first_turn(&akhub, "gpt-5").await;

    let got: Value = client()
        .get(format!("{}/v1/responses/{id}", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["marker"], "from_upstream_get", "{got}");
    assert_eq!(got["status"], "in_progress", "不能把本地状态冒充成上游状态");
    assert_eq!(got["id"].as_str(), Some(id.as_str()));
    assert!(!got.to_string().contains("resp_up"), "{got}");
}

/// 原生映射存在时，取消转发给上游并保留其真实结论。
#[tokio::test]
async fn a_native_cancel_is_proxied_and_keeps_the_upstream_verdict() {
    let upstream = spawn_lifecycle_upstream().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream,
            Protocol::OpenAiResponses,
            "gpt-5",
            "gpt-5",
            50,
        ),
    )
    .await;
    let id = first_turn(&akhub, "gpt-5").await;

    let cancelled: Value = client()
        .post(format!("{}/v1/responses/{id}/cancel", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    assert_eq!(cancelled["marker"], "from_upstream_cancel", "{cancelled}");
    assert_eq!(cancelled["id"].as_str(), Some(id.as_str()));
}

/// 原生映射存在时，输入项列表转发给上游。
#[tokio::test]
async fn native_input_items_are_proxied() {
    let upstream = spawn_lifecycle_upstream().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream,
            Protocol::OpenAiResponses,
            "gpt-5",
            "gpt-5",
            50,
        ),
    )
    .await;
    let id = first_turn(&akhub, "gpt-5").await;

    let items: Value = client()
        .get(format!("{}/v1/responses/{id}/input_items", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(items["object"], "list", "{items}");
    assert_eq!(items["data"][0]["id"], "msg_1", "{items}");
}

/// 跨协议响应没有原生生命周期：取消必须明确拒绝，绝不伪报成功。
#[tokio::test]
async fn a_cross_protocol_response_refuses_to_fake_a_cancellation() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    // 账号只有 Chat 端点，客户端的 Responses 请求走跨协议转换。
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "gpt-5",
            "glm-4.6",
            50,
        )
        .pinned(),
    )
    .await;
    let id = first_turn(&akhub, "gpt-5").await;

    let response = client()
        .post(format!("{}/v1/responses/{id}/cancel", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400, "没有原生后台能力就不能假装取消成功");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unsupported_parameter", "{body}");

    let items = client()
        .get(format!("{}/v1/responses/{id}/input_items", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(items.status(), 400);
}

/// `compact` 与 `input_tokens` 在原生 Responses 上游上必须真的转发（§15.4）。
#[tokio::test]
async fn native_compact_and_input_tokens_are_forwarded() {
    let upstream = spawn_lifecycle_upstream().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream,
            Protocol::OpenAiResponses,
            "gpt-5",
            "gpt-5-2026",
            50,
        ),
    )
    .await;

    let compact: Value = client()
        .post(format!("{}/v1/responses/compact", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "很长"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(compact["marker"], "from_upstream_compact", "{compact}");
    assert_eq!(
        compact["model"], "gpt-5-2026",
        "模型名必须按目标改写：{compact}"
    );

    let counted: Value = client()
        .post(format!("{}/v1/responses/input_tokens", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "很长"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(counted["marker"], "from_upstream_input_tokens", "{counted}");
    assert_eq!(counted["input_tokens"], 42, "{counted}");
    assert_eq!(counted["model"], "gpt-5-2026", "{counted}");
}

/// 上游没有这条路由时必须明确 400，而不是 503 让客户端反复重试（§15.4）。
#[tokio::test]
async fn a_missing_native_route_reports_unsupported() {
    // 只实现 /v1/responses 的上游：compact 与 input_tokens 都返回 404。
    async fn create() -> axum::response::Response {
        axum::Json(json!({"id": "resp_up", "object": "response", "output": []})).into_response()
    }
    let app = Router::new().route("/v1/responses", post(create));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &format!("http://{addr}"),
            Protocol::OpenAiResponses,
            "gpt-5",
            "gpt-5",
            50,
        ),
    )
    .await;

    for path in ["/v1/responses/compact", "/v1/responses/input_tokens"] {
        let response = client()
            .post(format!("{}{path}", akhub.base_url))
            .bearer_auth(&akhub.key)
            .json(&json!({"model": "gpt-5", "input": "很长"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "unsupported_parameter", "{body}");
    }
}

/// 非 Responses 上游没有等价端点：必须 400 明确拒绝，未鉴权仍是 401。
#[tokio::test]
async fn compact_and_input_tokens_refuse_on_non_responses_upstreams() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "gpt-5",
            "glm-4.6",
            50,
        )
        .pinned(),
    )
    .await;
    let http = client();

    for path in ["/v1/responses/compact", "/v1/responses/input_tokens"] {
        let payload = json!({"model": "gpt-5", "input": "很长"});
        let unauthenticated = http
            .post(format!("{}{path}", akhub.base_url))
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), 401, "{path}");

        let response = http
            .post(format!("{}{path}", akhub.base_url))
            .bearer_auth(&akhub.key)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "unsupported_parameter", "{body}");
    }
}
