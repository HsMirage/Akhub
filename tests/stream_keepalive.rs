//! 首个语义事件到达之前必须向下游发心跳：否则 CDN 会把"零字节"判成源站无响应，
//! Cloudflare 约 100 秒后回 524、EdgeOne 的长挂连接放大 522（§13.4 修订）。
//!
//! 现场：opus-5-5 的首字延迟动辄几十秒到几分钟，中间任何一层 CDN 都撑不住。

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
use common::{TargetSpec, request, spawn_akhub_with, wire_target};
use futures::StreamExt as _;
use serde_json::{Value, json};
use std::time::Duration;

/// 上游先沉默 `silence`，之后才吐一个语义事件。
#[derive(Clone)]
struct Slow {
    silence: Duration,
}

async fn slow_upstream(State(slow): State<Slow>) -> impl IntoResponse {
    let stream = async_stream::stream! {
        // 协议开始标记（不算语义内容）——和真实上游一样先给个开场。
        yield Ok::<_, std::io::Error>(Bytes::from(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        ));
        tokio::time::sleep(slow.silence).await;
        yield Ok::<_, std::io::Error>(Bytes::from(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        ));
        yield Ok::<_, std::io::Error>(Bytes::from(
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
    };
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(stream),
    )
}

/// 上游先沉默 `headers` 才交出响应头，再沉默 `silence` 才吐语义事件。
///
/// 与 `Slow` 分开是必要的：心跳路径要测的正是"响应头之前 + 之后"两段等待
/// 合起来有多长，只有把响应头也推迟，才量得出基准有没有丢掉前一段。
#[derive(Clone)]
struct SlowHeaders {
    headers: Duration,
    silence: Duration,
}

async fn slow_headers_upstream(State(slow): State<SlowHeaders>) -> impl IntoResponse {
    tokio::time::sleep(slow.headers).await;
    let silence = slow.silence;
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::from(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        ));
        tokio::time::sleep(silence).await;
        yield Ok::<_, std::io::Error>(Bytes::from(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        ));
        yield Ok::<_, std::io::Error>(Bytes::from(
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
    };
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(stream),
    )
}

async fn spawn_headers_upstream(headers: Duration, silence: Duration) -> String {
    let app = Router::new()
        .route("/v1/messages", post(slow_headers_upstream))
        .route("/v1/chat/completions", post(slow_headers_upstream))
        .with_state(SlowHeaders { headers, silence });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

async fn spawn_upstream(silence: Duration) -> String {
    let app = Router::new()
        .route("/v1/messages", post(slow_upstream))
        .route("/v1/chat/completions", post(slow_upstream))
        .with_state(Slow { silence });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

fn body(protocol: Protocol) -> Value {
    if protocol == Protocol::OpenAiResponses {
        json!({"model":"claude-sonnet-4-5","stream":true,"input":"hello"})
    } else {
        json!({"model":"claude-sonnet-4-5","stream":true,"max_tokens":100,"messages":[{"role":"user","content":"hello"}]})
    }
}

async fn wire(akhub: &common::Akhub, url: &str, protocol: Protocol) {
    wire_target(
        akhub,
        TargetSpec::new(
            "slow",
            url,
            protocol,
            "claude-sonnet-4-5",
            "claude-sonnet-4-5",
            50,
        )
        .pinned(),
    )
    .await;
}

/// 上游沉默 3 秒（远大于测试用的宽限期），下游必须在语义内容之前先收到注释帧。
///
/// 判据是**响应头与首个字节的到达时间**，不是最终状态码：CDN 看的是"源站在
/// 首字之前有没有发过东西"，524 就是在这一段时间里判出来的。
#[tokio::test]
async fn a_silent_upstream_still_produces_bytes_before_the_first_semantic_event() {
    let url = spawn_upstream(Duration::from_millis(3000)).await;
    // 宽限期压到 300ms，让"等不到语义事件"这条路在测试里真的走到。
    let akhub = spawn_akhub_with(
        Settings {
            first_content_grace: Duration::from_millis(300),
            keep_alive_interval: Duration::from_millis(200),
            ..Settings::default()
        },
        |_| {},
    )
    .await;
    wire(&akhub, &url, Protocol::AnthropicMessages).await;

    let response = request(
        &akhub,
        Protocol::AnthropicMessages,
        body(Protocol::AnthropicMessages),
    )
    .await;
    assert_eq!(response.status(), 200);
    let mut stream = response.bytes_stream();

    // 在 3 秒沉默结束之前，必须已经收到过心跳；而且到那一刻为止都还没有
    // 语义内容。CDN 判的就是这一段：首字之前有没有字节。
    let started = tokio::time::Instant::now();
    let mut seen = String::new();
    let mut saw_keepalive = false;
    while started.elapsed() < Duration::from_millis(2500) {
        let remaining = Duration::from_millis(2500).saturating_sub(started.elapsed());
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                if seen.contains("keep-alive") {
                    saw_keepalive = true;
                    break;
                }
            }
            Ok(Some(Err(error))) => panic!("流不应该报错：{error}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        saw_keepalive,
        "首字之前必须收到心跳，否则 CDN 会判源站无响应（实际收到：{seen:?}）"
    );
    assert!(
        !seen.contains("text_delta"),
        "心跳到达时上游还没产出语义内容：{seen:?}"
    );

    // 语义内容随后照常到达，客户端拿到完整回答。
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while let Ok(Some(item)) = tokio::time::timeout_at(deadline, stream.next()).await {
        text.push_str(&String::from_utf8_lossy(&item.expect("流不应该报错")));
        if text.contains("message_stop") {
            break;
        }
    }
    assert!(text.contains("hello"), "语义内容不能丢：{text:?}");
    assert!(!text.contains("\"error\""), "不该出现错误帧：{text:?}");
}

/// 心跳只发生在语义内容之前：一旦真内容到了就停，不能污染下游的流。
#[tokio::test]
async fn heartbeats_stop_once_semantic_content_arrives() {
    let url = spawn_upstream(Duration::from_millis(1200)).await;
    let akhub = spawn_akhub_with(
        Settings {
            first_content_grace: Duration::from_millis(300),
            keep_alive_interval: Duration::from_millis(200),
            ..Settings::default()
        },
        |_| {},
    )
    .await;
    wire(&akhub, &url, Protocol::AnthropicMessages).await;

    let response = request(
        &akhub,
        Protocol::AnthropicMessages,
        body(Protocol::AnthropicMessages),
    )
    .await;
    let text = tokio::time::timeout(Duration::from_secs(8), response.text())
        .await
        .unwrap()
        .unwrap();
    assert!(text.contains("hello"), "{text:?}");
    // 心跳后面不应再有注释帧：语义内容之后网关是纯透传。
    if let Some(position) = text.find("text_delta") {
        let after = &text[position..];
        assert!(
            !after.contains("keep-alive"),
            "语义内容之后不该再补心跳：{after:?}"
        );
    }
}

/// 客户端等不下去提前断开时，首字延迟不能被心跳时间冒充。
///
/// 现场：opus-5-5 首字 100–400 秒，客户端先超时（client_gone 366 条）。
/// 那时"用户等到内容"的时刻根本没出现，记录里必须留空。
#[tokio::test]
async fn a_client_that_gives_up_before_content_records_no_first_token() {
    let url = spawn_upstream(Duration::from_secs(30)).await;
    let akhub = spawn_akhub_with(
        Settings {
            first_content_grace: Duration::from_millis(300),
            keep_alive_interval: Duration::from_millis(200),
            ..Settings::default()
        },
        |_| {},
    )
    .await;
    wire(&akhub, &url, Protocol::AnthropicMessages).await;

    let response = request(
        &akhub,
        Protocol::AnthropicMessages,
        body(Protocol::AnthropicMessages),
    )
    .await;
    let mut stream = response.bytes_stream();
    // 收到心跳（说明响应头已下发）之后立刻断开。
    let _ = tokio::time::timeout(Duration::from_millis(2500), stream.next()).await;
    drop(stream);

    let mut record = None;
    for _ in 0..200 {
        if let Some(found) = akhub
            .state
            .store
            .list_request_records(1, 0)
            .await
            .unwrap()
            .pop()
        {
            record = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let record = record.expect("应该有请求记录");
    assert_eq!(
        record.first_token_ms, None,
        "真内容没来就不能把心跳的时间写成首字延迟：{record:?}"
    );
    assert_eq!(
        record.error_code.as_deref(),
        Some("client_gone"),
        "{record:?}"
    );
}

/// 心跳路径的首字延迟必须是"用户等到内容"的**全部**时长。
///
/// 现场（2026-10-14）：opus-5-5 首字 100–400 秒，其中"上游响应头之前"那一段
/// 常常占大头。心跳路径把响应头提前交出去了，如果计时从"开始转发响应体"起算，
/// 前一段等待就被整段丢掉——请求记录会显示一个远小于用户体感的数字，
/// 性能评分与容量规划跟着失真（§6.6、§9.3）。
#[tokio::test]
async fn the_heartbeat_path_reports_the_whole_wait_not_just_its_tail() {
    // 1.2s 等响应头 + 1.2s 等语义事件 = 用户真正等了约 2.4s。
    let url =
        spawn_headers_upstream(Duration::from_millis(1200), Duration::from_millis(1200)).await;
    let akhub = spawn_akhub_with(
        Settings {
            first_content_grace: Duration::from_millis(300),
            keep_alive_interval: Duration::from_millis(200),
            ..Settings::default()
        },
        |_| {},
    )
    .await;
    wire(&akhub, &url, Protocol::AnthropicMessages).await;

    let response = request(
        &akhub,
        Protocol::AnthropicMessages,
        body(Protocol::AnthropicMessages),
    )
    .await;
    assert_eq!(response.status(), 200);
    let text = tokio::time::timeout(Duration::from_secs(15), response.text())
        .await
        .unwrap()
        .unwrap();
    assert!(text.contains("hello"), "语义内容不能丢：{text:?}");

    let mut record = None;
    for _ in 0..200 {
        if let Some(found) = akhub
            .state
            .store
            .list_request_records(1, 0)
            .await
            .unwrap()
            .pop()
        {
            record = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let record = record.expect("应该有请求记录");
    let ms = record.first_token_ms.expect("语义内容到了就该有首字延迟");
    assert!(
        (2000..=3400).contains(&ms),
        "首字延迟要覆盖等响应头的那一段（约 2400ms），实际 {ms}ms"
    );
}

/// 语义帧被切在两次读取之间时，心跳路径不能把它当成噪声。
///
/// 上游按 TCP 分片，切点由不得我们：一个 content_block_delta 完全可能被切成
/// "前半段先到、后半段后到"。心跳提交时如果把已发出的字节从嗅探缓冲区里
/// 清掉，那个帧的尾巴就再也拼不回来——整条流被判成"没有语义内容"，
/// 客户端最终拿到的是一个凭空捏造的"上游在产生内容前就结束"错误帧。
#[tokio::test]
async fn a_semantic_frame_split_across_chunks_survives_the_handover() {
    let url = spawn_split_frame_upstream().await;
    let akhub = spawn_akhub_with(
        Settings {
            first_content_grace: Duration::from_millis(200),
            keep_alive_interval: Duration::from_millis(150),
            ..Settings::default()
        },
        |_| {},
    )
    .await;
    wire(&akhub, &url, Protocol::AnthropicMessages).await;

    let response = request(
        &akhub,
        Protocol::AnthropicMessages,
        body(Protocol::AnthropicMessages),
    )
    .await;
    assert_eq!(response.status(), 200);
    let text = tokio::time::timeout(Duration::from_secs(10), response.text())
        .await
        .unwrap()
        .unwrap();
    assert!(text.contains("hello"), "被切开的语义帧不能丢：{text:?}");
    assert!(
        !text.contains("产生内容前就结束"),
        "不该补一个假的断流错误帧：{text:?}"
    );
    // 心跳不能插进一个没收全的帧里：那会把 data 行截断，下游解析出坏 JSON。
    for line in text.lines() {
        if line.trim_start().starts_with("data:") {
            let payload = line.trim_start().trim_start_matches("data:").trim();
            assert!(
                serde_json::from_str::<Value>(payload).is_ok(),
                "下游收到的每个 data 行都必须是完整 JSON（心跳插进了帧中间？）：{payload:?}"
            );
        }
    }
}

/// 上游：送出一个**语义帧的前半段**（帧没结束，嗅探器只能把它留在缓冲区），
/// 然后沉默到宽限期之后才补上后半段。
///
/// 这样"开始发心跳"的那一刻，缓冲区里正躺着一个被切开的帧。心跳是独立的
/// SSE 注释帧，只能落在帧边界上——插进这个帧中间会把它的 data 行截断，
/// 客户端拿到的 JSON 就是坏的。
async fn split_frame_upstream() -> impl IntoResponse {
    let stream = async_stream::stream! {
        let frame = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n";
        let (head, tail) = frame.split_at(40);
        // 前半段在宽限期之内到达：帧没结束，只能进缓冲区。
        yield Ok::<_, std::io::Error>(Bytes::from(head.to_string()));
        // 沉默超过宽限期：网关在这里开始发心跳。
        tokio::time::sleep(Duration::from_millis(600)).await;
        // 后半段在心跳之后才到：必须能跟缓冲区里的前半段拼回一个完整帧。
        yield Ok::<_, std::io::Error>(Bytes::from(tail.to_string()));
        yield Ok::<_, std::io::Error>(Bytes::from(
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
    };
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(stream),
    )
}

async fn spawn_split_frame_upstream() -> String {
    let app = Router::new()
        .route("/v1/messages", post(split_frame_upstream))
        .route("/v1/chat/completions", post(split_frame_upstream));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}
