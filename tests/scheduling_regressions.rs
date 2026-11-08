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

/// 429 + 短 `Retry-After` 的冷却是"忙"不是"坏"：同一会话的下一个请求必须
/// 等过这段冷却、留在原目标上，而不是清掉强绑定换号（§10.2、§10.3）。
#[tokio::test]
async fn a_short_retry_after_cooldown_keeps_the_strong_binding() {
    use akhub::health::{Caller, Outcome};

    let first = FakeUpstream::spawn().await;
    let second = FakeUpstream::spawn().await;
    let akhub = spawn_akhub_with(Default::default(), |_| {}).await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("a", &first.base_url, Protocol::OpenAiChat, "m", "m", 50),
    )
    .await;
    let b = wire_target(
        &akhub,
        TargetSpec::new("b", &second.base_url, Protocol::OpenAiChat, "m", "m", 50),
    )
    .await;

    let mut body = common::bulky_chat_body("sys", "hello");
    body["model"] = json!("m");
    body["prompt_cache_key"] = json!("session-429");
    assert_eq!(chat(&akhub, body.clone()).await.status(), 200);
    let (bound, bound_upstream) = if first.requests() == 1 {
        (&a, &first)
    } else {
        (&b, &second)
    };

    akhub
        .state
        .runtime
        .health
        .try_admit(
            Caller {
                account_id: &bound.account_id,
                key_id: None,
                target_id: &bound.target_id,
            },
            Limits::default(),
            0,
        )
        .unwrap()
        .settle(
            Outcome::RateLimited {
                retry_after: Some(Duration::from_secs(1)),
            },
            None,
        );

    let started = Instant::now();
    assert_eq!(chat(&akhub, body).await.status(), 200);
    assert_eq!(
        bound_upstream.requests(),
        2,
        "短冷却期间同一会话必须等原目标，而不是换号"
    );
    assert_eq!(first.requests() + second.requests(), 2);
    assert!(started.elapsed() >= Duration::from_millis(900));
}

/// 响应链在同层权重相同时必须一直落在原账号上，走原生续链（§15.2）。
///
/// 粘性键取自每轮都变的 `previous_response_id`，以前从不命中，每轮随机落号。
#[tokio::test]
async fn a_responses_chain_stays_on_its_account_with_equal_priorities() {
    let first = FakeUpstream::spawn().await;
    let second = FakeUpstream::spawn().await;
    let akhub = spawn_akhub_with(Default::default(), |_| {}).await;
    for (name, upstream) in [("a", &first), ("b", &second)] {
        wire_target(
            &akhub,
            TargetSpec::new(
                name,
                &upstream.base_url,
                Protocol::OpenAiResponses,
                "m",
                "m",
                0,
            )
            .pinned(),
        )
        .await;
    }

    let mut previous: Option<String> = None;
    for turn in 0..8 {
        let mut body = json!({"model":"m","input":format!("第 {turn} 轮")});
        if let Some(id) = &previous {
            body["previous_response_id"] = json!(id);
        }
        let response = common::responses(&akhub, body).await;
        assert_eq!(response.status(), 200, "第 {turn} 轮");
        let body: Value = response.json().await.unwrap();
        previous = Some(body["id"].as_str().unwrap().to_string());
    }
    let (a, b) = (first.requests(), second.requests());
    assert!(a == 8 || b == 8, "整条链必须落在同一个账号上：a={a} b={b}");
    let seen = if a == 8 { &first } else { &second };
    let last = seen.seen.lock().unwrap().last().unwrap().body.clone();
    assert!(
        last.get("previous_response_id").is_some(),
        "后续轮次必须走原生续链：{last}"
    );
}
