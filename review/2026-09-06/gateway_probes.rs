//! 本轮审查的失败复现用例归档；在隔离副本中作为 tests/review_probes.rs 运行。
mod common;

use std::time::Duration;

use akhub::app::Settings;
use akhub::domain::{Limits, Protocol};
use common::{
    Behavior, FakeUpstream, TargetSpec, chat, responses, spawn_akhub, spawn_akhub_with,
    wire_extra_target, wire_target,
};
use serde_json::{Value, json};

#[tokio::test]
async fn account_concurrency_is_shared_across_models() {
    let upstream = FakeUpstream::spawn().await;
    upstream.script([Behavior::Hang]);
    let akhub = spawn_akhub().await;
    let target = wire_target(
        &akhub,
        TargetSpec::new(
            "shared",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "one",
            "one",
            50,
        )
        .limits(Limits {
            max_concurrency: Some(1),
            ..Default::default()
        }),
    )
    .await;
    wire_extra_target(&akhub, &target.account_id, "two", "two").await;
    let first = tokio::spawn(chat(&akhub, json!({"model":"one","messages":[]})));
    upstream.wait_for_requests(1).await;
    let second = tokio::spawn(chat(&akhub, json!({"model":"two","messages":[]})));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before_release = upstream.requests();
    upstream.release();
    assert_eq!(first.await.unwrap().status(), 200);
    assert_eq!(second.await.unwrap().status(), 200);
    assert_eq!(
        before_release, 1,
        "account concurrency=1 must cover both models"
    );
}

#[tokio::test]
async fn recovered_high_priority_preempts_lower_sticky_binding() {
    let high = FakeUpstream::spawn().await;
    let low = FakeUpstream::spawn().await;
    high.script([Behavior::Status(500, None)]);
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("high", &high.base_url, Protocol::OpenAiChat, "m", "m", 100),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("low", &low.base_url, Protocol::OpenAiChat, "m", "m", 50),
    )
    .await;
    let body = json!({"model":"m","messages":[{"role":"system","content":"stable"},{"role":"user","content":"hi"}]});
    assert_eq!(chat(&akhub, body.clone()).await.status(), 200);
    assert_eq!(chat(&akhub, body).await.status(), 200);
    assert_eq!(
        high.requests(),
        2,
        "a healthy top tier must be used after recovery"
    );
}

#[tokio::test]
async fn tpm_settlement_keeps_input_usage() {
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::Json(200, json!({"id":"c","object":"chat.completion","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":900,"completion_tokens":10,"total_tokens":910}})));
    let akhub = spawn_akhub_with(
        Settings {
            request_timeout: Duration::from_millis(300),
            ..Default::default()
        },
        |g| g.queue_capacity = 0,
    )
    .await;
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
    let body = json!({"model":"m","messages":[{"role":"user","content":"a".repeat(2750)}],"max_tokens":10});
    assert_eq!(chat(&akhub, body.clone()).await.status(), 200);
    assert_eq!(
        chat(&akhub, body).await.status(),
        429,
        "input+output usage must remain reserved"
    );
}

#[tokio::test]
async fn responses_failover_keeps_function_call_output() {
    let high = FakeUpstream::spawn().await;
    let low = FakeUpstream::spawn().await;
    high.script([Behavior::ToolCall, Behavior::Status(500, None)]);
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "high",
            &high.base_url,
            Protocol::OpenAiResponses,
            "m",
            "m",
            100,
        )
        .pinned(),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "low",
            &low.base_url,
            Protocol::OpenAiResponses,
            "m",
            "m",
            50,
        )
        .pinned(),
    )
    .await;
    let first: Value = responses(&akhub, json!({"model":"m","input":"weather"}))
        .await
        .json()
        .await
        .unwrap();
    let result = responses(&akhub, json!({"model":"m","previous_response_id":first["id"],"input":[{"type":"function_call_output","call_id":"call_1","output":"review-tool-result"}]})).await;
    assert_eq!(result.status(), 200);
    let sent = low.seen.lock().unwrap().last().unwrap().body.clone();
    assert!(
        sent.to_string().contains("review-tool-result"),
        "tool result missing after failover: {sent}"
    );
}
