//! 参数错误只属于本次请求，不能变成跨请求的账号/模型禁令。
mod common;

use akhub::domain::Protocol;
use common::{Behavior, FakeUpstream, TargetSpec, messages, spawn_akhub, wire_target};
use serde_json::{Value, json};

const MODEL: &str = "claude-opus-5-5";

fn body(choice: Value) -> Value {
    json!({
        "model": MODEL,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "Use get_weather for Paris."}],
        "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}],
        "tool_choice": choice,
        "metadata": {"user_id": "regression"},
    })
}

#[tokio::test]
async fn repeated_parameter_errors_never_become_a_local_503() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "kiro",
            &up.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            MODEL,
            100,
        ),
    )
    .await;
    let error = json!({"error": {
        "type": "invalid_request_error",
        "message": "claude-opus-5-5 does not support forced tool_choice; use auto or none"
    }});
    up.fallback(Behavior::Json(400, error.clone()));
    let payload = body(json!({"type": "any"}));
    for _ in 0..4 {
        let response = messages(&akhub, payload.clone()).await;
        assert_eq!(response.status(), 400, "上游参数错误不能被本地屏蔽改成 503");
        let public = response.json::<Value>().await.unwrap();
        assert_eq!(public["error"]["type"], "invalid_request_error");
        assert_eq!(public["akhub_error_code"], "unsupported_parameter");
        assert!(public["request_id"].is_string());
        assert!(!public.to_string().contains("claude-opus-5-5"));
    }
    assert_eq!(up.requests(), 4, "每一次请求都必须到达上游");
    for seen in up.seen.lock().unwrap().iter() {
        assert_eq!(seen.body, payload, "不能擅自把强制工具选择改成 auto");
    }
}

#[tokio::test]
async fn upstream_recovery_is_effective_on_the_next_identical_request() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "kiro",
            &up.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            MODEL,
            100,
        ),
    )
    .await;
    let error = json!({"error": {"message": "tool use is not supported with this request"}});
    up.script([
        Behavior::Json(400, error.clone()),
        Behavior::Json(400, error),
    ]);
    let payload = body(json!({"type": "auto"}));
    for _ in 0..2 {
        assert_eq!(messages(&akhub, payload.clone()).await.status(), 400);
    }
    assert_eq!(
        messages(&akhub, payload.clone()).await.status(),
        200,
        "上游恢复后下一次请求必须立即恢复，不能等待屏蔽到期"
    );
    assert_eq!(up.requests(), 3);
    assert_eq!(up.seen.lock().unwrap().last().unwrap().body, payload);
}

#[tokio::test]
async fn a_request_error_does_not_block_other_request_shapes_or_streams() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "kiro",
            &up.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            MODEL,
            100,
        ),
    )
    .await;
    let error = json!({"error": {"message": "tool use is not supported with this request"}});
    up.script([
        Behavior::Json(400, error.clone()),
        Behavior::Json(400, error),
    ]);
    for _ in 0..2 {
        assert_eq!(
            messages(&akhub, body(json!({"type": "any"})))
                .await
                .status(),
            400
        );
    }
    for streaming in [false, true] {
        let mut payload = body(json!({"type": "auto"}));
        payload["stream"] = json!(streaming);
        let response = messages(&akhub, payload.clone()).await;
        assert_eq!(response.status(), 200, "其他工具请求不能被连坐");
        let bytes = response.bytes().await.unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(up.seen.lock().unwrap().last().unwrap().body, payload);
    }
    assert_eq!(up.requests(), 4);
}
