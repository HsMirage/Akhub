//! 图片接口端到端验收：JSON generations 与 multipart edits 只原生转发到
//! OpenAI 兼容上游。

mod common;

use std::sync::{Arc, Mutex};

use akhub::domain::{Limits, Protocol};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use common::{TargetSpec, api_key_of, client, spawn_akhub, wire_target};
use serde_json::{Value, json};

/// 同步入口接到任务式供应商：只 POST 一次，其后通过同账号 GET 取结果。
async fn spawn_job_upstream(
    initial: Value,
    polls: Vec<(StatusCode, Value)>,
) -> (String, ImageUpstream) {
    let upstream = ImageUpstream {
        seen: Arc::new(Mutex::new(Vec::new())),
        response_body: Vec::new(),
        content_type: "application/json",
    };
    let seen = upstream.seen.clone();
    let polls = Arc::new(Mutex::new(std::collections::VecDeque::from(polls)));
    let handler = move |method: axum::http::Method, uri: Uri, headers: HeaderMap, body: Bytes| {
        let seen = seen.clone();
        let initial = initial.clone();
        let polls = polls.clone();
        async move {
            seen.lock().unwrap().push(SeenImageRequest {
                method: method.to_string(),
                path: uri.path().into(),
                headers,
                body: body.to_vec(),
            });
            let (status, value) = if method == axum::http::Method::POST {
                (StatusCode::ACCEPTED, initial)
            } else {
                polls.lock().unwrap().pop_front().unwrap_or((
                    StatusCode::OK,
                    json!({"id":"job_1","object":"image.generation.job","status":"processing"}),
                ))
            };
            (status, [("retry-after", "1")], axum::Json(value))
        }
    };
    let app = Router::new()
        .route("/v1/images/generations", post(handler.clone()))
        .route("/v1/images/edits", post(handler.clone()))
        .route(
            "/v1/images/generations/{id}",
            axum::routing::get(handler.clone()),
        )
        .route("/v1/images/tasks/{id}", axum::routing::get(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, upstream)
}

fn processing_job() -> Value {
    json!({"id":"job_1","object":"image.generation.job","status":"processing","created":1})
}

#[tokio::test]
async fn sub2api_task_results_are_unwrapped_on_synchronous_endpoints() {
    let initial = json!({"id":"job_1","object":"image.generation.task","status":"processing"});
    let (url, upstream) = spawn_job_upstream(
        initial,
        vec![(
            StatusCode::OK,
            json!({"id":"job_1","object":"image.generation.task","status":"completed",
            "result":{"created":10,"data":[{"b64_json":"YWJj"}]}}),
        )],
    )
    .await;
    let hub = spawn_akhub().await;
    wire_target(
        &hub,
        TargetSpec::new("sub2api", &url, Protocol::OpenAiChat, "m", "m", 100),
    )
    .await;
    let response = client()
        .post(format!("{}/v1/images/generations", hub.base_url))
        .bearer_auth(&hub.key)
        .json(&json!({"model":"m","prompt":"test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"created":10,"data":[{"b64_json":"YWJj"}]})
    );
    assert_eq!(
        upstream.seen.lock().unwrap()[1].path,
        "/v1/images/tasks/job_1"
    );
}

#[tokio::test]
async fn synchronous_images_wait_for_a_job_and_return_standard_images() {
    for endpoint in ["generations", "edits"] {
        let (url, upstream) = spawn_job_upstream(
            processing_job(),
            vec![
                (StatusCode::TOO_MANY_REQUESTS, json!({"error":"busy"})),
                (
                    StatusCode::OK,
                    json!({"id":"job_1","object":"image.generation.job","status":"succeeded",
                "created":123,"model":"private-provider-model","prompt":"private",
                "data":[{"url":"https://images.example/test.png","vendor":"private"}]}),
                ),
            ],
        )
        .await;
        let hub = spawn_akhub().await;
        let wired = wire_target(
            &hub,
            TargetSpec::new(
                "private-provider",
                &url,
                Protocol::OpenAiChat,
                "image-model",
                "vendor-image",
                100,
            ),
        )
        .await;
        let request = client()
            .post(format!("{}/v1/images/{endpoint}", hub.base_url))
            .bearer_auth(&hub.key);
        let response = if endpoint == "edits" {
            request
                .header("content-type", "multipart/form-data; boundary=test")
                .body(multipart_body("test", Some("image-model"), b"image"))
                .send()
                .await
                .unwrap()
        } else {
            request
                .json(&json!({"model":"image-model","prompt":"test"}))
                .send()
                .await
                .unwrap()
        };
        assert_eq!(response.status(), StatusCode::OK);
        let result: Value = response.json().await.unwrap();
        assert_eq!(
            result,
            json!({"created":123,"data":[{"url":"https://images.example/test.png"}]})
        );
        let seen = upstream.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].method, "POST");
        for query in &seen[1..] {
            assert_eq!(query.method, "GET");
            assert_eq!(query.path, "/v1/images/generations/job_1");
            assert!(query.body.is_empty());
            assert_eq!(
                api_key_of(&query.headers).as_deref(),
                Some(wired.api_key.as_str())
            );
        }
    }
}

#[tokio::test]
async fn accepted_image_jobs_never_fail_over_or_repost() {
    for poll in [
        (StatusCode::FORBIDDEN, json!({"error":"private-provider"})),
        (
            StatusCode::OK,
            json!({"id":"job_1","object":"image.generation.job","status":"failed","error":"private-provider"}),
        ),
        (
            StatusCode::OK,
            json!({"id":"job_1","object":"image.generation.job","status":"blocked"}),
        ),
        (
            StatusCode::OK,
            json!({"id":"wrong_job","object":"image.generation.job","status":"succeeded","data":[{"url":"bad"}]}),
        ),
        (
            StatusCode::OK,
            json!({"id":"job_1","object":"image.generation.job","status":"succeeded","data":[]}),
        ),
    ] {
        let (url, upstream) = spawn_job_upstream(processing_job(), vec![poll]).await;
        let hub = spawn_akhub().await;
        wire_target(
            &hub,
            TargetSpec::new(
                "private-provider",
                &url,
                Protocol::OpenAiChat,
                "image-model",
                "vendor-image",
                100,
            ),
        )
        .await;
        let (backup_url, backup) =
            spawn_image_upstream_at(br#"{"created":1,"data":[{"url":"backup"}]}"#).await;
        wire_target(
            &hub,
            TargetSpec::new(
                "backup",
                &backup_url,
                Protocol::OpenAiChat,
                "image-model",
                "vendor-image",
                10,
            ),
        )
        .await;
        let response = client()
            .post(format!("{}/v1/images/generations", hub.base_url))
            .bearer_auth(&hub.key)
            .json(&json!({"model":"image-model","prompt":"test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let result = response.text().await.unwrap();
        assert!(result.contains("upstream_protocol_error"));
        assert!(!result.contains("private-provider"));
        assert_eq!(
            upstream
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.method == "POST")
                .count(),
            1
        );
        assert!(
            backup.seen.lock().unwrap().is_empty(),
            "已接单后不得换号重复计费"
        );
    }
}

#[tokio::test]
async fn accepted_jobs_obey_total_timeout_and_reject_unsafe_ids() {
    for id in ["job_1", "../../other"] {
        let mut initial = processing_job();
        initial["id"] = json!(id);
        let (url, upstream) = spawn_job_upstream(initial, vec![]).await;
        let hub = common::spawn_akhub_with(
            akhub::app::Settings {
                request_timeout: std::time::Duration::from_millis(150),
                ..Default::default()
            },
            |_| {},
        )
        .await;
        wire_target(
            &hub,
            TargetSpec::new(
                "private-provider",
                &url,
                Protocol::OpenAiChat,
                "image-model",
                "vendor-image",
                100,
            ),
        )
        .await;
        let response = client()
            .post(format!("{}/v1/images/generations", hub.base_url))
            .bearer_auth(&hub.key)
            .json(&json!({"model":"image-model","prompt":"test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if id == "job_1" {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            }
        );
        assert_eq!(
            upstream.seen.lock().unwrap().len(),
            1,
            "只应下单一次，不能查询危险路径"
        );
    }
}

#[derive(Debug, Clone)]
struct SeenImageRequest {
    method: String,
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone)]
struct ImageUpstream {
    seen: Arc<Mutex<Vec<SeenImageRequest>>>,
    response_body: Vec<u8>,
    content_type: &'static str,
}

async fn image_upstream_handler(
    State(upstream): State<ImageUpstream>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    upstream.seen.lock().unwrap().push(SeenImageRequest {
        method: method.to_string(),
        path: uri.path().to_string(),
        headers,
        body: body.to_vec(),
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, upstream.content_type)],
        upstream.response_body.clone(),
    )
        .into_response()
}

async fn spawn_image_upstream_at(response_body: &[u8]) -> (String, ImageUpstream) {
    spawn_image_upstream_typed("application/json", response_body).await
}

/// 指定响应类型的假图片上游：流式生图返回的是 SSE。
async fn spawn_image_upstream_typed(
    content_type: &'static str,
    response_body: &[u8],
) -> (String, ImageUpstream) {
    let upstream = ImageUpstream {
        seen: Arc::new(Mutex::new(Vec::new())),
        response_body: response_body.to_vec(),
        content_type,
    };
    let app = Router::new()
        .route("/v1/images/generations", post(image_upstream_handler))
        .route("/v1/images/edits", post(image_upstream_handler))
        .route("/v1/images/variations", post(image_upstream_handler))
        // 图片编辑动辄几 MB：假上游也必须能收下真实尺寸的正文，否则测的是
        // axum 的 2 MB 默认上限，而不是网关的行为。
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(upstream.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), upstream)
}

/// 异步生图的假上游：下单回 202 任务对象，轮询回任务状态（形状照 sub2api）。
async fn spawn_async_image_upstream() -> (String, ImageUpstream) {
    const SUBMIT_BODY: &[u8] = br#"{"id":"task_up_1","task_id":"up_1","object":"image.task","status":"processing","created_at":1,"expires_at":2}"#;
    const POLL_BODY: &[u8] = br#"{"id":"task_up_1","object":"image.task","status":"processing"}"#;

    async fn handler(
        State(upstream): State<ImageUpstream>,
        method: axum::http::Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        upstream.seen.lock().unwrap().push(SeenImageRequest {
            method: method.to_string(),
            path: uri.path().to_string(),
            headers,
            body: body.to_vec(),
        });
        if uri.path().ends_with("/async") {
            return (
                StatusCode::ACCEPTED,
                [(header::CONTENT_TYPE, "application/json")],
                SUBMIT_BODY.to_vec(),
            )
                .into_response();
        }
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::RETRY_AFTER, "3"),
            ],
            POLL_BODY.to_vec(),
        )
            .into_response()
    }

    let upstream = ImageUpstream {
        seen: Arc::new(Mutex::new(Vec::new())),
        response_body: Vec::new(),
        content_type: "application/json",
    };
    let app = Router::new()
        .route("/v1/images/generations/async", axum::routing::post(handler))
        .route("/v1/images/edits/async", axum::routing::post(handler))
        .route("/v1/images/tasks/{task_id}", axum::routing::get(handler))
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

/// 等一条"生图任务轮询"的请求记录落库（写入是攒批的，最多 1 秒刷一次）。
async fn wait_for_task_record(akhub: &common::Akhub) -> akhub::storage::store::RequestRecord {
    for _ in 0..100 {
        let records = akhub.state.store.list_request_records(10, 0).await.unwrap();
        if let Some(record) = records
            .into_iter()
            .find(|record| record.endpoint.as_deref() == Some("images_tasks"))
        {
            return record;
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    panic!("生图任务轮询的记录没有在预期时间内落库");
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

/// 追加一个普通表单字段。
fn push_multipart_field(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .as_bytes(),
    );
}

/// 追加一个文件 part（图片本体，带 filename）。
fn push_multipart_file(
    body: &mut Vec<u8>,
    boundary: &str,
    name: &str,
    filename: &str,
    bytes: &[u8],
) {
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
}

fn finish_multipart(body: &mut Vec<u8>, boundary: &str) {
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
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

/// 图生图的流式：stream 是 multipart 表单字段，响应是 SSE。网关必须原样转发
/// 事件流——把 SSE 当 JSON 解析会在上游已经生成图片之后才报错。
#[tokio::test]
async fn multipart_edits_with_stream_keep_the_sse_response() {
    let frames = concat!(
        "event: image_generation.partial_image\n",
        "data: {\"type\":\"image_generation.partial_image\",\"partial_image_index\":0,\"b64_json\":\"QUJD\"}\n\n",
        "event: image_generation.completed\n",
        "data: {\"type\":\"image_generation.completed\",\"b64_json\":\"QUJDRA==\",\"usage\":{\"input_tokens\":12,\"output_tokens\":1056,\"total_tokens\":1068}}\n\n",
        "data: [DONE]\n\n",
    );
    let (upstream_url, upstream) =
        spawn_image_upstream_typed("text/event-stream", frames.as_bytes()).await;
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

    let boundary = "stream-boundary";
    let mut body = Vec::new();
    // 字段顺序打乱：stream 与 partial_images 都在图片之后。
    push_multipart_file(&mut body, boundary, "image", "in.png", &[1_u8, 2, 3, 4]);
    push_multipart_field(&mut body, boundary, "stream", "true");
    push_multipart_field(&mut body, boundary, "partial_images", "3");
    push_multipart_field(&mut body, boundary, "model", "edit-model");
    finish_multipart(&mut body, boundary);

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
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );
    assert_eq!(
        response.text().await.unwrap(),
        frames,
        "SSE 事件流必须逐字节透传，usage 收尾帧也不能被当成 Chat 收尾块丢掉"
    );

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let content_type = seen[0]
        .headers
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let parts = parse_multipart_parts(&seen[0].body, &content_type);
    assert_eq!(parts[0].0, "image");
    assert_eq!(parts[0].1, vec![1_u8, 2, 3, 4], "图片字节不能被改写");
    assert_eq!(parts[1], ("stream".to_string(), b"true".to_vec()));
    assert_eq!(parts[2], ("partial_images".to_string(), b"3".to_vec()));
    assert_eq!(
        parts[3],
        ("model".to_string(), b"edit-model-upstream".to_vec())
    );
}

/// OpenAI 的第三个图片端点：变体图同样是 multipart 原生透传。
#[tokio::test]
async fn variations_are_forwarded_natively() {
    let response_body = br#"{"created":2,"data":[{"b64_json":"dmFy"}]}"#;
    let (upstream_url, upstream) = spawn_image_upstream_at(response_body).await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream_url,
            Protocol::OpenAiChat,
            "variation-model",
            "variation-model-upstream",
            50,
        ),
    )
    .await;

    let boundary = "variation-boundary";
    let mut body = Vec::new();
    push_multipart_field(&mut body, boundary, "model", "variation-model");
    push_multipart_field(&mut body, boundary, "n", "2");
    push_multipart_file(&mut body, boundary, "image", "src.png", &[9_u8, 8, 7]);
    finish_multipart(&mut body, boundary);

    let response = client()
        .post(format!("{}/v1/images/variations", akhub.base_url))
        .bearer_auth(&akhub.key)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), response_body);

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/images/variations");
    let content_type = seen[0]
        .headers
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let parts = parse_multipart_parts(&seen[0].body, &content_type);
    assert_eq!(
        parts[0],
        ("model".to_string(), b"variation-model-upstream".to_vec())
    );
    assert_eq!(parts[1], ("n".to_string(), b"2".to_vec()));
    assert_eq!(parts[2].0, "image");
    assert_eq!(parts[2].1, vec![9_u8, 8, 7]);
}

/// JSON 入口的流式生图：最终事件同时带图片数据与 usage，两个都必须到达客户端。
#[tokio::test]
async fn generations_with_stream_keep_every_image_event() {
    let frames = concat!(
        "event: image_generation.partial_image\n",
        "data: {\"type\":\"image_generation.partial_image\",\"partial_image_index\":0,\"b64_json\":\"QUJD\"}\n\n",
        "event: image_generation.completed\n",
        "data: {\"type\":\"image_generation.completed\",\"b64_json\":\"QUJDRA==\",\"usage\":{\"input_tokens\":12,\"output_tokens\":1056,\"total_tokens\":1068}}\n\n",
        "data: [DONE]\n\n",
    );
    let (upstream_url, upstream) =
        spawn_image_upstream_typed("text/event-stream", frames.as_bytes()).await;
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
            "stream": true,
            "partial_images": 2
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.unwrap(),
        frames,
        "最终事件带 usage，但它是图片本体，不能被当成 Chat 的 usage 收尾块丢掉"
    );

    let seen = upstream.seen.lock().unwrap();
    let request: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(request["model"], "image-model-upstream");
    assert_eq!(request["stream"], true);
}

/// 客户端要流式、上游按普通 JSON 回：上游给什么就转发什么。报协议错不仅
/// 用户拿不到已经生成好的图，多目标下还会换号重试——那是第二次真金白银的生图。
#[tokio::test]
async fn stream_request_falls_back_to_json_when_upstream_ignores_it() {
    let body = br#"{"created":1,"data":[{"url":"https://upstream.example/x.png"}]}"#;
    let (upstream_url, upstream) = spawn_image_upstream_typed("application/json", body).await;
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
        .json(&json!({"model": "image-model", "prompt": "猫", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), body);

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "形状不符不是上游故障，不许换号重试");
    let request: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(request["model"], "image-model-upstream");
    assert!(
        request.get("stream_options").is_none(),
        "图片接口没有 stream_options，网关不能为了自己的 usage 硬塞进去"
    );
}

/// 请求没声明流式、上游却回了 SSE：按 SSE 原样转发，不去按 JSON 解析。
#[tokio::test]
async fn upstream_sse_wins_when_the_client_did_not_ask_for_streaming() {
    let frames = "event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\"b64_json\":\"QUJD\"}\n\ndata: [DONE]\n\n";
    let (upstream_url, upstream) =
        spawn_image_upstream_typed("text/event-stream", frames.as_bytes()).await;
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
        .json(&json!({"model": "image-model", "prompt": "猫"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.text().await.unwrap(), frames);

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let request: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert!(request.get("stream_options").is_none());
}

/// edits 也接受 JSON 正文（内联 base64 图片），与 multipart 一样原样转发。
#[tokio::test]
async fn edits_accept_json_bodies_with_inline_images() {
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

    let inline = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==";
    let response = client()
        .post(format!("{}/v1/images/edits", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "edit-model",
            "prompt": "把背景换成白色",
            "image": inline
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen[0].path, "/v1/images/edits");
    let request: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(request["model"], "edit-model-upstream");
    assert_eq!(request["image"], inline, "内联图片必须逐字节透传");
}

/// 图片字节不是 Token：一次 4 MB 的图生图不能被算成上百万 token，
/// 否则任何配了 TPM 的分组都会把它判成超限，而且永远等不到窗口释放。
#[tokio::test]
async fn image_bytes_do_not_consume_the_tpm_budget() {
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
        )
        .limits(Limits {
            rpm: None,
            tpm: Some(2_000),
            max_concurrency: None,
        }),
    )
    .await;

    let boundary = "big-image";
    let mut body = Vec::new();
    push_multipart_field(&mut body, boundary, "model", "edit-model");
    push_multipart_field(&mut body, boundary, "prompt", "把背景换成白色");
    push_multipart_file(
        &mut body,
        boundary,
        "image",
        "big.png",
        &vec![9_u8; 4 * 1024 * 1024],
    );
    finish_multipart(&mut body, boundary);

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
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "提示词只有几十字节的图生图不该被 TPM 拦下：{body}"
    );
    assert_eq!(upstream.seen.lock().unwrap().len(), 1);
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

/// 异步下单 + 轮询：任务 ID 是上游签发的，轮询必须回到接单的那个账号，
/// 并用当初那把 Key（§14.9、§4.2.1 的不变量 A）。
#[tokio::test]
async fn async_image_tasks_are_polled_back_to_the_accepting_account() {
    let (upstream_url, upstream) = spawn_async_image_upstream().await;
    let akhub = spawn_akhub().await;
    // 账号名用 ASCII：假上游按 "key-<账号名>" 造凭据，中文名会让 Bearer 值
    // 落到 HeaderValue 的非可见 ASCII 之外，测的就不是网关了。
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "account-a",
            &upstream_url,
            Protocol::OpenAiChat,
            "image-model",
            "image-model-upstream",
            50,
        ),
    )
    .await;

    // 下单：202 与任务对象原样透传。
    let response = client()
        .post(format!("{}/v1/images/generations/async", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "image-model", "prompt": "一只猫"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let submitted: Value = response.json().await.unwrap();
    assert_eq!(submitted["id"], "task_up_1");
    assert_eq!(submitted["status"], "processing");

    // 轮询：同一个上游账号、同一个任务路径，Retry-After 也照传。
    let response = client()
        .get(format!("{}/v1/images/tasks/task_up_1", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "3");
    // 同一个 URL 的答案随时间变化，缓存住就会让客户端永远看到"处理中"。
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store"
    );
    let task: Value = response.json().await.unwrap();
    assert_eq!(task["id"], "task_up_1");

    // 轮询同样留一条请求记录，并带上接单账号（否则现场只剩客户端说"我一直 404"）。
    // 这段要 await，所以放在取上游锁之前：锁不跨 await 是硬规矩。
    let record = wait_for_task_record(&akhub).await;
    assert_eq!(record.http_status, 200);
    assert_eq!(
        record.account_id.as_deref(),
        Some(wired.account_id.as_str())
    );
    assert_eq!(record.error_code, None);

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/images/generations/async");
    let request: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(request["model"], "image-model-upstream");
    assert_eq!(seen[1].method, "GET");
    assert_eq!(seen[1].path, "/v1/images/tasks/task_up_1");
    // 判据是"与下单那一把相同"，而不是"等于某把固定 Key"：多 Key 账号上池子
    // 选哪把都行，但轮询必须跟着走（§4.2.1 的不变量 A）。
    let submitted_with = api_key_of(&seen[0].headers);
    let polled_with = api_key_of(&seen[1].headers);
    assert_eq!(submitted_with.as_deref(), Some(wired.api_key.as_str()));
    assert_eq!(polled_with, submitted_with, "轮询必须用接单时那把 Key");
}

/// 异步改图的 multipart 形状：与同步 edits 一样逐字节透传，并按同一套规则
/// 登记任务归属（否则下单能过、轮询永远 404）。
#[tokio::test]
async fn async_edits_accept_multipart_and_stay_pollable() {
    let (upstream_url, upstream) = spawn_async_image_upstream().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "account-a",
            &upstream_url,
            Protocol::OpenAiChat,
            "edit-model",
            "edit-model-upstream",
            50,
        ),
    )
    .await;

    let boundary = "async-edit-boundary";
    let mut body = Vec::new();
    push_multipart_field(&mut body, boundary, "model", "edit-model");
    push_multipart_field(&mut body, boundary, "prompt", "换成白底");
    push_multipart_file(&mut body, boundary, "image", "in.png", &[1_u8, 2, 3]);
    finish_multipart(&mut body, boundary);

    let response = client()
        .post(format!("{}/v1/images/edits/async", akhub.base_url))
        .bearer_auth(&akhub.key)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let submitted: Value = response.json().await.unwrap();
    assert_eq!(submitted["id"], "task_up_1");

    // 能轮询到，说明 multipart 下单同样登记了归属。
    let response = client()
        .get(format!("{}/v1/images/tasks/task_up_1", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/images/edits/async");
    let content_type = seen[0]
        .headers
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let parts = parse_multipart_parts(&seen[0].body, &content_type);
    assert_eq!(
        parts[0],
        ("model".to_string(), b"edit-model-upstream".to_vec())
    );
    assert_eq!(
        parts[1],
        ("prompt".to_string(), "换成白底".as_bytes().to_vec())
    );
    assert_eq!(parts[2].0, "image");
    assert_eq!(parts[2].1, vec![1_u8, 2, 3]);
}

/// 查不到的任务：明确 404，且一个字节都不打上游。
#[tokio::test]
async fn unknown_image_task_is_404_without_touching_upstream() {
    let (upstream_url, upstream) = spawn_async_image_upstream().await;
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
        .get(format!("{}/v1/images/tasks/never-existed", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "image_task_not_found");
    assert!(upstream.seen.lock().unwrap().is_empty());

    // 查不到也要留下记录：这时还没有目标，账号与目标必须留空，而不是编造。
    let record = wait_for_task_record(&akhub).await;
    assert_eq!(record.http_status, 404);
    assert_eq!(record.error_code.as_deref(), Some("image_task_not_found"));
    assert!(record.account_id.is_none());
    assert!(record.target_id.is_none());
}

/// 定位记录里那把 Key 已经不在了（老记录没存摘要、或那把 Key 刚被删）：
/// 退回账号第一把 Key 试一次，而不是把整个任务判死。
#[tokio::test]
async fn poll_falls_back_when_the_recorded_key_is_gone() {
    let (upstream_url, upstream) = spawn_async_image_upstream().await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "account-a",
            &upstream_url,
            Protocol::OpenAiChat,
            "image-model",
            "image-model-upstream",
            50,
        ),
    )
    .await;

    let now = akhub::storage::now_unix();
    akhub
        .state
        .store
        .upsert_image_task(&akhub::storage::store::ImageTaskRow {
            task_id: "task_up_1".into(),
            group_id: akhub.group_id.clone(),
            account_id: wired.account_id.clone(),
            target_id: None,
            upstream_model: None,
            key_digest: Some("digest-that-no-longer-exists".into()),
            created_at: now,
            expires_at: now + 600,
        })
        .await
        .unwrap();

    let response = client()
        .get(format!("{}/v1/images/tasks/task_up_1", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        api_key_of(&seen[0].headers).as_deref(),
        Some(wired.api_key.as_str()),
        "摘要对不上时退回账号第一把 Key"
    );
}

/// 任务 ID 不是跨组探测工具：别的分组的任务一律当作不存在（§26.8）。
#[tokio::test]
async fn image_tasks_do_not_cross_groups() {
    let (upstream_url, upstream) = spawn_async_image_upstream().await;
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

    let now = akhub::storage::now_unix();
    akhub
        .state
        .store
        .upsert_image_task(&akhub::storage::store::ImageTaskRow {
            task_id: "other-group-task".into(),
            group_id: "grp_someone_else".into(),
            account_id: "acc_someone_else".into(),
            target_id: None,
            upstream_model: None,
            key_digest: None,
            created_at: now,
            expires_at: now + 600,
        })
        .await
        .unwrap();

    let response = client()
        .get(format!(
            "{}/v1/images/tasks/other-group-task",
            akhub.base_url
        ))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "image_task_not_found");
    assert!(upstream.seen.lock().unwrap().is_empty());
}

/// 过期的定位记录在**读取时**就当作不存在（§14.9）。
///
/// 后台清理是 600 秒一轮，只靠它会让记录在到期后多存活最多 10 分钟；文档承诺
/// 的是"过期就按任务不存在处理"。这条用例把 expires_at 放到过去，验证惰性判定。
#[tokio::test]
async fn an_expired_image_task_is_404_without_touching_upstream() {
    let (upstream_url, upstream) = spawn_async_image_upstream().await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "account-a",
            &upstream_url,
            Protocol::OpenAiChat,
            "image-model",
            "image-model-upstream",
            50,
        ),
    )
    .await;

    let now = akhub::storage::now_unix();
    akhub
        .state
        .store
        .upsert_image_task(&akhub::storage::store::ImageTaskRow {
            task_id: "expired-task".into(),
            group_id: akhub.group_id.clone(),
            account_id: wired.account_id.clone(),
            target_id: None,
            upstream_model: None,
            key_digest: None,
            created_at: now - 7200,
            expires_at: now - 60,
        })
        .await
        .unwrap();

    let response = client()
        .get(format!("{}/v1/images/tasks/expired-task", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "image_task_not_found");
    assert!(upstream.seen.lock().unwrap().is_empty(), "过期任务不打上游");
}

/// 管理员停用一把 Key 之后，此前由它接单的任务不能继续拿它发请求（§4.2.1）。
///
/// 轮询是**直连**路径，绕过了调度器的 select_key——那把 Key 的 enabled 过滤
/// 必须在这里自己做，否则停用一把泄露的 Key 起不到任何作用。
#[tokio::test]
async fn polling_refuses_a_disabled_key_and_falls_back_to_an_enabled_one() {
    let (upstream_url, upstream) = spawn_async_image_upstream().await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "account-a",
            &upstream_url,
            Protocol::OpenAiChat,
            "image-model",
            "image-model-upstream",
            50,
        ),
    )
    .await;

    // 定位记录指向这把 Key，然后把它停用。
    let now = akhub::storage::now_unix();
    akhub
        .state
        .store
        .upsert_image_task(&akhub::storage::store::ImageTaskRow {
            task_id: "task_up_1".into(),
            group_id: akhub.group_id.clone(),
            account_id: wired.account_id.clone(),
            target_id: None,
            upstream_model: None,
            key_digest: Some(wired.credential_digest.clone()),
            created_at: now,
            expires_at: now + 600,
        })
        .await
        .unwrap();
    let keys = akhub
        .state
        .store
        .list_account_key_rows(&wired.account_id)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    // 原样重写同一把 Key，只把 enabled 关掉（摘要不变，定位记录仍指向它）。
    let stored = keys[0].clone();
    akhub
        .state
        .store
        .replace_account_keys(
            &wired.account_id,
            &[akhub::storage::store::AccountKeyWrite {
                id: stored.id.clone(),
                label: stored.label.clone(),
                sealed_key: stored.sealed_key.clone(),
                credential_digest: wired.credential_digest.clone(),
                limits: stored.limits,
                enabled: false,
            }],
        )
        .await
        .unwrap();
    akhub.state.reload_credentials().await.unwrap();

    // 账号只剩一把停用的 Key：轮询必须明确失败，绝不能拿它发出去。
    let response = client()
        .get(format!("{}/v1/images/tasks/task_up_1", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    assert!(
        upstream.seen.lock().unwrap().is_empty(),
        "停用的 Key 绝不能被用来发轮询请求"
    );
}

/// 同一个上游任务 ID 在两个分组下必须各存各的：后下单的分组不能覆盖先方
/// 的定位行（§14.9、§23.4）。
///
/// 上游任务 ID 只在**上游站点**内唯一，而多个分组完全可能指向同一个站点。
/// 按单列 task_id 做主键时，后写会 REPLACE 掉先方那一行，先方轮询永久 404，
/// 而任务还在上游跑——客户端多半会重新下单，真金白银重复扣费。
#[tokio::test]
async fn the_same_task_id_can_belong_to_two_groups_without_overwriting() {
    let (upstream_url, upstream) = spawn_async_image_upstream().await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "account-a",
            &upstream_url,
            Protocol::OpenAiChat,
            "image-model",
            "image-model-upstream",
            50,
        ),
    )
    .await;

    let now = akhub::storage::now_unix();
    // 本组的定位行。
    akhub
        .state
        .store
        .upsert_image_task(&akhub::storage::store::ImageTaskRow {
            task_id: "shared-task".into(),
            group_id: akhub.group_id.clone(),
            account_id: wired.account_id.clone(),
            target_id: None,
            upstream_model: None,
            key_digest: None,
            created_at: now,
            expires_at: now + 600,
        })
        .await
        .unwrap();
    // 另一个分组在同一个上游站点上拿到了同一个任务 ID：这一行不该顶掉上面那行。
    akhub
        .state
        .store
        .upsert_image_task(&akhub::storage::store::ImageTaskRow {
            task_id: "shared-task".into(),
            group_id: "grp_someone_else".into(),
            account_id: "acc_someone_else".into(),
            target_id: None,
            upstream_model: None,
            key_digest: None,
            created_at: now,
            expires_at: now + 600,
        })
        .await
        .unwrap();

    // 本组的轮询必须仍然命中本组那一行、正常打到上游。
    let response = client()
        .get(format!("{}/v1/images/tasks/shared-task", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        api_key_of(&seen[0].headers).as_deref(),
        Some(wired.api_key.as_str()),
        "轮询必须回到本组接单的那个账号"
    );
}
