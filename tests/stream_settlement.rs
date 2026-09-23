//! 流式结算与 Responses 流式状态链验收（§15.2、§26.3）。
//!
//! 这组用例专门覆盖"HTTP 头已经发出、但流还没结束"的时段：
//!
//! - TPM 必须按流里真实上报的 usage 回补，而不是把预留占满整个窗口；
//! - 上游没上报 usage 时必须保持保守预留（宁可少发也不超限），不估算；
//! - Responses 流结束后必须把输出项补进状态链，跨上游重建才无损；
//! - 跨协议进入 Responses 时，客户端引用的一定是网关 ID。

mod common;

use akhub::domain::{Limits, Protocol};
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::post;
use common::{
    Behavior, FakeUpstream, TargetSpec, client, spawn_akhub, spawn_akhub_with, wire_target,
};
use serde_json::{Value, json};

/// 假上游返回的流式脚本。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    /// Chat 流，收尾块带 usage（正常 `include_usage` 上游）。
    ChatWithUsage,
    /// Chat 流，完全不提 usage（客户端没要、上游也没给）。
    ChatWithoutUsage,
    /// Responses 流：开始标记 → 输出项 → 带 output 与 usage 的 completed。
    Responses,
    /// 非流式 Responses：完整响应体，usage 带 Responses 形状的 details。
    ResponsesNonStream,
}

fn chat_stream(with_usage: bool) -> String {
    let mut frames = vec![
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string(),
    ];
    if with_usage {
        frames.push(
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":10,\"total_tokens\":12}}\n\n"
                .to_string(),
        );
    }
    frames.push("data: [DONE]\n\n".to_string());
    frames.concat()
}

fn responses_stream() -> String {
    [
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_up\",\"status\":\"in_progress\"}}\n\n",
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"你好\"}]}}\n\n",
        // usage 按真实 Responses 形状带 details：缓存读在 `input_tokens_details`、
        // 思考 Token 在 `output_tokens_details`——都不是 Chat 的那两个父字段。
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_up\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"你好\"}]}],\"usage\":{\"input_tokens\":1000,\"output_tokens\":50,\"total_tokens\":1050,\"input_tokens_details\":{\"cached_tokens\":768},\"output_tokens_details\":{\"reasoning_tokens\":32}}}}\n\n",
    ]
    .concat()
}

async fn spawn_upstream(script: Script) -> String {
    async fn handler(
        axum::extract::State(script): axum::extract::State<Script>,
        body: String,
    ) -> axum::response::Response {
        let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if request
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let payload = match script {
                Script::ChatWithUsage => chat_stream(true),
                Script::ChatWithoutUsage => chat_stream(false),
                // 非流式那个变体走不到这条分支（请求体里没有 stream）。
                Script::Responses | Script::ResponsesNonStream => responses_stream(),
            };
            return ([("content-type", "text/event-stream")], payload).into_response();
        }
        match script {
            Script::ResponsesNonStream => axum::Json(json!({
                "id": "resp_up",
                "status": "completed",
                "output": [{"type": "message", "role": "assistant",
                            "content": [{"type": "output_text", "text": "你好"}]}],
                "usage": {
                    "input_tokens": 1000,
                    "output_tokens": 50,
                    "total_tokens": 1050,
                    "input_tokens_details": {"cached_tokens": 768},
                    "output_tokens_details": {"reasoning_tokens": 32}
                }
            }))
            .into_response(),
            _ => axum::Json(json!({"id": "resp_up", "output": [], "usage": {}})).into_response(),
        }
    }

    let app = Router::new()
        .route("/v1/chat/completions", post(handler))
        .route("/v1/responses", post(handler))
        .with_state(script);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn chat_request() -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "stream": true,
        "max_tokens": 400,
        "messages": [{"role": "user", "content": "你好"}],
    })
}

/// 一次带 usage 的 Chat 流结束后，TPM 必须按真实用量回补。
///
/// 预估预留约 `请求字节/4 + max_tokens(400)`，上限 500；上游实际只用了
/// 12。若结算发生在首段提交（旧行为），第二个请求会因窗口仍被预留占满
/// 而被限流；按流结束的真实 usage 回补后，第二个请求必须放行。
#[tokio::test]
async fn a_chat_stream_refunds_the_tpm_reservation_with_reported_usage() {
    let upstream = spawn_upstream(Script::ChatWithUsage).await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream,
            Protocol::OpenAiChat,
            "claude-sonnet-4-5",
            "claude-sonnet-4-5",
            50,
        )
        .limits(Limits {
            tpm: Some(500),
            ..Limits::default()
        }),
    )
    .await;

    for round in 0..2 {
        let response = client()
            .post(format!("{}/v1/chat/completions", akhub.base_url))
            .bearer_auth(&akhub.key)
            .json(&chat_request())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "第 {} 次请求应被放行", round + 1);
        let text = response.text().await.unwrap();
        assert!(text.contains("你好"), "{text}");
    }
}

/// 上游没有上报 usage 时，预留必须保持到窗口过期（保守方向）。
///
/// 用一个很短的请求总超时把"继续等"变成"等不到"：第二个请求只能被限流
/// 拒绝（429），而不是被放行——这正是"预留没有被退还"的证据。
#[tokio::test]
async fn a_stream_without_usage_keeps_the_conservative_reservation() {
    let upstream = spawn_upstream(Script::ChatWithoutUsage).await;
    let akhub = spawn_akhub_with(
        akhub::app::Settings {
            request_timeout: std::time::Duration::from_millis(500),
            ..akhub::app::Settings::default()
        },
        |_| {},
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream,
            Protocol::OpenAiChat,
            "claude-sonnet-4-5",
            "claude-sonnet-4-5",
            50,
        )
        .limits(Limits {
            tpm: Some(500),
            ..Limits::default()
        }),
    )
    .await;

    let first = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let _ = first.text().await.unwrap();

    let second = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        429,
        "没有 usage 就不能猜，预留必须继续占着直到窗口释放"
    );
}

/// 流式 Responses 结束后，输出项必须补进状态链（§15.2）。
#[tokio::test]
async fn a_streamed_responses_turn_stores_its_output_items() {
    let upstream = spawn_upstream(Script::Responses).await;
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

    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "stream": true, "input": "你好"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("response.completed"), "{text}");

    let gateway_id = text
        .lines()
        .find_map(|line| {
            let data = line.strip_prefix("data: ")?;
            let value: Value = serde_json::from_str(data).ok()?;
            let id = value
                .get("response")
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)?;
            id.starts_with("resp_akh_").then(|| id.to_string())
        })
        .expect("客户端必须看到网关 ID");
    assert!(!text.contains("resp_up"), "上游 ID 不得泄漏：{text}");

    // 状态补写在流结束后异步落库，轮询等待。
    let mut stored = None;
    for _ in 0..50 {
        let row = akhub
            .state
            .store
            .response_state(&gateway_id, &akhub.group_id)
            .await
            .unwrap();
        if let Some(row) = row
            && let Some(sealed) = row.sealed_body.as_ref()
            && let Ok(plaintext) = akhub.state.cipher.open(sealed)
            && let Ok(body) = serde_json::from_slice::<Value>(&plaintext)
        {
            let has_output = body
                .get("input")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.to_string().contains("你好")
                            && item.get("role").and_then(Value::as_str) == Some("assistant")
                    })
                });
            if has_output {
                stored = Some(body);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    let stored = stored.expect("流式输出项必须补进状态链");
    assert!(
        stored.to_string().contains("你好"),
        "保存的历史必须包含助手输出：{stored}"
    );
}

/// 跨协议进入 Responses 时，客户端引用的是网关 ID，状态链也要登记（§15.1）。
#[tokio::test]
async fn a_cross_protocol_stream_into_responses_uses_a_gateway_id() {
    let upstream = spawn_upstream(Script::ChatWithUsage).await;
    let akhub = spawn_akhub().await;
    // 账号只有 Chat 端点，客户端说的是 Responses：必须走跨协议转换。
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream,
            Protocol::OpenAiChat,
            "gpt-5",
            "claude-sonnet-4-5",
            50,
        )
        .pinned(),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "stream": true, "input": "你好"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("response.created"), "{text}");
    let gateway_id = text
        .lines()
        .find_map(|line| {
            let data = line.strip_prefix("data: ")?;
            let value: Value = serde_json::from_str(data).ok()?;
            let id = value
                .get("response")
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)?;
            id.starts_with("resp_akh_").then(|| id.to_string())
        })
        .expect("跨协议进入 Responses 也必须用网关 ID");

    // 状态行必须存在，且没有上游 ID（跨协议没有可复用的 Responses ID）。
    let mut found = None;
    for _ in 0..50 {
        found = akhub
            .state
            .store
            .response_state(&gateway_id, &akhub.group_id)
            .await
            .unwrap();
        if found.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    let row = found.expect("跨协议流也必须登记状态链");
    assert!(row.upstream_id.is_none(), "跨协议没有原生上游 ID");
}

/// 客户端在流中途断开：状态不是"未知"而是 `client_gone`（§18.1、§24.1）。
///
/// 这是线上真实故障的回归：Codex 侧掉线时 Akhub 记录成绿色的 200 成功、
/// 输入/输出与首字全是空，事后完全看不出这次为什么没有用量。
#[tokio::test]
async fn a_client_that_disconnects_mid_stream_is_recorded_as_client_gone() {
    // `StreamThenHang`：语义增量送出后挂住。只有上游还挂着的时候断开连接，
    // 才会走到 `Ending::Aborted`；瞬间跑完的流在断开前就结算成 `Completed` 了。
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::StreamThenHang);
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiResponses,
            "gpt-5",
            "gpt-5",
            50,
        ),
    )
    .await;

    // 上游吐完第一块语义内容就挂住；此时丢弃响应体 = 客户端掉线，
    // 生成器被 drop，结算走 `Ending::Aborted`。
    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "stream": true, "input": "你好"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    drop(response);

    let record = wait_for_record(&akhub, 1).await;
    assert_eq!(
        record.error_code.as_deref(),
        Some("client_gone"),
        "客户端断开必须留下可排查的稳定标识，而不是伪装成 200 成功：{record:?}"
    );
    assert_eq!(record.http_status, 200, "响应头确实已经发出去了");
    // 这个夹具送出了第一个语义事件，所以首字延迟是真实值；关键是不再凭空
    // 写一个 0（"未知"与"0 毫秒"在记录页是两种不同的结论）。
    assert!(record.first_token_ms.is_some(), "{record:?}");
    assert!(
        record.input_tokens.is_none() && record.output_tokens.is_none(),
        "上游没来得及上报用量：这一栏必须是空，而不是 0"
    );
}

/// 断开也不能污染目标的可靠性评分（§9.3、§12.3）。
///
/// 上游什么都没做错，却因为调用方掉线被扣掉两成成功率，要连着十次成功
/// 才爬得回来——那是拿一个健康账号给客户端的网络问题买单。
#[tokio::test]
async fn a_disconnect_does_not_lower_the_targets_reliability() {
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::StreamThenHang);
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiResponses,
            "gpt-5",
            "gpt-5",
            50,
        ),
    )
    .await;

    let dimension = akhub::routing::score::Dimension {
        protocol: Protocol::OpenAiResponses,
        streaming: true,
    };
    let before = akhub.state.runtime.perf.stats(&wired.target_id, dimension);
    assert_eq!(before.samples, 0, "还没有任何样本");

    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "stream": true, "input": "你好"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    drop(response);

    let record = wait_for_record(&akhub, 1).await;
    assert_eq!(record.error_code.as_deref(), Some("client_gone"));
    let after = akhub.state.runtime.perf.stats(&wired.target_id, dimension);
    assert_eq!(after.samples, 0, "客户端断开不该进性能样本");
    assert_eq!(after.success_rate, 1.0, "可靠性不得被断开拉低");
}

/// 端到端回归：流式 Responses 的缓存读与思考 Token 必须落进请求记录。
///
/// 现场故障：解析只认 Chat 的 `prompt_tokens_details`，于是整个 Responses
/// 协议族的 `cache_read_tokens` / `reasoning_tokens` 恒为空——生产库 2299 条
/// Responses 请求里只有 50 条有缓存读、思考 Token 是 0 条，而同期 Chat 的
/// 缓存读上报率是 60.9%。这条用例从真实入口打进去，断言的是**落库结果**，
/// 而不是解析函数的返回值。
#[tokio::test]
async fn a_responses_stream_records_cache_and_reasoning_tokens() {
    let upstream = spawn_upstream(Script::Responses).await;
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

    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "stream": true, "input": "你好"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.text().await.unwrap();

    let record = wait_for_record(&akhub, 1).await;
    assert_eq!(
        record.cache_read_tokens,
        Some(768),
        "Responses 流式的缓存读必须落进请求记录（§11.6）"
    );
    assert_eq!(
        record.reasoning_tokens,
        Some(32),
        "Responses 流式的思考 Token 必须落进请求记录（§11.6）"
    );
    assert_eq!(record.input_tokens, Some(1000));
    assert_eq!(record.output_tokens, Some(50));
}

/// 端到端回归：非流式 Responses 同样必须记下缓存读与思考 Token。
#[tokio::test]
async fn a_non_stream_responses_records_cache_and_reasoning_tokens() {
    let upstream = spawn_upstream(Script::ResponsesNonStream).await;
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

    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "你好"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.text().await.unwrap();

    let record = wait_for_record(&akhub, 1).await;
    assert_eq!(
        record.cache_read_tokens,
        Some(768),
        "非流式 Responses 的缓存读必须落进请求记录（§11.6）"
    );
    assert_eq!(
        record.reasoning_tokens,
        Some(32),
        "非流式 Responses 的思考 Token 必须落进请求记录（§11.6）"
    );
}

/// 等请求记录真正落库（写入是攒批的，最多 1 秒刷一次）。
async fn wait_for_record(
    akhub: &common::Akhub,
    wanted: usize,
) -> akhub::storage::store::RequestRecord {
    for _ in 0..100 {
        let records = akhub.state.store.list_request_records(5, 0).await.unwrap();
        if records.len() >= wanted {
            return records[0].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    panic!("请求记录没有在预期时间内落库");
}
