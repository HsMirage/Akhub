//! 流式结算与 Responses 流式状态链验收（§15.2、§26.3）。
//!
//! 这组用例专门覆盖"HTTP 头已经发出、但流还没结束"的时段：
//!
//! - TPM 必须按流里真实上报的 usage 回补，而不是把预留占满整个窗口；
//! - 上游没上报 usage 时必须保持保守预留（宁可少发也不超限），不估算；
//! - Responses 流结束后必须把输出项补进状态链，跨上游重建才无损；
//! - 跨协议进入 Responses 时，客户端引用的一定是网关 ID。

mod common;

use akhub::app::Settings;
use akhub::domain::{Limits, Protocol};
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::post;
use common::{
    Behavior, FakeUpstream, TargetSpec, client, spawn_akhub, spawn_akhub_with, wire_target,
};
use futures::StreamExt as _;
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

/// **现场回归**：上游把回答写完了、下游才断开，必须计入证据。
///
/// 这是 `deepseek-v4.1-flash` 的真实形态：绝大部分 `client_gone` 其实收到了
/// 终止信号（上游写完了），只是下游没等到收尾就关了连接。修复前这些观测被整条
/// 丢弃，五个目标的有效样本量全在 0.1~6.6 之间，没有任何目标能跨过
/// `MIN_SAMPLES`，评分永久退化成"近似均匀抽签"，于是每轮换一次渠道、前缀缓存
/// 每轮重建一次。
///
/// 断言两件事：证据必须进（样本数与首字），可靠性必须不动。
#[tokio::test]
async fn a_disconnect_after_the_answer_finished_still_feeds_the_evidence() {
    // `StreamUsageThenHang`：连 usage 收尾事件都发了，然后挂住不回结束标记。
    // 此时丢弃响应体 = 下游断开，而上游其实已经跑完。
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::StreamUsageThenHang);
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
    // 终止信号已送达：这是一次完整的回答，不是掉线（2026-11-19 修订）。
    assert_eq!(record.error_code, None, "{record:?}");
    // 夹具确实走了"上游写完、下游才断开"那条路：用量被记录下来了。
    // 注意判据本身是终止信号，不是这里的 output_tokens（见 stream.rs 的
    // `answer_completed`：Anthropic 开头就有 output_tokens 占位值）。
    assert!(
        record.output_tokens.is_some(),
        "夹具应当送出带 output_tokens 的收尾事件：{record:?}"
    );

    let after = akhub.state.runtime.perf.stats(&wired.target_id, dimension);
    assert_eq!(
        after.samples, 1,
        "跑完但下游断开的请求必须留下证据，否则低流量目标永远热不起来"
    );
    assert!(after.last_sample_at > 0, "采样时刻必须被记录");
    assert!(
        after.weight > 0.0,
        "样本权重必须被记下，它是跨过 MIN_SAMPLES 的唯一途径"
    );
    // 但可靠性一根汗毛都不能动。
    assert_eq!(after.success_rate, 1.0, "断开不得压低成功率（§9.3）");
}

/// **协议回归**：Anthropic 开头就报输入用量，不能在那一刻就认定"跑完了"。
///
/// `message_start` 带 `input_tokens`（还有缓存字段），但那时回答一个字都没写。
/// 若拿输入侧的用量当"上游跑完了"的判据，一次刚开始就被截断的请求会被当成
/// 完整证据喂进评分，把目标的质量判断污染掉（§9.3 修订）。
///
/// 这里用 `StreamThenHang` 让上游在**开头帧之后**就挂住：客户端断开时只有
/// 输入侧用量，因此必须走"没有证据"那条路，样本数保持为 0。
#[tokio::test]
async fn an_anthropic_truncation_before_output_is_not_evidence() {
    // 只发开头帧就挂住：真实 Anthropic 的 `message_start` 自带 `usage`
    // （`output_tokens` 还是占位值 1），回答一个字都没写。
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::StreamUsageStartThenHang);
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            "claude-x",
            "claude-x",
            50,
        ),
    )
    .await;

    let dimension = akhub::routing::score::Dimension {
        protocol: Protocol::AnthropicMessages,
        streaming: true,
    };

    let response = client()
        .post(format!("{}/v1/messages", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "claude-x",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "你好"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    drop(response);

    let record = wait_for_record(&akhub, 1).await;
    assert_eq!(record.error_code.as_deref(), Some("client_gone"));
    // 夹具确实发送了 output_tokens: 1，但它只是开场占位。
    // 输入可保留，输出必须未知，不能拿它退款或当成性能样本。
    assert_eq!(record.input_tokens, Some(120));
    assert_eq!(
        record.output_tokens, None,
        "开场占位不能冒充最终用量：{record:?}"
    );
    let after = akhub.state.runtime.perf.stats(&wired.target_id, dimension);
    assert_eq!(
        after.samples, 0,
        "回答还没写就断掉的请求不得成为证据，否则质量判断被污染"
    );
    assert_eq!(after.success_rate, 1.0, "可靠性同样不得被动");
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

/// **现场回归**：上游断流（EOF）时，akhub 必须按上游 newapi 口径补发带用量的终止帧，
/// 让下游 sub2api 正常记账计费，而不是把这次调用整条丢弃或按 95% 缓存读估算。
///
/// 修复前：上游 EOF 后 akhub 只发一个错误事件就断流，sub2api 拿不到终止帧与用量，
/// 把它当成"可重放失败"丢弃（少扣费），或按请求体估算并把 95% 当缓存读折扣计价。
/// 修复后：补出 Chat 的 finish_reason + usage + [DONE]，缓存并入输入、不带缓存细分。
#[tokio::test]
async fn an_upstream_eof_bills_with_a_merged_cache_usage_frame() {
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::StreamThenAbort);
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "gpt-5",
            "gpt-5",
            50,
        ),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "gpt-5",
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": 400,
            "messages": [{"role": "user", "content": "你好"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();

    // 下游必须拿到一个完整的协议终止：finish_reason + usage 收尾块 + [DONE]。
    assert!(
        body.contains("[DONE]"),
        "EOF 后必须补出 [DONE] 终止帧：{body}"
    );
    assert!(
        body.contains(r#""finish_reason":"stop""#),
        "EOF 后必须补出 finish_reason 终止信号：{body}"
    );
    assert!(
        body.contains(r#""prompt_tokens""#),
        "EOF 后必须补出带输入用量的收尾块，下游才计得了费：{body}"
    );
    assert!(
        !body.contains("cached_tokens"),
        "缓存必须并入输入、不再单独计价（与上游 newapi 口径一致）：{body}"
    );

    // 这次调用必须留下记录（而不是被当成"没有用量"丢弃）。EOF 被内层转发
    // 改写成错误帧再收尾，记 upstream_protocol_error；直接透传传输错误时记
    // upstream_exhausted——两者都是异常结束的稳定标识。
    let record = wait_for_record(&akhub, 1).await;
    assert!(
        matches!(
            record.error_code.as_deref(),
            Some("upstream_protocol_error" | "upstream_exhausted")
        ),
        "EOF 必须留下异常结束标识：{record:?}"
    );
    // 记录页显示的是**下发给下游计费的那一份**估算：否则控制台永远是
    // 「未知 / 未知」，与下游实际扣费的用量对不上。
    assert!(
        record.input_tokens.unwrap_or(0) > 0,
        "异常结束的记录必须显示计费用量：{record:?}"
    );
}

/// **现场回归**：上游流内错误帧（cyber-policy 拒绝）时，用量帧必须排在错误帧之前。
///
/// sub2api / newapi 读到错误事件就停止解析后续帧。修复前 akhub 把用量帧排在错误帧
/// 之后（甚至根本不发），下游的用量统计永远是 0——现场表现是「记录在、没有错误、
/// 输入输出 0、计费 0」。这条用例从真实入口打进去，断言的是**下发字节的顺序**。
#[tokio::test]
async fn an_upstream_error_event_bills_usage_before_the_error_frame() {
    // 假上游：Responses 协议，先送 created + 一个语义增量，再送 error 帧，然后结束。
    async fn handler(body: String) -> axum::response::Response {
        let _ = body;
        let frames = [
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_up\",\"status\":\"in_progress\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"code\":\"cyber_policy\",\"message\":\"Request blocked by cyber policy.\"}}\n\n",
        ].concat();
        ([("content-type", "text/event-stream")], frames).into_response()
    }
    let app = Router::new().route("/v1/responses", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let upstream = format!("http://{addr}");

    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream,
            Protocol::OpenAiResponses,
            "gpt-6-sol",
            "gpt-6-sol",
            50,
        )
        .pinned(),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-6-sol", "stream": true, "stream_options": {"include_usage": true}, "max_tokens": 400, "messages": [{"role": "user", "content": "你好"}]}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    // 用量帧必须排在错误帧之前：sub2api / newapi 读到错误就停止解析，排在后面的
    // 用量帧永远到不了它们的计费统计（现场就是这个顺序问题：记录在、用量 0、计费 0）。
    let usage_at = text.find(r#""usage":"#);
    let error_at = text.find(r#""error":"#);
    assert!(
        usage_at.is_some() && error_at.is_some() && usage_at < error_at,
        "用量帧必须排在错误帧之前：{text}"
    );
    // 错误之后照上游 newapi 的做法补协议终止帧：sub2api 的 raw 路径靠它清掉
    // "非计费错误"标记，从而按真实用量计费；错误本身已经先送达，客户端看得到中断。
    let finish_at = text.find(r#""finish_reason":"stop""#);
    assert!(
        finish_at.is_some() && error_at < finish_at,
        "错误之后必须补收尾帧（下游据此清掉非计费标记）：{text}"
    );
    assert!(text.contains("[DONE]"), "下游需要 [DONE] 才收尾：{text}");
    assert!(text.contains("cyber policy"), "错误原因必须保留：{text}");

    // 补出的用量帧必须是非零输入：上游没报量时要退回请求体估算，报 0 等于没报
    // （跨协议转换的开场占位常带 input_tokens:0，不能拿它当"已知用量"）。
    let usage_line = text
        .lines()
        .find(|line| line.contains("\"usage\":"))
        .expect("必须有用量帧");
    assert!(
        !usage_line.contains("\"prompt_tokens\":0"),
        "用量帧的输入不能是 0，否则下游仍按 0 计费：{usage_line}"
    );

    // akhub 自己的记录也要能看到这次计费的用量，控制台不再显示「未知 / 未知」。
    let record = wait_for_record(&akhub, 1).await;
    assert_eq!(
        record.error_code.as_deref(),
        Some("upstream_protocol_error")
    );
    assert!(
        record.input_tokens.unwrap_or(0) > 0,
        "记录必须显示计费用量：{record:?}"
    );
}

/// **碎片回归**：上游按 TCP 分片、一帧被切成多块到达时，错误帧之前的用量帧
/// 必须正常补出、且原字节一个不丢。修复按帧边界切块转发，跨块半帧靠 FrameReader
/// 内部缓冲拼回；这条用例把同一段字节按 1 字节一块喂进去，钉住切块与缓冲的对齐。
#[tokio::test]
async fn a_fragmented_error_stream_still_emits_billing_usage_before_the_error() {
    async fn handler() -> axum::response::Response {
        // 一次只给 1 字节：最强的分片压力，任何按块对齐的假设都会暴露。
        let frames = [
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\n",
            "data: {\"error\":{\"code\":\"cyber_policy\",\"message\":\"Request blocked by cyber policy.\"}}\n\n",
        ]
        .concat();
        let bytes = frames.into_bytes();
        let stream = futures::stream::unfold(0usize, move |offset| {
            let bytes = bytes.clone();
            async move {
                if offset >= bytes.len() {
                    return None;
                }
                let end = (offset + 1).min(bytes.len());
                Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(
                        &bytes[offset..end],
                    )),
                    end,
                ))
            }
        });
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(stream),
        )
            .into_response()
    }

    let app = Router::new().route("/v1/chat/completions", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &format!("http://{addr}"),
            Protocol::OpenAiChat,
            "gpt-5",
            "gpt-5",
            50,
        )
        .pinned(),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "gpt-5",
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": 400,
            "messages": [{"role": "user", "content": "你好"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    // 原字节不丢：语义增量必须原样到达。
    assert!(text.contains("你好"), "分片不得丢字节：{text}");
    // 用量帧仍在错误帧之前。
    let usage_at = text.find(r#""usage":"#);
    let error_at = text.find(r#""error":"#);
    assert!(
        usage_at.is_some() && error_at.is_some() && usage_at < error_at,
        "分片下用量帧仍必须排在错误帧之前：{text}"
    );
}

/// **现场回归**：客户端断开后，akhub 仍要把上游流读完，拿到与上游一致的 usage。
///
/// 现场（req_01M7Y760MFGD5SMVHVYPBERDN4）：终端客户端断开 → sub2api 取消 → akhub。
/// 修复前 akhub 立刻丢掉上游流，只留下「未知 / 未知」；而上游 New API 那边早就把
/// 真实用量记进了自己的账本（49,517/178），sub2api 只能用 95% 缓存读估算。
/// 修复后：断开只是不再往客户端写，上游流继续读完，usage 落进请求记录。
#[tokio::test]
async fn a_client_disconnect_still_drains_the_upstream_for_real_usage() {
    // `StreamUsageAfterRelease`：断开前只给语义增量，usage 与收尾等 release
    // 之后才发——这正是"断开时上游还在生成"的形状，drain 必须把它读回来。
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::StreamUsageAfterRelease);
    let akhub = spawn_akhub_with(
        Settings {
            client_gone_drain: std::time::Duration::from_secs(10),
            ..Settings::default()
        },
        |_| {},
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "gpt-5",
            "gpt-5",
            50,
        ),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "gpt-5",
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": 400,
            "messages": [{"role": "user", "content": "你好"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    // 先真的读掉第一块（现场：客户端已经在收流），再断开——这样下游 body
    // 确实进过 poll 循环，连接断开才是"半路走人"而不是"从没开始读"。
    let mut body = response.bytes_stream();
    let _ = body.next().await;
    drop(body);
    // 等 akhub 真正感知到断开，再放行上游——顺序反了的话上游会先把用量发完并
    // 正常结算，测到的就不是 drain 路径了。
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    upstream.release();

    // drain 是后台任务：给它一点时间把上游读完再落库。
    let mut record = None;
    for _ in 0..100 {
        if let Some(found) = akhub
            .state
            .store
            .list_request_records(5, 0)
            .await
            .unwrap()
            .first()
        {
            record = Some(found.clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    let record = record.expect("断开后必须仍然落一条记录");
    assert_eq!(
        record.error_code.as_deref(),
        Some("client_gone"),
        "{record:?}"
    );
    // 关键：上游在断开之后才发完，那些用量必须被 drain 读到。
    assert!(
        record.input_tokens.unwrap_or(0) > 0,
        "断开后必须把上游的输入用量读回来，而不是记成未知：{record:?}"
    );
    assert!(
        record.output_tokens.unwrap_or(0) > 0,
        "断开后必须把上游的输出用量读回来：{record:?}"
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
