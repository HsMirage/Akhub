//! 端到端验收：三个入口通过真实 HTTP 打到一个假上游（§27 阶段 1 验收）。
//!
//! 这些测试不 mock 网关内部，只 mock 上游站点：请求经过完整的鉴权、配置快照、
//! 资格过滤、模型改写、请求头构造和响应转发路径。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use akhub::app::{AppState, Settings, SharedState};
use akhub::domain::{
    Account, DispatchTarget, Group, Limits, LogicalModel, ModelOrigin, Multiplier, MultiplierMode,
    Protocol, SchedulingWeights, UpstreamType,
};
use akhub::storage::store::{AccountSecrets, ids};
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use time::OffsetDateTime;

/// 假上游收到的一次请求。
#[derive(Debug, Clone)]
struct Seen {
    body: Value,
    headers: HeaderMap,
}

#[derive(Clone)]
struct Upstream {
    seen: Arc<std::sync::Mutex<Vec<Seen>>>,
    /// 前 N 次请求返回 500，用来验证故障切换。
    fail_first: Arc<AtomicUsize>,
}

async fn upstream_handler(
    State(upstream): State<Upstream>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    upstream.seen.lock().unwrap().push(Seen {
        body: parsed.clone(),
        headers,
    });

    if upstream.fail_first.load(Ordering::SeqCst) > 0 {
        upstream.fail_first.fetch_sub(1, Ordering::SeqCst);
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream boom",
        )
            .into_response();
    }

    if parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let sse = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                   event: content_block_delta\ndata: {\"delta\":{\"text\":\"你好\"}}\n\n\
                   data: [DONE]\n\n";
        return ([("content-type", "text/event-stream")], sse).into_response();
    }

    axum::Json(json!({
        "id": "msg_upstream",
        "model": parsed.get("model").cloned().unwrap_or(Value::Null),
        "content": [{"type": "text", "text": "你好"}],
        // 上游上报的用量：成本页与请求记录都依赖它（§6.6、§6.8）。
        "usage": {"input_tokens": 10, "output_tokens": 2},
    }))
    .into_response()
}

/// 启动假上游，返回它的地址与观察句柄。
async fn spawn_upstream(fail_first: usize) -> (String, Upstream) {
    let upstream = Upstream {
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        fail_first: Arc::new(AtomicUsize::new(fail_first)),
    };
    let app = Router::new()
        .route("/v1/messages", post(upstream_handler))
        .route("/v1/messages/count_tokens", post(upstream_handler))
        .route("/v1/chat/completions", post(upstream_handler))
        .route("/v1/responses", post(upstream_handler))
        .with_state(upstream.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), upstream)
}

/// 启动一台完整的 Akhub，返回它的地址、下游 Key 与共享状态。
async fn spawn_akhub() -> (String, String, SharedState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap(dir.path(), Settings::default())
        .await
        .unwrap();

    let (key, prefix) = akhub::security::generate_group_key().unwrap();
    let group = Group {
        id: ids::group(),
        name: "主力".into(),
        key_prefix: prefix,
        key_digest_hex: state.key_digest.digest_hex(&key),
        multiplier_limit: Multiplier::ONE,
        weights: SchedulingWeights::default(),
        queue_capacity: 100,
        max_wait_secs: 60,
        allow_managed_background: false,
        allow_degrade: true,
        created_at: OffsetDateTime::now_utc(),
    };
    state.store.insert_group(&group).await.unwrap();
    state.config.reload().await.unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let router = akhub::server::router(Arc::clone(&state));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    (format!("http://{addr}"), key.to_string(), state, dir)
}

/// 在指定分组下建一个账号，并把它接到一个逻辑模型上。
#[allow(clippy::too_many_arguments)]
async fn wire_target(
    state: &SharedState,
    name: &str,
    base_url: &str,
    protocol: Protocol,
    logical_model: &str,
    upstream_model: &str,
    priority: i32,
) {
    let group_id = state.store.list_groups().await.unwrap()[0].id.clone();
    let account = Account {
        id: ids::account(),
        group_id: group_id.clone(),
        name: name.into(),
        upstream_type: UpstreamType::OpenAiCompatible,
        base_url: base_url.into(),
        preferred_protocol: protocol,
        adaptive_protocol: true,
        default_priority: priority,
        calibration: Multiplier::ONE,
        multiplier_mode: MultiplierMode::Manual,
        manual_multiplier: Multiplier::ONE,
        new_api_user_id: None,
        new_api_group: None,
        limits: Limits::default(),
        // 假上游监听在 127.0.0.1，必须显式开启内网访问才能通过 SSRF 检查。
        allow_private_network: true,
        enabled: true,
        hide_original: false,
        auto_sync: false,
        model_synced_at: None,
        created_at: OffsetDateTime::now_utc(),
    };
    let sealed = state.cipher.seal(b"upstream-secret-key").unwrap();
    state
        .store
        .insert_account(&account, &AccountSecrets::new(sealed, None))
        .await
        .unwrap();

    let existing = state
        .store
        .list_logical_models()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.name == logical_model);
    let model_id = match existing {
        Some(model) => model.id,
        None => {
            let model = LogicalModel {
                id: ids::logical_model(),
                group_id,
                name: logical_model.into(),
                origin: ModelOrigin::Manual,
                enabled: true,
                created_at: OffsetDateTime::now_utc(),
            };
            state.store.insert_logical_model(&model).await.unwrap();
            model.id
        }
    };

    state
        .store
        .insert_target(&DispatchTarget {
            id: ids::target(),
            logical_model_id: model_id,
            account_id: account.id,
            upstream_model: upstream_model.into(),
            hide_original: false,
            priority_override: None,
            limits: Limits::default(),
            enabled: true,
            created_at: OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();
    // 走与后台完全相同的重载路径，让倍率表与动态状态一并同步。
    state.reload_config().await.unwrap();
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().unwrap()
}

#[tokio::test]
async fn anthropic_messages_pass_through_with_the_model_rewritten() {
    let (upstream_url, upstream) = spawn_upstream(0).await;
    let (akhub, key, state, _dir) = spawn_akhub().await;
    wire_target(
        &state,
        "账号A",
        &upstream_url,
        Protocol::AnthropicMessages,
        "claude-sonnet-4-5",
        "claude-sonnet-4-5-20250929",
        50,
    )
    .await;

    let response = client()
        .post(format!("{akhub}/v1/messages"))
        .header("x-api-key", &key)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}],
            "供应商扩展字段": {"保留": true}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key("x-akhub-request-id"));

    let seen = upstream.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    // 下游只用逻辑模型名，上游只看到真实模型名（§2.1）。
    assert_eq!(seen[0].body["model"], "claude-sonnet-4-5-20250929");
    // 未知字段在同协议路径原样保留（§14.1）。
    assert_eq!(seen[0].body["供应商扩展字段"]["保留"], true);
    // 上游拿到的是账号自己的凭据，绝不是下游 Key（§14.7）。
    assert_eq!(seen[0].headers["x-api-key"], "upstream-secret-key");
    assert_ne!(seen[0].headers["x-api-key"], key.as_str());
    assert_eq!(seen[0].headers["anthropic-version"], "2023-06-01");
}

#[tokio::test]
async fn openai_chat_streams_server_sent_events_through() {
    let (upstream_url, _upstream) = spawn_upstream(0).await;
    let (akhub, key, state, _dir) = spawn_akhub().await;
    wire_target(
        &state,
        "账号A",
        &upstream_url,
        Protocol::OpenAiChat,
        "glm-4.6",
        "glm-4.6-bf16",
        50,
    )
    .await;

    let response = client()
        .post(format!("{akhub}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model": "glm-4.6", "stream": true, "messages": []}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = response.text().await.unwrap();
    assert!(body.contains("content_block_delta"));
    assert!(body.trim_end().ends_with("data: [DONE]"));
}

#[tokio::test]
async fn count_tokens_reaches_the_anthropic_endpoint() {
    let (upstream_url, upstream) = spawn_upstream(0).await;
    let (akhub, key, state, _dir) = spawn_akhub().await;
    wire_target(
        &state,
        "账号A",
        &upstream_url,
        Protocol::AnthropicMessages,
        "claude-sonnet-4-5",
        "claude-sonnet-4-5-20250929",
        50,
    )
    .await;

    let response = client()
        .post(format!("{akhub}/v1/messages/count_tokens"))
        .header("x-api-key", &key)
        .json(
            &json!({"model": "claude-sonnet-4-5", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200, "Claude Code 依赖这个端点（§15.5）");
    assert_eq!(
        upstream.seen.lock().unwrap()[0].body["model"],
        "claude-sonnet-4-5-20250929"
    );
}

#[tokio::test]
async fn a_failing_target_falls_over_to_the_next_one() {
    // 第一个上游连续失败两次，第二个正常。
    let (broken_url, broken) = spawn_upstream(10).await;
    let (healthy_url, healthy) = spawn_upstream(0).await;
    let (akhub, key, state, _dir) = spawn_akhub().await;

    wire_target(
        &state,
        "坏账号",
        &broken_url,
        Protocol::OpenAiChat,
        "glm-4.6",
        "glm-4.6",
        100,
    )
    .await;
    wire_target(
        &state,
        "好账号",
        &healthy_url,
        Protocol::OpenAiChat,
        "glm-4.6",
        "glm-4.6",
        50,
    )
    .await;

    let response = client()
        .post(format!("{akhub}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model": "glm-4.6", "messages": []}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    // 严格阶梯：先试高优先级的坏账号，失败后才降到低优先级的好账号（§9.2）。
    assert_eq!(
        broken.seen.lock().unwrap().len(),
        1,
        "高优先级目标必须被先尝试"
    );
    assert_eq!(healthy.seen.lock().unwrap().len(), 1);

    // 尝试明细要能解释"换了几个号、各自为什么失败"（§6.6）。
    let record = wait_for_record(&state).await;
    assert_eq!(record.attempts_detail.len(), 2, "两次尝试都要留下明细");
    assert_eq!(record.attempts_detail[0].outcome, "failed");
    assert_eq!(
        record.attempts_detail[0].error_code.as_deref(),
        Some("upstream_exhausted")
    );
    assert!(
        record.attempts_detail[0].counts_against_budget,
        "上游 500 要计入尝试预算"
    );
    assert_eq!(record.attempts_detail[1].outcome, "ok");
}

/// 等后台批量落盘，取最近一条请求记录。
async fn wait_for_record(state: &SharedState) -> akhub::storage::store::RequestRecord {
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let records = state.store.list_request_records(10, 0).await.unwrap();
        if !records.is_empty() {
            return records[0].clone();
        }
    }
    panic!("请求元数据未落盘");
}

#[tokio::test]
async fn all_targets_failing_yields_a_retryable_status() {
    let (broken_url, _broken) = spawn_upstream(10).await;
    let (akhub, key, state, _dir) = spawn_akhub().await;
    wire_target(
        &state,
        "坏账号",
        &broken_url,
        Protocol::OpenAiChat,
        "glm-4.6",
        "glm-4.6",
        50,
    )
    .await;

    let response = client()
        .post(format!("{akhub}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model": "glm-4.6", "messages": []}))
        .send()
        .await
        .unwrap();

    // upstream_exhausted → 503，且带 Retry-After，客户端才会退避重试（§18.3）。
    assert_eq!(response.status(), 503);
    assert!(response.headers().contains_key("retry-after"));
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream_exhausted");
}

#[tokio::test]
async fn models_endpoint_switches_shape_by_authentication_header() {
    let (upstream_url, _upstream) = spawn_upstream(0).await;
    let (akhub, key, state, _dir) = spawn_akhub().await;
    wire_target(
        &state,
        "账号A",
        &upstream_url,
        Protocol::AnthropicMessages,
        "claude-sonnet-4-5",
        "claude-sonnet-4-5-20250929",
        50,
    )
    .await;

    // 默认不隐藏原始模型：对外名与上游真名都会出现在列表里。
    let anthropic: Value = client()
        .get(format!("{akhub}/v1/models"))
        .header("x-api-key", &key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(anthropic["data"][0]["type"], "model");
    assert_eq!(anthropic["data"][0]["display_name"], "claude-sonnet-4-5");
    assert_eq!(
        anthropic["data"][1]["display_name"],
        "claude-sonnet-4-5-20250929"
    );
    assert_eq!(anthropic["has_more"], false);

    let openai: Value = client()
        .get(format!("{akhub}/v1/models"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(openai["object"], "list");
    assert_eq!(openai["data"][0]["id"], "claude-sonnet-4-5");
    assert_eq!(openai["data"][1]["id"], "claude-sonnet-4-5-20250929");
    assert_eq!(openai["data"][0]["owned_by"], "akhub");

    // 两个名字都指向同一个逻辑模型，只是列表里的两个入口。
    for body in [&anthropic, &openai] {
        let names: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert_eq!(
            names,
            vec!["claude-sonnet-4-5", "claude-sonnet-4-5-20250929"]
        );
    }

    // 用上游真名（别名）发请求也必须落在同一个逻辑模型上。
    let response = client()
        .post(format!("{akhub}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({
            "model": "claude-sonnet-4-5-20250929",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
}

#[tokio::test]
async fn invalid_credentials_are_rejected_before_any_upstream_call() {
    let (upstream_url, upstream) = spawn_upstream(0).await;
    let (akhub, _key, state, _dir) = spawn_akhub().await;
    wire_target(
        &state,
        "账号A",
        &upstream_url,
        Protocol::OpenAiChat,
        "glm-4.6",
        "glm-4.6",
        50,
    )
    .await;

    let response = client()
        .post(format!("{akhub}/v1/chat/completions"))
        .bearer_auth("akh-伪造的Key")
        .json(&json!({"model": "glm-4.6", "messages": []}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);
    assert!(
        upstream.seen.lock().unwrap().is_empty(),
        "鉴权失败绝不能碰到上游"
    );

    // 两个鉴权头同时存在且不一致时不猜测（§7.2）。
    let conflicting = client()
        .post(format!("{akhub}/v1/chat/completions"))
        .bearer_auth("akh-a")
        .header("x-api-key", "akh-b")
        .json(&json!({"model": "glm-4.6", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(conflicting.status(), 401);
}

#[tokio::test]
async fn unknown_models_are_not_searched_across_groups() {
    let (upstream_url, _upstream) = spawn_upstream(0).await;
    let (akhub, key, state, _dir) = spawn_akhub().await;
    wire_target(
        &state,
        "账号A",
        &upstream_url,
        Protocol::OpenAiChat,
        "glm-4.6",
        "glm-4.6",
        50,
    )
    .await;

    let response = client()
        .post(format!("{akhub}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model": "不存在的模型", "messages": []}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "model_not_found");
}

#[tokio::test]
async fn request_metadata_is_recorded_without_any_body() {
    let (upstream_url, _upstream) = spawn_upstream(0).await;
    let (akhub, key, state, _dir) = spawn_akhub().await;
    wire_target(
        &state,
        "账号A",
        &upstream_url,
        Protocol::OpenAiChat,
        "glm-4.6",
        "glm-4.6-bf16",
        50,
    )
    .await;

    client()
        .post(format!("{akhub}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model": "glm-4.6", "messages": [{"role": "user", "content": "机密内容"}]}))
        .send()
        .await
        .unwrap();

    // 元数据由后台任务批量落盘，给它一点时间。
    let mut records = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        records = state.store.list_request_records(10, 0).await.unwrap();
        if !records.is_empty() {
            break;
        }
    }
    assert_eq!(records.len(), 1, "请求元数据未落盘");

    let record = &records[0];
    assert_eq!(record.logical_model.as_deref(), Some("glm-4.6"));
    assert_eq!(record.upstream_model.as_deref(), Some("glm-4.6-bf16"));
    assert_eq!(record.http_status, 200);
    assert!(
        record.request_bytes > 0,
        "请求体字节数是粘性等待预算的锚点（§10.3）"
    );
    // 上游回的 usage 必须进元数据：成本页与请求记录都靠它（§6.6、§6.8）。
    assert_eq!(record.input_tokens, Some(10), "输入 token 未记录");
    assert_eq!(record.output_tokens, Some(2), "输出 token 未记录");
    assert_eq!(record.attempts_detail.len(), 1, "尝试明细未记录");
    assert_eq!(record.attempts_detail[0].outcome, "ok");
    assert!(record.config_version.is_some(), "配置版本未记录");
    assert!(
        !format!("{record:?}").contains("机密内容"),
        "正文绝不能进入元数据"
    );
}

#[tokio::test]
async fn health_endpoints_expose_nothing_sensitive() {
    let (akhub, _key, _state, _dir) = spawn_akhub().await;

    let live = client()
        .get(format!("{akhub}/health/live"))
        .send()
        .await
        .unwrap();
    assert_eq!(live.status(), 200);

    let ready = client()
        .get(format!("{akhub}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
    let body = ready.text().await.unwrap();
    assert!(!body.contains("akh-"), "健康接口不得暴露任何配置细节");
}
