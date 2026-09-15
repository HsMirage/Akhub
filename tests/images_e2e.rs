//! 图片接口端到端验收：JSON generations 与 multipart edits 只原生转发到
//! OpenAI 兼容上游。

mod common;

use std::sync::{Arc, Mutex};

use akhub::domain::Protocol;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use common::{TargetSpec, client, spawn_akhub, wire_target};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
struct SeenImageRequest {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone)]
struct ImageUpstream {
    seen: Arc<Mutex<Vec<SeenImageRequest>>>,
    response_body: Vec<u8>,
}

async fn image_upstream_handler(
    State(upstream): State<ImageUpstream>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    upstream.seen.lock().unwrap().push(SeenImageRequest {
        path: uri.path().to_string(),
        headers,
        body: body.to_vec(),
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        upstream.response_body.clone(),
    )
        .into_response()
}

async fn spawn_image_upstream_at(response_body: &[u8]) -> (String, ImageUpstream) {
    let upstream = ImageUpstream {
        seen: Arc::new(Mutex::new(Vec::new())),
        response_body: response_body.to_vec(),
    };
    let app = Router::new()
        .route("/v1/images/generations", post(image_upstream_handler))
        .route("/v1/images/edits", post(image_upstream_handler))
        .with_state(upstream.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), upstream)
}

async fn spawn_missing_image_upstream() -> (String, Arc<Mutex<usize>>) {
    async fn missing(State(calls): State<Arc<Mutex<usize>>>) -> Response {
        *calls.lock().unwrap() += 1;
        StatusCode::NOT_FOUND.into_response()
    }

    let calls = Arc::new(Mutex::new(0));
    let app = Router::new()
        .fallback(missing)
        .with_state(Arc::clone(&calls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls)
}

fn multipart_body(boundary: &str, model: Option<&str>, image: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(model) = model {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"input.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(image);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn parse_multipart_parts(body: &[u8], content_type: &str) -> Vec<(String, Vec<u8>)> {
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .unwrap()
        .trim_matches('"');
    let delimiter = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = body[cursor..]
        .windows(delimiter.len())
        .position(|window| window == delimiter)
    {
        let boundary_start = cursor + relative;
        let after = boundary_start + delimiter.len();
        if body
            .get(after..)
            .is_some_and(|rest| rest.starts_with(b"--"))
        {
            break;
        }
        let mut part_start = after;
        if body
            .get(part_start..)
            .is_some_and(|rest| rest.starts_with(b"\r\n"))
        {
            part_start += 2;
        } else if body
            .get(part_start..)
            .is_some_and(|rest| rest.starts_with(b"\n"))
        {
            part_start += 1;
        }
        let header_end = find_bytes(body, b"\r\n\r\n", part_start)
            .map(|position| (position, position + 4))
            .or_else(|| {
                find_bytes(body, b"\n\n", part_start).map(|position| (position, position + 2))
            })
            .unwrap();
        let headers = String::from_utf8_lossy(&body[part_start..header_end.0]);
        let name = headers
            .lines()
            .find_map(|line| line.split_once("name=\"").map(|(_, value)| value))
            .and_then(|value| value.split('"').next())
            .unwrap_or_default()
            .to_string();
        let data_start = header_end.1;
        let next = find_bytes(body, &delimiter, data_start).unwrap();
        let mut data_end = next;
        if data_end >= 2 && &body[data_end - 2..data_end] == b"\r\n" {
            data_end -= 2;
        } else if data_end >= 1 && body[data_end - 1] == b'\n' {
            data_end -= 1;
        }
        parts.push((name, body[data_start..data_end].to_vec()));
        cursor = next;
    }
    parts
}

fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

#[tokio::test]
async fn generations_are_forwarded_natively_with_model_rewrite_and_exact_response() {
    let response_body = br#"{"created":1,"data":[{"b64_json":"ZmFrZQ=="}],"marker":"exact"}"#;
    let (upstream_url, upstream) = spawn_image_upstream_at(response_body).await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream_url,
            Protocol::OpenAiChat,
            "image-model",
            "image-model-upstream",
            50,
        ),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/images/generations", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "image-model",
            "prompt": "一只猫",
            "size": "1024x1024",
            "response_format": "b64_json",
            "unknown_extension": {"keep": true}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), response_body);

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let request: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(request["model"], "image-model-upstream");
    assert_eq!(request["response_format"], "b64_json");
    assert_eq!(request["unknown_extension"]["keep"], true);
    assert_eq!(seen[0].path, "/v1/images/generations");
}

#[tokio::test]
async fn edits_preserve_multipart_boundary_and_file_bytes() {
    let (upstream_url, upstream) = spawn_image_upstream_at(br#"{"ok":true}"#).await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream_url,
            Protocol::OpenAiChat,
            "edit-model",
            "edit-model-upstream",
            50,
        ),
    )
    .await;

    let boundary = "images-test-boundary";
    let image = [0_u8, 1, 2, 0xff, 0x10, 0x20];
    let body = multipart_body(boundary, Some("edit-model"), &image);
    let content_type = format!("multipart/form-data; boundary={boundary}");
    let response = client()
        .post(format!("{}/v1/images/edits", akhub.base_url))
        .bearer_auth(&akhub.key)
        .header(header::CONTENT_TYPE, &content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0]
            .headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        content_type
    );
    let parts = parse_multipart_parts(
        &seen[0].body,
        seen[0]
            .headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
    );
    assert_eq!(parts[0].0, "model");
    assert_eq!(parts[0].1, b"edit-model-upstream");
    assert_eq!(parts[1].0, "image");
    assert_eq!(parts[1].1, image);
}

#[tokio::test]
async fn edits_without_model_are_rejected_before_upstream() {
    let (upstream_url, upstream) = spawn_image_upstream_at(br#"{"ok":true}"#).await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream_url,
            Protocol::OpenAiChat,
            "edit-model",
            "edit-model-upstream",
            50,
        ),
    )
    .await;

    let boundary = "missing-model";
    let body = multipart_body(boundary, None, b"file");
    let response = client()
        .post(format!("{}/v1/images/edits", akhub.base_url))
        .bearer_auth(&akhub.key)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "unsupported_parameter");
    assert!(upstream.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn images_refuse_non_openai_accounts_when_adaptive_is_disabled() {
    let (upstream_url, upstream) = spawn_image_upstream_at(br#"{"ok":true}"#).await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream_url,
            Protocol::AnthropicMessages,
            "image-model",
            "image-model-upstream",
            50,
        )
        .pinned(),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/images/generations", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model":"image-model","prompt":"猫"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "unsupported_parameter");
    assert!(upstream.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_native_image_route_returns_terminal_400_without_retry() {
    let (upstream_url, calls) = spawn_missing_image_upstream().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream_url,
            Protocol::OpenAiChat,
            "image-model",
            "image-model-upstream",
            50,
        ),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/images/generations", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model":"image-model","prompt":"猫"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "unsupported_parameter");
    assert_eq!(*calls.lock().unwrap(), 1, "native-only 404 不应重试");
}

#[tokio::test]
async fn images_require_authentication() {
    let akhub = spawn_akhub().await;
    let response = client()
        .post(format!("{}/v1/images/generations", akhub.base_url))
        .json(&json!({"model":"image-model","prompt":"猫"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// multipart 只允许用于图片编辑入口；其它入口必须明确 400。
#[tokio::test]
async fn multipart_on_other_endpoints_is_rejected() {
    let akhub = spawn_akhub().await;
    let body = multipart_body("b", Some("m"), &[1_u8, 2, 3]);
    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=b")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "unsupported_parameter");
}
