//! 已修复缺陷的定向回归：每一项都对应一次真实复现过的问题。
//!
//! - 账号最大并发必须跨模型共享：一个模型占满的并发槽，另一个模型也要排队。
//! - 低优先级粘性绑定不能绕过已经恢复的高优先级层。
//! - TPM 结算必须把输入 Token 算进去。
//! - Responses 故障切换不能丢掉工具调用结果。
//! - Responses 的 `response.failed` 是失败：产生内容前要换号，之后要记进
//!   `error_code`（现场：可达鸭的 gpt-6.1-sol 在 New API 里显示"异常 (eof)"）。
//!
//! 这几条曾经是审计里复现出来的缺陷，修好之后作为回归用例留在这里，
//! 防止后续重构把它们悄悄改回去。
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

/// 现场回归：上游（可达鸭的 gpt-6.1-sol）HTTP 200、等到最后才回
/// `response.failed`，此前**一个语义块都没有**，也没有 usage。
///
/// 旧逻辑把 `response.failed` 判成"已经出现语义内容"：既不换号、也不计失败，
/// 坏答案被原样端给下游——New API 里看到的就是一条"流状态 ✗ 异常 (eof)"，
/// 而账号的可靠性分毫发无损，流量继续往它身上压。
#[tokio::test]
async fn a_failed_responses_stream_switches_to_the_next_target() {
    let bad = FakeUpstream::spawn().await;
    let good = FakeUpstream::spawn().await;
    bad.fallback(Behavior::StreamFailedResponse);
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "坏",
            &bad.base_url,
            Protocol::OpenAiResponses,
            "m",
            "m",
            100,
        ),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "好",
            &good.base_url,
            Protocol::OpenAiResponses,
            "m",
            "m",
            50,
        ),
    )
    .await;

    let response = responses(&akhub, json!({"model":"m","input":"hi","stream":true})).await;
    assert_eq!(
        response.status(),
        200,
        "产生内容之前就失败的流必须换号，而不是把失败原样端出去"
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("response.completed"),
        "下游必须拿到好账号的完整回答：{body}"
    );
    assert_eq!(bad.requests(), 1, "坏账号只该被尝试一次");
    assert_eq!(good.requests(), 1, "换号之后必须落到下一个候选");
}

/// 已经在写正文之后才失败：换不了号（字节早就发出去了），但这条流必须记成失败。
///
/// 记录里的 `error_code` 是账号扣分的依据：没有它，这种"200 的失败流"会被
/// 结算成一次成功，坏账号永远排在前面。
#[tokio::test]
async fn a_failed_responses_stream_after_content_is_recorded_as_a_fault() {
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::StreamFailedAfterContent);
    let akhub = spawn_akhub().await;
    let target = wire_target(
        &akhub,
        TargetSpec::new(
            "半途",
            &upstream.base_url,
            Protocol::OpenAiResponses,
            "m",
            "m",
            100,
        ),
    )
    .await;

    let response = responses(&akhub, json!({"model":"m","input":"hi","stream":true})).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("response.failed"),
        "已经开始输出之后，失败收尾只能照实转发：{body}"
    );

    let record = wait_for_record(&akhub, &target.target_id).await;
    assert_eq!(
        record.error_code.as_deref(),
        Some("upstream_protocol_error"),
        "失败收尾必须写进 error_code，否则这笔会被记成成功"
    );
}

/// 结算发生在流结束之后，记录是异步落库的：轮询等它出现。
async fn wait_for_record(
    akhub: &common::Akhub,
    target_id: &str,
) -> akhub::storage::store::RequestRecord {
    for _ in 0..200 {
        if let Ok(records) = akhub.state.store.list_request_records(50, 0).await
            && let Some(record) = records
                .into_iter()
                .find(|record| record.target_id.as_deref() == Some(target_id))
        {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("请求记录没有落库");
}
