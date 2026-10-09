//! 长思考不会撞总时长上限；无数据仍超时，开场用量不冒充最终用量。
mod common;

use akhub::app::Settings;
use akhub::domain::Protocol;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    response::IntoResponse,
    routing::post,
};
use common::{Behavior, FakeUpstream, TargetSpec, request, spawn_akhub_with, wire_target};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Clone)]
struct Script {
    frames: Vec<String>,
    delay: Duration,
    hang: bool,
    hits: Arc<AtomicUsize>,
}

async fn scripted(script: Script) -> String {
    async fn handler(State(script): State<Script>) -> impl IntoResponse {
        script.hits.fetch_add(1, Ordering::SeqCst);
        let stream = async_stream::stream! {
            for frame in script.frames {
                yield Ok::<_, std::io::Error>(Bytes::from(frame));
                tokio::time::sleep(script.delay).await;
            }
            if script.hang { std::future::pending::<()>().await; }
        };
        (
            [("content-type", "text/event-stream")],
            Body::from_stream(stream),
        )
    }
    let app = Router::new()
        .route("/v1/messages", post(handler))
        .route("/v1/chat/completions", post(handler))
        .route("/v1/responses", post(handler))
        .with_state(script);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

fn frame(kind: &str, value: Value) -> String {
    format!("event: {kind}\ndata: {value}\n\n")
}

fn anthropic_frames(output: Option<u64>) -> Vec<String> {
    let mut frames = vec![
        frame(
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_test","model":"claude-sonnet-4-5","role":"assistant","usage":{"input_tokens":100,"output_tokens":0,"cache_read_input_tokens":20}}}),
        ),
        frame(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        ),
    ];
    for _ in 0..8 {
        frames.push(frame("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"continued thought"}})));
    }
    frames.push(frame(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    ));
    let usage = output
        .map(|n| json!({"output_tokens":n}))
        .unwrap_or(json!({}));
    frames.push(frame(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":usage}),
    ));
    frames.push(frame("message_stop", json!({"type":"message_stop"})));
    frames
}

fn body(protocol: Protocol, streaming: bool) -> Value {
    if protocol == Protocol::OpenAiResponses {
        json!({"model":"claude-sonnet-4-5","stream":streaming,"input":"hello"})
    } else {
        json!({"model":"claude-sonnet-4-5","stream":streaming,"max_tokens":100,"messages":[{"role":"user","content":"hello"}]})
    }
}

async fn record(akhub: &common::Akhub) -> akhub::storage::store::RequestRecord {
    for _ in 0..100 {
        if let Some(record) = akhub
            .state
            .store
            .list_request_records(1, 0)
            .await
            .unwrap()
            .pop()
        {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("missing request record");
}

async fn wire(akhub: &common::Akhub, url: &str, protocol: Protocol) -> common::Wired {
    wire_target(
        akhub,
        TargetSpec::new(
            "stream",
            url,
            protocol,
            "claude-sonnet-4-5",
            "claude-sonnet-4-5",
            50,
        )
        .pinned(),
    )
    .await
}

#[tokio::test]
async fn active_thinking_outlives_total_timeout_for_native_and_translated_streams() {
    for downstream in [
        Protocol::AnthropicMessages,
        Protocol::OpenAiChat,
        Protocol::OpenAiResponses,
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let url = scripted(Script {
            frames: anthropic_frames(Some(42)),
            delay: Duration::from_millis(80),
            hang: false,
            hits: hits.clone(),
        })
        .await;
        let akhub = spawn_akhub_with(
            Settings {
                request_timeout: Duration::from_millis(400),
                stream_idle_timeout: Duration::from_millis(400),
                ..Settings::default()
            },
            |_| {},
        )
        .await;
        let wired = wire(&akhub, &url, Protocol::AnthropicMessages).await;
        let response = request(&akhub, downstream, body(downstream, true)).await;
        assert_eq!(response.status(), 200, "{downstream:?}");
        let text = tokio::time::timeout(Duration::from_secs(5), response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(text.contains("continued thought"), "{downstream:?}: {text}");
        assert!(!text.contains("\"error\""), "{downstream:?}: {text}");
        if downstream == Protocol::OpenAiChat {
            assert!(
                !text.contains("\"usage\""),
                "未索取 usage 的下游不能多收到统计帧：{text}"
            );
        }
        let r = record(&akhub).await;
        assert!(r.duration_ms > 800, "{r:?}");
        assert_eq!(r.error_code, None, "{r:?}");
        assert_eq!(r.output_tokens, Some(42), "{r:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            akhub
                .state
                .runtime
                .health
                .target(&wired.target_id)
                .inflight(),
            0
        );
    }
}

#[tokio::test]
async fn idle_stream_times_out_once_without_replay_or_placeholder_usage() {
    for downstream in [
        Protocol::AnthropicMessages,
        Protocol::OpenAiChat,
        Protocol::OpenAiResponses,
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut frames = anthropic_frames(None);
        frames.truncate(3);
        let url = scripted(Script {
            frames,
            delay: Duration::ZERO,
            hang: true,
            hits: hits.clone(),
        })
        .await;
        let akhub = spawn_akhub_with(
            Settings {
                request_timeout: Duration::from_secs(3),
                stream_idle_timeout: Duration::from_millis(150),
                ..Settings::default()
            },
            |_| {},
        )
        .await;
        let wired = wire(&akhub, &url, Protocol::AnthropicMessages).await;
        let response = request(&akhub, downstream, body(downstream, true)).await;
        assert_eq!(response.status(), 200);
        let text = tokio::time::timeout(Duration::from_secs(2), response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(text.contains("upstream_timeout"), "{text}");
        let r = record(&akhub).await;
        assert_eq!(r.error_code.as_deref(), Some("upstream_timeout"), "{r:?}");
        assert_eq!(r.output_tokens, None, "{r:?}");
        if downstream == Protocol::AnthropicMessages {
            assert_eq!(r.input_tokens, Some(120));
        }
        assert_eq!(r.output_tps, None);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            akhub
                .state
                .runtime
                .health
                .target(&wired.target_id)
                .inflight(),
            0
        );
    }
}

#[tokio::test]
async fn pre_stream_and_non_stream_waits_still_have_a_total_deadline() {
    for streaming in [true, false] {
        let upstream = FakeUpstream::spawn().await;
        upstream.fallback(Behavior::Hang);
        let hits = Arc::new(AtomicUsize::new(0));
        let url = if streaming {
            scripted(Script {
                frames: anthropic_frames(None).into_iter().take(1).collect(),
                delay: Duration::ZERO,
                hang: true,
                hits: hits.clone(),
            })
            .await
        } else {
            upstream.base_url.clone()
        };
        let akhub = spawn_akhub_with(
            Settings {
                request_timeout: Duration::from_millis(150),
                ..Settings::default()
            },
            |_| {},
        )
        .await;
        let wired = wire(&akhub, &url, Protocol::AnthropicMessages).await;
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            request(
                &akhub,
                Protocol::AnthropicMessages,
                body(Protocol::AnthropicMessages, streaming),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), 504);
        let payload: Value = response.json().await.unwrap();
        assert!(
            payload.to_string().contains("upstream_timeout"),
            "{payload}"
        );
        assert_eq!(
            if streaming {
                hits.load(Ordering::SeqCst)
            } else {
                upstream.requests()
            },
            1,
            "超时不重放"
        );
        assert_eq!(
            akhub
                .state
                .runtime
                .health
                .target(&wired.target_id)
                .inflight(),
            0
        );
    }
}

#[tokio::test]
async fn native_chat_and_responses_streams_outlive_the_start_deadline() {
    for protocol in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
        let upstream = FakeUpstream::spawn().await;
        upstream.fallback(Behavior::StreamThenHang);
        let akhub = spawn_akhub_with(
            Settings {
                request_timeout: Duration::from_millis(150),
                stream_idle_timeout: Duration::from_secs(2),
                ..Settings::default()
            },
            |_| {},
        )
        .await;
        wire(&akhub, &upstream.base_url, protocol).await;
        let response = request(&akhub, protocol, body(protocol, true)).await;
        assert_eq!(response.status(), 200);
        tokio::time::sleep(Duration::from_millis(400)).await;
        upstream.release();
        let text = response.text().await.unwrap();
        assert!(!text.contains("\"error\""), "{protocol:?}: {text}");
        let r = record(&akhub).await;
        assert_eq!(r.error_code, None, "{r:?}");
        assert!(r.duration_ms >= 400, "{r:?}");
        assert_eq!(upstream.requests(), 1);
    }
}

#[tokio::test]
async fn missing_final_usage_is_unknown_but_a_real_zero_is_preserved() {
    for output in [None, Some(0), Some(42)] {
        for downstream in [
            Protocol::AnthropicMessages,
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
        ] {
            let url = scripted(Script {
                frames: anthropic_frames(output),
                delay: Duration::ZERO,
                hang: false,
                hits: Arc::new(AtomicUsize::new(0)),
            })
            .await;
            let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
            wire(&akhub, &url, Protocol::AnthropicMessages).await;
            let response = request(&akhub, downstream, body(downstream, true)).await;
            assert_eq!(response.status(), 200);
            response.text().await.unwrap();
            let r = record(&akhub).await;
            assert_eq!(r.error_code, None, "{r:?}");
            assert_eq!(
                r.output_tokens,
                output.map(|n| n as i64),
                "{downstream:?}: {r:?}"
            );
        }
    }
}
