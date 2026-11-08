//! 调度与故障转移的定向回归（2026-11 审计）。
//!
//! 每一条都走真实的 HTTP 入口与转发路径，不直接调内部函数。
mod common;

use std::time::{Duration, Instant};

use akhub::domain::{Limits, Protocol};
use common::{FakeUpstream, TargetSpec, chat, spawn_akhub_with, wire_target};
use serde_json::{Value, json};

/// 单条请求的 Token 估算就超过 TPM 上限：窗口永远装不下它，必须立刻失败，
/// 而不是排队干等到超时。`max_completion_tokens` 也要计入估算。
#[tokio::test]
async fn a_request_larger_than_the_tpm_limit_fails_fast() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub_with(Default::default(), |group| group.max_wait_secs = 30).await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "tpm",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "m",
            "m",
            50,
        )
        .limits(Limits {
            tpm: Some(1000),
            ..Default::default()
        }),
    )
    .await;

    let started = Instant::now();
    let response = chat(
        &akhub,
        json!({"model":"m","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":5000}),
    )
    .await;
    assert_eq!(response.status(), 413, "超过 TPM 的单条请求不可重试");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "request_too_large", "{body}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "不能排队等到超时：{:?}",
        started.elapsed()
    );
    assert_eq!(upstream.requests(), 0);
}

/// 只配了 RPM、没配并发上限时，限流中的请求排队必须**睡**而不是忙等。
///
/// 单线程运行时下忙等会饿死同一运行时上的其它任务：另一个不受限的模型的
/// 请求在排队期间必须照常很快完成。
#[tokio::test(flavor = "current_thread")]
async fn a_rate_limited_wait_does_not_spin() {
    let limited = FakeUpstream::spawn().await;
    let free = FakeUpstream::spawn().await;
    let akhub = spawn_akhub_with(Default::default(), |group| group.max_wait_secs = 3).await;
    wire_target(
        &akhub,
        TargetSpec::new("rpm", &limited.base_url, Protocol::OpenAiChat, "m", "m", 50).limits(
            Limits {
                rpm: Some(1),
                ..Default::default()
            },
        ),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("free", &free.base_url, Protocol::OpenAiChat, "f", "f", 50),
    )
    .await;

    assert_eq!(
        chat(&akhub, json!({"model":"m","messages":[]}))
            .await
            .status(),
        200
    );
    let started = Instant::now();
    let waiting = tokio::spawn(chat(&akhub, json!({"model":"m","messages":[]})));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let probe = Instant::now();
    assert_eq!(
        chat(&akhub, json!({"model":"f","messages":[]}))
            .await
            .status(),
        200
    );
    assert!(
        probe.elapsed() < Duration::from_millis(1500),
        "排队中的限流请求把运行时钉死了：{:?}",
        probe.elapsed()
    );

    let response = waiting.await.unwrap();
    assert_eq!(response.status(), 429);
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(limited.requests(), 1, "窗口没释放前不能再打上游");
}
