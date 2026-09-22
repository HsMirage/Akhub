//! 首字 / 速度两个维度的**采样口径**验收（§6.6、§9.3）。
//!
//! 这两维长期存在一个共同病症：某些场景下永远采不到数据，于是评分里它们
//! 只能取中性分常数，分配给它们的权重（默认 20 / 15）等于白给。这里把两条
//! 采集路径钉死：
//!
//! - 非流式响应也要记「用户等到的时刻」，否则样本落在非流式维度上的目标首字维恒为常数；
//! - 网关必须主动向上游索取 usage，否则没有客户端会写 stream_options，输出速度维恒为常数、TPM 也无法按真实用量归还。

mod common;

use akhub::domain::{Limits, Protocol};
use akhub::routing::score::Dimension;
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::post;
use common::{TargetSpec, client, spawn_akhub, wire_target};
use serde_json::{Value, json};

/// 假上游：记录收到的请求体，并按脚本回一个 Chat 响应。
#[derive(Clone, Copy)]
struct Script {
    /// 流式响应里是否带 usage 收尾块。
    stream_usage: bool,
}

type SeenBodies = std::sync::Arc<std::sync::Mutex<Vec<Value>>>;

async fn spawn_upstream(script: Script) -> (String, SeenBodies) {
    let seen: SeenBodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    async fn handler(
        axum::extract::State((script, seen)): axum::extract::State<(Script, SeenBodies)>,
        body: String,
    ) -> axum::response::Response {
        let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        seen.lock().unwrap().push(request.clone());
        let streaming = request
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if streaming {
            let mut frames = vec![
                "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n".to_string(),
                "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\n".to_string(),
                "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string(),
            ];
            if script.stream_usage {
                frames.push(
                    "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":11,\"total_tokens\":16}}\n\n"
                        .to_string(),
                );
            }
            frames.push("data: [DONE]\n\n".to_string());
            return ([("content-type", "text/event-stream")], frames.concat()).into_response();
        }
        axum::Json(json!({
            "id": "c1",
            "object": "chat.completion",
            "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "你好"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 11, "total_tokens": 16},
        }))
        .into_response()
    }

    let app = Router::new()
        .route("/v1/chat/completions", post(handler))
        .with_state((script, seen.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

/// 非流式请求也必须给首字维留下一个真实样本。
///
/// 这一维此前只在流式路径上产生数据：commit_body 明确写死空值，于是
/// 「某模型的样本落在非流式维度」时首字得分恒为中性分，权重全部浪费。
#[tokio::test]
async fn a_non_streaming_response_still_feeds_the_first_token_dimension() {
    let (upstream, _seen) = spawn_upstream(Script { stream_usage: true }).await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new("账号A", &upstream, Protocol::OpenAiChat, "m", "m", 50),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "m",
            "stream": false,
            "messages": [{"role": "user", "content": "你好"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.text().await.unwrap();

    let dimension = Dimension {
        protocol: Protocol::OpenAiChat,
        streaming: false,
    };
    let stats = akhub.state.runtime.perf.stats(&wired.target_id, dimension);
    assert_eq!(stats.samples, 1, "非流式成功请求必须进性能样本");
    assert!(
        stats.first_token_ms > 0.0,
        "非流式也必须给出首字维的测量值，否则该维恒为中性分：{stats:?}"
    );
    assert!(
        stats.first_token_ms < 60_000.0,
        "首字延迟不应是离谱值：{}",
        stats.first_token_ms
    );
}

/// 网关必须主动向上游索取 usage，即使下游没写 stream_options。
///
/// 输出速度维取自流里的真实 usage；若只在「客户端主动要了」时才索取，那么
/// 绝大多数 SDK 默认流量都拿不到输出 Token，吞吐维恒为中性分，TPM 也只能
/// 一直保守占用到窗口过期（§9.3、§17.2）。
#[tokio::test]
async fn the_gateway_asks_for_usage_even_when_the_client_did_not() {
    let (upstream, seen) = spawn_upstream(Script { stream_usage: true }).await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new("账号A", &upstream, Protocol::OpenAiChat, "m", "m", 50),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "m",
            "stream": true,
            "messages": [{"role": "user", "content": "你好"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.text().await.unwrap();

    let forwarded = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        forwarded["stream_options"]["include_usage"], true,
        "网关必须主动索取 usage，否则吞吐维永远采不到数据：{forwarded}"
    );

    let dimension = Dimension {
        protocol: Protocol::OpenAiChat,
        streaming: true,
    };
    let stats = akhub.state.runtime.perf.stats(&wired.target_id, dimension);
    assert_eq!(stats.samples, 1);
    assert!(
        stats.output_tps > 0.0,
        "拿到 usage 后输出速度维必须有真实测量值：{stats:?}"
    );
}

/// 下游没要 usage 时，网关**不得**把这个额外的收尾块塞给客户端。
///
/// 同协议路径按字节原样转发；向上一句索取 usage 之后，若原样把这些字节
/// 放行，客户端会平白收到一个自己没要的 choices:[] 帧。行为要变的是
/// 「网关知道用量」，不是「客户端的响应形状变了」。
#[tokio::test]
async fn an_unrequested_usage_chunk_is_not_forwarded_to_the_client() {
    let (upstream, _seen) = spawn_upstream(Script { stream_usage: true }).await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("账号A", &upstream, Protocol::OpenAiChat, "m", "m", 50),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "m",
            "stream": true,
            "messages": [{"role": "user", "content": "你好"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("你好"), "正文必须照常转发：{text}");
    assert!(
        !text.contains("\"usage\""),
        "下游没有索取 usage，网关就不该把这个收尾块塞给它：{text}"
    );
}

/// 同一份流：客户端主动要了 usage 时，必须照常收到。
#[tokio::test]
async fn a_requested_usage_chunk_still_reaches_the_client() {
    let (upstream, _seen) = spawn_upstream(Script { stream_usage: true }).await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("账号A", &upstream, Protocol::OpenAiChat, "m", "m", 50),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "m",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "你好"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(
        text.contains("\"usage\"") && text.contains("total_tokens"),
        "客户端主动要了 usage，就必须收到：{text}"
    );
}

/// 非流式：TPM 归还同样依赖响应体里的 usage（§17.2）。
#[tokio::test]
async fn a_non_streaming_response_refunds_tpm_with_its_usage() {
    let (upstream, _seen) = spawn_upstream(Script { stream_usage: true }).await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("账号A", &upstream, Protocol::OpenAiChat, "m", "m", 50).limits(Limits {
            tpm: Some(600),
            ..Limits::default()
        }),
    )
    .await;

    for round in 0..2 {
        let response = client()
            .post(format!("{}/v1/chat/completions", akhub.base_url))
            .bearer_auth(&akhub.key)
            .json(&json!({
                "model": "m",
                "stream": false,
                "max_tokens": 400,
                "messages": [{"role": "user", "content": "你好"}],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "第 {} 次请求应放行", round + 1);
        let _ = response.text().await.unwrap();
    }
}
