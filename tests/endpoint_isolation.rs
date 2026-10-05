//! 一个模型的 404/405 不能封掉同账号其他模型的原生接口或共用 Key。
mod common;

use akhub::domain::Protocol;
use common::{
    Behavior, FakeUpstream, TargetSpec, responses, spawn_akhub, wire_extra_target, wire_target,
};
use serde_json::{Value, json};

const BAD: &str = "gpt-6-luna";
const GOOD: &str = "gpt-6.1-sol";

fn native_only(model: &str, streaming: bool) -> Value {
    // 自定义工具没有 Chat 等价物，不能为绕过错误而删除或改写工具。
    json!({
        "model": model, "input": "hi", "stream": streaming,
        "tools": [{"type": "custom", "name": "exec", "format": {"type": "text"}}],
    })
}

#[tokio::test]
async fn one_model_rejection_never_blocks_another_models_native_stream() {
    for status in [404, 405] {
        let up = FakeUpstream::spawn().await;
        let hub = spawn_akhub().await;
        let wired = wire_target(
            &hub,
            TargetSpec::new("A", &up.base_url, Protocol::OpenAiChat, BAD, BAD, 50),
        )
        .await;
        wire_extra_target(&hub, &wired.account_id, GOOD, GOOD).await;

        let warm = responses(&hub, native_only(GOOD, false)).await;
        assert_eq!(warm.status(), 200);
        warm.bytes().await.unwrap();
        up.script([Behavior::Status(status, None)]);
        let rejected = responses(&hub, native_only(BAD, false)).await;
        assert!(rejected.status().is_client_error() || rejected.status().is_server_error());
        rejected.bytes().await.unwrap();

        let payload = native_only(GOOD, true);
        let result = responses(&hub, payload.clone()).await;
        assert_eq!(
            result.status(),
            200,
            "另一模型的 {status} 不能连带屏蔽 Responses"
        );
        let stream = result.text().await.unwrap();
        assert!(
            stream.contains("response.completed"),
            "必须完整送达流式结束事件"
        );
        let seen = up.seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "不能转换或重放原生专用请求");
        assert!(seen.iter().all(|r| r.path == "/v1/responses"));
        assert_eq!(seen.last().unwrap().body, payload);
    }
}

#[tokio::test]
async fn repeated_route_rejections_never_cool_the_shared_account_or_key() {
    for preferred in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
        for status in [404, 405] {
            let up = FakeUpstream::spawn().await;
            let hub = spawn_akhub().await;
            let wired = wire_target(
                &hub,
                TargetSpec::new("A", &up.base_url, preferred, BAD, BAD, 50),
            )
            .await;
            wire_extra_target(&hub, &wired.account_id, GOOD, GOOD).await;
            for attempt in 1..=6 {
                up.script([Behavior::Status(status, None)]);
                let result = responses(&hub, native_only(BAD, false)).await;
                assert!(result.status().is_client_error() || result.status().is_server_error());
                result.bytes().await.unwrap();
                assert_eq!(
                    up.requests(),
                    attempt,
                    "{status} 不能导致本地跳过下一次调用"
                );
            }
            let response = responses(&hub, native_only(GOOD, false)).await;
            assert_eq!(
                response.status(),
                200,
                "模型/接口错误不能熔断共用账号或 Key"
            );
            response.bytes().await.unwrap();
            assert_eq!(up.requests(), 7);
        }
    }
}

#[tokio::test]
async fn conversion_is_local_to_the_request_and_each_endpoint_is_tried_once() {
    for status in [404, 405] {
        let up = FakeUpstream::spawn().await;
        let hub = spawn_akhub().await;
        wire_target(
            &hub,
            TargetSpec::new("A", &up.base_url, Protocol::OpenAiChat, GOOD, GOOD, 50),
        )
        .await;
        let payload = json!({"model": GOOD, "input": "hi"});
        up.script([Behavior::Status(status, None)]);
        let converted = responses(&hub, payload.clone()).await;
        assert_eq!(converted.status(), 200, "仍应尝试本次请求的可用转换端点");
        converted.bytes().await.unwrap();
        let recovered = responses(&hub, payload.clone()).await;
        assert_eq!(recovered.status(), 200);
        recovered.bytes().await.unwrap();
        let seen = up.seen.lock().unwrap();
        let paths: Vec<_> = seen.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            ["/v1/responses", "/v1/chat/completions", "/v1/responses"]
        );
        assert_eq!(seen.last().unwrap().body, payload);
    }
}

#[tokio::test]
async fn exhausted_endpoints_keep_the_real_error_and_do_not_loop() {
    for status in [404, 405] {
        let up = FakeUpstream::spawn().await;
        let hub = spawn_akhub().await;
        wire_target(
            &hub,
            TargetSpec::new("A", &up.base_url, Protocol::OpenAiChat, BAD, BAD, 50),
        )
        .await;
        up.fallback(Behavior::Status(status, None));
        let response = responses(&hub, native_only(BAD, false)).await;
        let error: Value = response.json().await.unwrap();
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(&status.to_string()),
            "不能把原始状态码丢成泛化的无可用端点：{error}"
        );
        assert_eq!(up.requests(), 1, "无法转换时不应改写原生请求");

        // 可转换请求遇到所有端点都被拒绝，也必须有界结束。
        let response = responses(&hub, json!({"model": BAD, "input": "hi"})).await;
        assert!(response.status().is_client_error() || response.status().is_server_error());
        response.bytes().await.unwrap();
        let seen = up.seen.lock().unwrap();
        let paths: Vec<_> = seen[1..].iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["/v1/responses", "/v1/chat/completions"]);
    }
}

#[tokio::test]
async fn a_model_rejection_still_switches_to_another_target_without_changing_tools() {
    let first = FakeUpstream::spawn().await;
    let second = FakeUpstream::spawn().await;
    let hub = spawn_akhub().await;
    wire_target(
        &hub,
        TargetSpec::new(
            "first",
            &first.base_url,
            Protocol::OpenAiChat,
            GOOD,
            GOOD,
            100,
        ),
    )
    .await;
    wire_target(
        &hub,
        TargetSpec::new(
            "second",
            &second.base_url,
            Protocol::OpenAiResponses,
            GOOD,
            GOOD,
            50,
        ),
    )
    .await;
    first.fallback(Behavior::Json(
        404,
        json!({"error": {"message": "model not found"}}),
    ));
    let payload = native_only(GOOD, true);
    let response = responses(&hub, payload.clone()).await;
    assert_eq!(response.status(), 200);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("response.completed")
    );
    assert_eq!(first.requests(), 1);
    let seen = second.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/responses");
    assert_eq!(seen[0].body, payload);
}
