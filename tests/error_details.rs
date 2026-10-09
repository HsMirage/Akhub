//! Public failures retain actionable reasons without exposing upstream identity.
mod common;

use akhub::domain::Protocol;
use axum::{Router, response::IntoResponse, routing::post};
use common::{Behavior, FakeUpstream, TargetSpec, client, request, spawn_akhub, wire_target};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const ACCOUNT: &str = "private-channel";
const DETAIL: &str = "private-channel: Unsupported size \"auto\". Supported: 1024x1024, 1024x1536. key=key-private-channel https://private.example/debug";

#[tokio::test]
async fn accepted_image_failure_preserves_reason_without_submitting_twice() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let app = Router::new().route(
        "/v1/images/generations",
        post(move || {
            count.fetch_add(1, Ordering::SeqCst);
            async {
                (
                    axum::http::StatusCode::ACCEPTED,
                    axum::Json(json!({
                        "id":"job_1","object":"image.generation.job","status":"failed",
                        "error":{"message":DETAIL},"debug":"vendor-debug"
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let hub = spawn_akhub().await;
    wire_target(
        &hub,
        TargetSpec::new(
            ACCOUNT,
            &url,
            Protocol::OpenAiChat,
            "test-model",
            "test-model",
            50,
        )
        .pinned(),
    )
    .await;
    let response = client()
        .post(format!("{}/v1/images/generations", hub.base_url))
        .bearer_auth(&hub.key)
        .json(&json!({"model":"test-model","prompt":"hello","size":"auto"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert_reason(&response.text().await.unwrap());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

fn assert_reason(body: &str) {
    assert!(body.contains("Unsupported size"), "{body}");
    assert!(
        body.contains("auto") && body.contains("1024x1024") && body.contains("1024x1536"),
        "{body}"
    );
    assert!(
        !body.contains(ACCOUNT) && !body.contains("private.example"),
        "{body}"
    );
    assert!(!body.contains("vendor-debug"), "{body}");
}

#[tokio::test]
async fn http_parameter_failures_preserve_reason_and_status_in_all_entrypoints() {
    for (protocol, path) in [
        (Protocol::OpenAiChat, "/v1/images/generations"),
        (Protocol::OpenAiChat, "/v1/chat/completions"),
        (Protocol::AnthropicMessages, "/v1/messages"),
        (Protocol::OpenAiResponses, "/v1/responses"),
    ] {
        let app = Router::new().route(
            path,
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error":{"message":DETAIL},"debug":"vendor-debug"})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let hub = spawn_akhub().await;
        wire_target(
            &hub,
            TargetSpec::new(ACCOUNT, &url, protocol, "test-model", "test-model", 50).pinned(),
        )
        .await;
        let body = if path.contains("images") {
            json!({"model":"test-model","prompt":"hello","size":"auto"})
        } else if protocol == Protocol::OpenAiResponses {
            json!({"model":"test-model","input":"hello"})
        } else {
            json!({"model":"test-model","max_tokens":10,"messages":[{"role":"user","content":"hello"}]})
        };
        let response = client()
            .post(format!("{}{path}", hub.base_url))
            .bearer_auth(&hub.key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(response.headers().contains_key("x-akhub-request-id"));
        assert_reason(&response.text().await.unwrap());
    }
}

#[tokio::test]
async fn exhausted_upstream_keeps_safe_reason() {
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::Json(503, json!({"error":{"message":DETAIL}})));
    let hub = spawn_akhub().await;
    wire_target(
        &hub,
        TargetSpec::new(
            ACCOUNT,
            &upstream.base_url,
            Protocol::OpenAiChat,
            "test-model",
            "test-model",
            50,
        )
        .pinned(),
    )
    .await;
    let response = request(
        &hub,
        Protocol::OpenAiChat,
        json!({"model":"test-model","messages":[{"role":"user","content":"hello"}]}),
    )
    .await;
    assert_eq!(response.status(), 503);
    assert_reason(&response.text().await.unwrap());
}

#[tokio::test]
async fn native_and_translated_streams_keep_error_reason_without_editing_normal_content() {
    let error = json!({"type":"error","error":{"message":DETAIL},"debug":"vendor-debug"});
    let frames = format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"role\":\"assistant\"}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"normal content\"}}}}\n\nevent: error\ndata: {error}\n\n"
    );
    let app = Router::new().route(
        "/v1/messages",
        post(move || {
            let frames = frames.clone();
            async move { ([("content-type", "text/event-stream")], frames).into_response() }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for protocol in [
        Protocol::AnthropicMessages,
        Protocol::OpenAiChat,
        Protocol::OpenAiResponses,
    ] {
        let hub = spawn_akhub().await;
        wire_target(
            &hub,
            TargetSpec::new(
                ACCOUNT,
                &url,
                Protocol::AnthropicMessages,
                "test-model",
                "test-model",
                50,
            )
            .pinned(),
        )
        .await;
        let mut body: Value = json!({"model":"test-model","stream":true,"max_tokens":10,"messages":[{"role":"user","content":"hello"}]});
        if protocol == Protocol::OpenAiResponses {
            body = json!({"model":"test-model","stream":true,"input":"hello"});
        }
        let response = request(&hub, protocol, body).await;
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(text.contains("normal content"), "{text}");
        assert_reason(&text);
    }
}
