//! 小版本回归：异常上游不能中断故障切换。
mod common;

use akhub::domain::Protocol;
use common::{Behavior, FakeUpstream, TargetSpec, chat, spawn_akhub, wire_target};
use serde_json::json;

#[tokio::test]
async fn concurrent_requests_spread_within_the_top_layer_only() {
    let hub = spawn_akhub().await;
    let a = FakeUpstream::spawn().await;
    let b = FakeUpstream::spawn().await;
    let fallback = FakeUpstream::spawn().await;
    for (name, up, priority) in [("a", &a, 100), ("b", &b, 100), ("fallback", &fallback, 50)] {
        wire_target(
            &hub,
            TargetSpec::new(
                name,
                &up.base_url,
                Protocol::OpenAiChat,
                "review",
                "review",
                priority,
            ),
        )
        .await;
    }
    futures::future::join_all((0..100).map(|i| {
        let hub = &hub;
        async move {
            let response = chat(hub, json!({"model":"review", "messages":[{"role":"user", "content":format!("request {i}")}]})).await;
            assert_eq!(response.status(), 200);
            response.bytes().await.unwrap();
        }
    })).await;
    assert_eq!(a.requests() + b.requests(), 100);
    assert!(
        a.requests() > 0 && b.requests() > 0,
        "同层两个健康账号都必须获得流量"
    );
    assert_eq!(fallback.requests(), 0, "并发请求不能越过健康的最高优先级层");
}

#[tokio::test]
async fn overflowing_retry_after_does_not_abort_failover() {
    for status in [401, 402, 429, 503] {
        let bad = FakeUpstream::spawn().await;
        let good = FakeUpstream::spawn().await;
        let hub = spawn_akhub().await;
        bad.fallback(Behavior::Status(status, Some(u64::MAX)));
        wire_target(
            &hub,
            TargetSpec::new(
                "bad",
                &bad.base_url,
                Protocol::OpenAiChat,
                "review",
                "review",
                100,
            ),
        )
        .await;
        wire_target(
            &hub,
            TargetSpec::new(
                "good",
                &good.base_url,
                Protocol::OpenAiChat,
                "review",
                "review",
                50,
            ),
        )
        .await;
        let response = chat(&hub, json!({"model":"review", "messages":[]})).await;
        assert_eq!(
            response.status(),
            200,
            "{status} 的异常等待时间不能让网关崩溃"
        );
        response.bytes().await.unwrap();
        assert_eq!(good.requests(), 1);
    }
}
