//! 阶段 3 验收：六个方向的跨协议转换、能力降级白名单与端点选择
//! （§26.1、§14.3、§14.8）。
//!
//! 全部通过真实 HTTP 打到假上游：请求经过完整的鉴权、资格过滤、端点选择、
//! 中间格式转换、上游调用与响应回译。

mod common;

use akhub::domain::Protocol;
use akhub::health::Caller;
use common::{
    Behavior, FakeUpstream, TargetSpec, chat, messages, request, responses, spawn_akhub,
    spawn_akhub_with, wire_target,
};
use serde_json::{Value, json};

const ALL: [Protocol; 3] = [
    Protocol::OpenAiChat,
    Protocol::OpenAiResponses,
    Protocol::AnthropicMessages,
];

const MODEL: &str = "m1";

fn name_of(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiChat => "chat",
        Protocol::OpenAiResponses => "responses",
        Protocol::AnthropicMessages => "messages",
    }
}

/// 一个带 system prompt、工具定义与图片的请求，按下游协议给出。
fn rich_body(protocol: Protocol, stream: bool) -> Value {
    let mut body = match protocol {
        Protocol::OpenAiChat => json!({
            "model": MODEL,
            "messages": [
                {"role": "system", "content": "你是助手"},
                {"role": "user", "content": [
                    {"type": "text", "text": "北京天气如何"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]}
            ],
            "tools": [{"type": "function", "function": {
                "name": "weather", "description": "查天气",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }}],
            "max_tokens": 256
        }),
        Protocol::OpenAiResponses => json!({
            "model": MODEL,
            "instructions": "你是助手",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "北京天气如何"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
            ]}],
            "tools": [{"type": "function", "name": "weather", "description": "查天气",
                       "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}],
            "max_output_tokens": 256
        }),
        Protocol::AnthropicMessages => json!({
            "model": MODEL,
            "max_tokens": 256,
            "system": "你是助手",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "北京天气如何"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]}],
            "tools": [{"name": "weather", "description": "查天气",
                       "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}]
        }),
    };
    if stream && let Some(object) = body.as_object_mut() {
        object.insert("stream".into(), json!(true));
    }
    body
}

/// 下游看到的响应里，工具调用是否完整。
fn asserts_tool_call(protocol: Protocol, body: &str, label: &str) {
    assert!(body.contains("call_1"), "{label}：丢了工具调用 ID\n{body}");
    assert!(body.contains("weather"), "{label}：丢了工具名\n{body}");
    assert!(body.contains("北京"), "{label}：丢了工具参数\n{body}");
    // 停止原因必须映射成"该执行工具了"，否则客户端不会进入下一轮。
    let reason = match protocol {
        Protocol::OpenAiChat => "tool_calls",
        Protocol::OpenAiResponses => "function_call",
        Protocol::AnthropicMessages => "tool_use",
    };
    assert!(body.contains(reason), "{label}：停止原因不对\n{body}");
}

// ------------------------------------------------------ 六个方向：非流式

#[tokio::test]
async fn every_direction_carries_tools_images_and_system_prompts() {
    for upstream_protocol in ALL {
        let upstream = FakeUpstream::spawn().await;
        let akhub = spawn_akhub().await;
        wire_target(
            &akhub,
            // 关掉运行时适配：强制走"下游协议 → 账号首选协议"的转换路径。
            TargetSpec::new(
                "A",
                &upstream.base_url,
                upstream_protocol,
                MODEL,
                "真实模型",
                50,
            )
            .pinned(),
        )
        .await;

        for downstream in ALL {
            let label = format!("{} → {}", name_of(downstream), name_of(upstream_protocol));
            let before = upstream.requests();
            let response = request(&akhub, downstream, rich_body(downstream, false)).await;
            assert_eq!(response.status(), 200, "{label}");
            assert!(
                !response.headers().contains_key("x-akhub-degraded"),
                "{label}：这个请求不该有任何降级"
            );

            // 上游收到的是它自己协议的形状，且内容一件不少。
            let seen = upstream.seen.lock().unwrap().last().unwrap().clone();
            assert_eq!(upstream.requests(), before + 1, "{label}");
            assert!(
                seen.path.contains(match upstream_protocol {
                    Protocol::OpenAiChat => "chat/completions",
                    Protocol::OpenAiResponses => "responses",
                    Protocol::AnthropicMessages => "messages",
                }),
                "{label}：打到了错误的端点 {}",
                seen.path
            );
            let sent = seen.body.to_string();
            assert_eq!(seen.body["model"], "真实模型", "{label}：模型名未改写");
            assert!(sent.contains("你是助手"), "{label}：丢了系统提示\n{sent}");
            assert!(sent.contains("weather"), "{label}：丢了工具定义\n{sent}");
            assert!(sent.contains("AAAA"), "{label}：丢了图片\n{sent}");
            assert!(sent.contains("北京天气如何"), "{label}：丢了用户消息");

            // 下游拿回的是自己协议的形状。
            let body: Value = response.json().await.unwrap();
            let text = body.to_string();
            assert!(text.contains("你好"), "{label}：丢了响应文本\n{text}");
            match downstream {
                Protocol::OpenAiChat => assert_eq!(body["object"], "chat.completion", "{label}"),
                Protocol::OpenAiResponses => assert_eq!(body["object"], "response", "{label}"),
                Protocol::AnthropicMessages => assert_eq!(body["type"], "message", "{label}"),
            }
        }
    }
}

#[tokio::test]
async fn tool_calls_come_back_in_the_downstream_shape_in_every_direction() {
    for upstream_protocol in ALL {
        let upstream = FakeUpstream::spawn().await;
        upstream.fallback(Behavior::ToolCall);
        let akhub = spawn_akhub().await;
        wire_target(
            &akhub,
            TargetSpec::new(
                "A",
                &upstream.base_url,
                upstream_protocol,
                MODEL,
                "真实模型",
                50,
            )
            .pinned(),
        )
        .await;

        for downstream in ALL {
            let label = format!("{} → {}", name_of(downstream), name_of(upstream_protocol));
            let response = request(&akhub, downstream, rich_body(downstream, false)).await;
            assert_eq!(response.status(), 200, "{label}");
            asserts_tool_call(downstream, &response.text().await.unwrap(), &label);
        }
    }
}

// -------------------------------------------------------- 六个方向：流式

#[tokio::test]
async fn streaming_text_survives_every_direction() {
    for upstream_protocol in ALL {
        let upstream = FakeUpstream::spawn().await;
        let akhub = spawn_akhub().await;
        wire_target(
            &akhub,
            TargetSpec::new(
                "A",
                &upstream.base_url,
                upstream_protocol,
                MODEL,
                "真实模型",
                50,
            )
            .pinned(),
        )
        .await;

        for downstream in ALL {
            let label = format!("{} → {}", name_of(downstream), name_of(upstream_protocol));
            let response = request(&akhub, downstream, rich_body(downstream, true)).await;
            assert_eq!(response.status(), 200, "{label}");
            assert_eq!(
                response.headers()["content-type"],
                "text/event-stream",
                "{label}"
            );
            let text = response.text().await.unwrap();
            assert!(text.contains("你好"), "{label}：流式丢了文本\n{text}");

            // 收尾事件必须齐全且**只出现一次**：少了客户端一直等，多了客户端
            // 在第二个收尾事件上报错。
            match downstream {
                Protocol::OpenAiChat => {
                    assert!(text.contains("chat.completion.chunk"), "{label}\n{text}");
                    assert_eq!(
                        text.matches("\"finish_reason\":\"stop\"").count(),
                        1,
                        "{label}\n{text}"
                    );
                    assert_eq!(text.matches("[DONE]").count(), 1, "{label}\n{text}");
                    assert!(text.trim_end().ends_with("data: [DONE]"), "{label}\n{text}");
                }
                Protocol::OpenAiResponses => {
                    assert!(text.contains("event: response.created"), "{label}\n{text}");
                    assert_eq!(
                        text.matches("event: response.completed").count(),
                        1,
                        "{label}\n{text}"
                    );
                }
                Protocol::AnthropicMessages => {
                    assert!(text.contains("event: message_start"), "{label}\n{text}");
                    assert_eq!(
                        text.matches("event: message_stop").count(),
                        1,
                        "{label}\n{text}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn streaming_tool_calls_survive_every_direction() {
    for upstream_protocol in ALL {
        let upstream = FakeUpstream::spawn().await;
        upstream.fallback(Behavior::ToolCall);
        let akhub = spawn_akhub().await;
        wire_target(
            &akhub,
            TargetSpec::new(
                "A",
                &upstream.base_url,
                upstream_protocol,
                MODEL,
                "真实模型",
                50,
            )
            .pinned(),
        )
        .await;

        for downstream in ALL {
            let label = format!("{} → {}", name_of(downstream), name_of(upstream_protocol));
            let response = request(&akhub, downstream, rich_body(downstream, true)).await;
            assert_eq!(response.status(), 200, "{label}");
            asserts_tool_call(downstream, &response.text().await.unwrap(), &label);
        }
    }
}

// ---------------------------------------------------------------- 降级

/// 一个带签名思考历史的 Anthropic 请求：转到别的协议必然丢思考。
fn thinking_body() -> Value {
    json!({
        "model": MODEL,
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "问题"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "内部推理", "signature": "sig-abc"},
                {"type": "text", "text": "上一轮答案"}
            ]},
            {"role": "user", "content": "继续"}
        ],
        "thinking": {"type": "enabled", "budget_tokens": 10000}
    })
}

#[tokio::test]
async fn thinking_degrades_only_after_the_lossless_target_is_exhausted() {
    let native = FakeUpstream::spawn().await;
    let converted = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    // 同一层：一个原生 Messages（无损），一个只能转成 Chat（要丢思考）。
    wire_target(
        &akhub,
        TargetSpec::new(
            "原生",
            &native.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "只能转换",
            &converted.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;

    // 首次选择：无损目标独占流量，即使抽签本该给另一个（§14.8）。
    for _ in 0..6 {
        let response = messages(&akhub, thinking_body()).await;
        assert_eq!(response.status(), 200);
        assert!(
            !response.headers().contains_key("x-akhub-degraded"),
            "无损目标可用时不该发生降级"
        );
    }
    assert_eq!(native.requests(), 6);
    assert_eq!(converted.requests(), 0, "降级目标在首次选择中拿不到流量");

    // 无损目标失败：这才是"故障切换"，此时允许丢弃白名单内的能力。
    native.script([Behavior::Status(500, None)]);
    let response = messages(&akhub, thinking_body()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-akhub-degraded"],
        "thinking",
        "降级必须显式标记（§14.7）"
    );
    assert_eq!(converted.requests(), 1);

    // 上游收到的请求里没有思考块，但"要思考"的意图与其余内容一件不少。
    let seen = converted.seen.lock().unwrap().last().unwrap().clone();
    let sent = seen.body.to_string();
    assert!(!sent.contains("sig-abc"), "签名无处安放，必须丢弃\n{sent}");
    assert!(!sent.contains("内部推理"), "{sent}");
    assert!(
        sent.contains("上一轮答案"),
        "答案不是思考，绝不能一起丢\n{sent}"
    );
    assert_eq!(seen.body["reasoning_effort"], "high", "思考强度必须传下去");

    // 响应体本身不被修改去掩盖降级（§14.8）。
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert!(body.to_string().contains("你好"));
}

/// 无损目标只是忙时不能绕过它去用降级目标（§14.8）。
///
/// **这个用例的形状**：两个同名模型分属两个账号，一个走原生 Messages、一个只能
/// 跨协议转换。它们同层，所以"谁进计划"本来由抽签决定。
///
/// 关键在于**两个账号都要被占住**：只占一个的话，抽到另一个时请求会被正常服务，
/// 用例就会随抽签结果飘。占位用同一个 `Limits`（并发 1）打在两个账号上，于是
/// 无论抽到谁，无损候选都是"忙"，必须在本层等待而不能下沉到降级候选。
#[tokio::test]
async fn a_busy_lossless_target_is_not_bypassed_by_degraded_traffic() {
    let native = FakeUpstream::spawn().await;
    let converted = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let native_target = wire_target(
        &akhub,
        TargetSpec::new(
            "原生",
            &native.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "up",
            50,
        )
        .pinned()
        .limits(akhub::domain::Limits {
            max_concurrency: Some(1),
            ..Default::default()
        }),
    )
    .await;
    // 计划里活下来的是**最后**接线的那个账号：同名逻辑模型的两个目标属于不同
    // 账号，占位必须占在真正参与调度的那个上（§9.1）。
    let planned = wire_target(
        &akhub,
        TargetSpec::new(
            "只能转换",
            &converted.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;

    // 占住**两个**候选账号的唯一并发名额：无损候选有两个，抽签会选其中之一，
    // 只占一个的话用例会随抽签结果飘。
    //
    // 容量是在准入路径上校准的：先"准入后立刻取消"把每个账号校准到 1，再真正
    // 占住名额。
    let one_slot = akhub::domain::Limits {
        max_concurrency: Some(1),
        ..Default::default()
    };
    let mut held = Vec::new();
    for wired in [&native_target, &planned] {
        let caller = Caller {
            account_id: &wired.account_id,
            key_id: None,
            target_id: &wired.target_id,
        };
        akhub
            .state
            .runtime
            .health
            .try_admit(caller, one_slot, 0)
            .unwrap()
            .cancel_before_upstream();
        held.push(
            akhub
                .state
                .runtime
                .health
                .try_admit(caller, one_slot, 0)
                .unwrap(),
        );
    }
    let waiting = tokio::spawn(messages(&akhub, thinking_body()));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!waiting.is_finished(), "无损目标只是忙时必须在本层等待");
    assert_eq!(converted.requests(), 0, "不能绕过忙的无损目标直接降级");

    drop(held);
    assert_eq!(waiting.await.unwrap().status(), 200);
    assert_eq!(native.requests(), 1);
    assert_eq!(converted.requests(), 0);
}

#[tokio::test]
async fn a_group_that_forbids_degradation_refuses_instead_of_dropping_thinking() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub_with(Default::default(), |group| group.allow_degrade = false).await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "只能转换",
            &upstream.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;

    let response = messages(&akhub, thinking_body()).await;
    assert_eq!(response.status(), 400);
    assert_eq!(upstream.requests(), 0, "宁可不发，也不静默丢能力");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");

    // 不带思考历史的普通请求照常无损通过。
    let plain = json!({
        "model": MODEL, "max_tokens": 64,
        "messages": [{"role": "user", "content": "你好"}]
    });
    assert_eq!(messages(&akhub, plain).await.status(), 200);
    assert_eq!(upstream.requests(), 1);
}

#[tokio::test]
async fn whitelist_external_capabilities_fail_fast_instead_of_being_dropped() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "只有 Chat",
            &upstream.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;

    // 结构化输出 Schema：丢了客户端会解析崩溃，不在白名单内（§14.8）。
    let structured = json!({
        "model": MODEL, "max_tokens": 64,
        "messages": [{"role": "user", "content": "给我 JSON"}],
        "response_format": {"type": "json_schema", "json_schema": {
            "name": "out", "strict": true, "schema": {"type": "object"}
        }}
    });
    let response = messages(&akhub, structured).await;
    assert_eq!(response.status(), 400, "白名单外的能力必须报错");
    assert_eq!(upstream.requests(), 0);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["akhub_error_code"], "unsupported_parameter");
    assert!(
        !body["error"]["message"].as_str().unwrap().is_empty(),
        "错误必须说清楚是哪一项表达不了"
    );
}

#[tokio::test]
async fn unknown_fields_pass_through_natively_but_are_refused_across_protocols() {
    let native = FakeUpstream::spawn().await;
    let foreign = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "原生",
            &native.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;

    let body = json!({
        "model": MODEL, "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "供应商私有字段": {"保留": true}
    });

    // 同协议：原样透传，一个字段都不动（§14.1）。
    assert_eq!(messages(&akhub, body.clone()).await.status(), 200);
    let seen = native.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(seen.body["供应商私有字段"]["保留"], true);

    // 跨协议：明确拒绝，绝不静默删除（§14.6）。
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "只有 Chat",
            &foreign.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;
    let response = messages(&akhub, body).await;
    assert_eq!(response.status(), 400);
    assert_eq!(foreign.requests(), 0);
    let error: Value = response.json().await.unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("供应商私有字段"),
        "错误必须指出是哪个字段：{error}"
    );
}

// ------------------------------------------------------------ 端点选择

#[tokio::test]
async fn a_rejected_native_endpoint_falls_back_only_for_the_current_request() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    // 账号首选 Chat，但打开了运行时适配：先试上游可能有的 /v1/messages。
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "up",
            50,
        ),
    )
    .await;
    // 这台假上游没有 /v1/messages 之外的路由问题：让它对第一次调用报 404。
    upstream.script([Behavior::Status(404, None)]);

    let body = json!({
        "model": MODEL, "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let response = messages(&akhub, body.clone()).await;
    assert_eq!(response.status(), 200, "404 之后应当改走转换端点");

    let paths: Vec<String> = upstream
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|seen| seen.path.clone())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/v1/messages".to_string(),
            "/v1/chat/completions".to_string()
        ],
        "先试原生端点，证实不存在后才转换（§14.2）"
    );

    // 下一个请求重新从原生端点开始，不能沿用上一次的拒绝。
    assert_eq!(messages(&akhub, body).await.status(), 200);
    let paths: Vec<String> = upstream
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|seen| seen.path.clone())
        .collect();
    assert_eq!(paths.len(), 3);
    assert_eq!(paths[2], "/v1/messages");
}

#[tokio::test]
async fn a_missing_endpoint_does_not_count_as_a_target_fault() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "up",
            50,
        ),
    )
    .await;

    // 连续多次原生 404 都只是"走错门"，不该把账号熔断（§16.7）。
    for _ in 0..6 {
        upstream.script([Behavior::Status(404, None)]);
        let body = json!({
            "model": MODEL, "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert_eq!(messages(&akhub, body).await.status(), 200);
    }
    assert!(
        akhub
            .state
            .runtime
            .health
            .check(
                Caller {
                    account_id: &wired.account_id,
                    key_id: None,
                    target_id: &wired.target_id
                },
                akhub::domain::Limits::default()
            )
            .is_ok(),
        "端点不存在不是这个目标的故障"
    );
}

#[tokio::test]
async fn count_tokens_is_refused_rather_than_estimated_when_it_cannot_be_forwarded() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    // 首选 Chat 且关掉适配：没有任何办法原生转发 count_tokens。
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;

    let response = common::client()
        .post(format!("{}/v1/messages/count_tokens", akhub.base_url))
        .header("x-api-key", &akhub.key)
        .json(&json!({"model": MODEL, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400, "不返回估算值冒充精确值（§15.5）");
    assert_eq!(upstream.requests(), 0);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["akhub_error_code"], "unsupported_parameter");
}

// ------------------------------------------------------ 流式边界与记录

#[tokio::test]
async fn the_switch_boundary_still_holds_across_protocols() {
    let a = FakeUpstream::spawn().await;
    let b = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    // 两个只有 Chat 端点的账号，下游用 Anthropic 进来：全程跨协议。
    wire_target(
        &akhub,
        TargetSpec::new("A", &a.base_url, Protocol::OpenAiChat, MODEL, "up", 100).pinned(),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("B", &b.base_url, Protocol::OpenAiChat, MODEL, "up", 50).pinned(),
    )
    .await;
    let body = json!({
        "model": MODEL, "stream": true, "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}]
    });

    // 语义内容之前的错误事件：还没花钱，换目标（§13.4）。
    a.script([Behavior::StreamErrorEvent]);
    let response = messages(&akhub, body.clone()).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("你好"), "{text}");
    assert!(!text.contains("Overloaded"), "错误不该泄漏给客户端：{text}");
    assert_eq!((a.requests(), b.requests()), (1, 1));

    // 已经送出语义增量后中断：禁止拼接第二个上游，客户端看到截断的流。
    a.script([Behavior::StreamThenAbort]);
    let response = messages(&akhub, body).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap_or_default();
    assert!(!text.contains("message_stop"), "不得伪造正常完成：{text}");
    assert_eq!(b.requests(), 1, "第一个语义事件之后不得切换");
}

#[tokio::test]
async fn request_records_capture_the_endpoint_and_the_degradation() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "只能转换",
            &upstream.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;

    assert_eq!(messages(&akhub, thinking_body()).await.status(), 200);

    let mut records = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        records = akhub.state.store.list_request_records(10, 0).await.unwrap();
        if !records.is_empty() {
            break;
        }
    }
    assert_eq!(records.len(), 1);
    let record = &records[0];
    // 下游走的是 Messages，实际打到的是 Chat：两者都要看得见（§6.6）。
    assert_eq!(record.protocol, Protocol::AnthropicMessages);
    assert_eq!(record.endpoint.as_deref(), Some("chat_completions"));
    assert_eq!(record.degraded.as_deref(), Some("thinking"));
    assert!(
        !format!("{record:?}").contains("内部推理"),
        "正文绝不能进入元数据"
    );
}

#[tokio::test]
async fn a_native_target_never_pays_for_conversion() {
    // 同协议路径必须原样透传：未知字段、供应商扩展全部保留，一个都不动。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            MODEL,
            "真实模型",
            50,
        ),
    )
    .await;

    let body = json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": "hi"}],
        "seed": 42,
        "logit_bias": {"123": -100},
        "厂商扩展": {"任意": [1, 2, 3]},
    });
    assert_eq!(chat(&akhub, body.clone()).await.status(), 200);

    let seen = upstream.seen.lock().unwrap().last().unwrap().clone();
    let mut expected = body;
    expected["model"] = json!("真实模型");
    assert_eq!(seen.body, expected, "同协议只改模型名，其余字节原样送达");
}

#[tokio::test]
async fn responses_entry_reaches_a_messages_only_upstream() {
    // Codex CLI 走 Responses，上游只有 Anthropic：这条路必须通。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-up",
            50,
        )
        .pinned(),
    )
    .await;

    let response = responses(
        &akhub,
        json!({
            "model": MODEL,
            "instructions": "你是助手",
            "input": "北京天气如何",
            "reasoning": {"effort": "high"},
            "max_output_tokens": 512
        }),
    )
    .await;
    assert_eq!(response.status(), 200);

    let seen = upstream.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.body["system"][0]["text"], "你是助手");
    assert_eq!(seen.body["thinking"]["type"], "enabled");
    // 思考预算必须小于 max_tokens，否则 Anthropic 直接 400。
    let budget = seen.body["thinking"]["budget_tokens"].as_u64().unwrap();
    assert!(seen.body["max_tokens"].as_u64().unwrap() > budget);

    let body: Value = response.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"][0]["content"][0]["text"], "你好");
    assert_eq!(body["usage"]["input_tokens"], 10);
}

#[tokio::test]
async fn a_modern_messages_upstream_gets_the_adaptive_contract_even_behind_an_alias() {
    // 逻辑模型名只是个别名（"m1"），真上游是 5.x。发射时按逻辑名猜出来的是手工
    // 形状，必须在上游模型名对齐后改成自适应形状——否则 5.x 直接 400：
    // "claude-opus-5-5 requires adaptive thinking; omit thinking or use
    //  thinking.type=adaptive and output_config.effort"。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        )
        .pinned(),
    )
    .await;

    let response = responses(
        &akhub,
        json!({
            "model": MODEL,
            "input": "北京天气如何",
            "reasoning": {"effort": "xhigh"},
            "max_output_tokens": 512
        }),
    )
    .await;
    assert_eq!(response.status(), 200);

    let seen = upstream.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.body["model"], "claude-opus-5-5");
    assert_eq!(seen.body["thinking"]["type"], "adaptive");
    assert!(
        seen.body["thinking"].get("budget_tokens").is_none(),
        "新契约不接受手工预算：\n{}",
        seen.body
    );
    // 跨协议时替客户端要摘要，否则 4.7 起思考文本一个字都拿不到。
    assert_eq!(seen.body["thinking"]["display"], "summarized");
    assert_eq!(seen.body["output_config"]["effort"], "xhigh");
}

#[tokio::test]
async fn a_legacy_messages_upstream_is_not_sent_the_adaptive_shape() {
    // 反过来的别名：逻辑名看起来是新模型，真上游是 4.5。自适应形状与
    // output_config 对老模型都是 400，必须还原成手工预算。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            "claude-opus-5-5",
            "claude-opus-4-5",
            50,
        )
        .pinned(),
    )
    .await;

    let response = responses(
        &akhub,
        json!({
            "model": "claude-opus-5-5",
            "input": "北京天气如何",
            "reasoning": {"effort": "high"},
            "max_output_tokens": 512
        }),
    )
    .await;
    assert_eq!(response.status(), 200);

    let seen = upstream.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(seen.body["model"], "claude-opus-4-5");
    assert_eq!(seen.body["thinking"]["type"], "enabled");
    assert!(
        seen.body.get("output_config").is_none(),
        "老模型不认识 output_config：\n{}",
        seen.body
    );
    let budget = seen.body["thinking"]["budget_tokens"].as_u64().unwrap();
    assert!(seen.body["max_tokens"].as_u64().unwrap() > budget);
}

#[tokio::test]
async fn a_rejected_level_is_first_retried_one_step_higher() {
    // 现场（supeai.top 的 claude-opus-5-5）：形状完全正确、只有档位不被接受，
    // 上游回的还是同一句 "requires adaptive thinking"。中转站实现的档位**不单调**
    // （一个站收 low 却拒 medium），所以"这一档不行"推不出"更高的也不行"。
    //
    // 处置方向是**往上**：客户端要 high 被拒，就试 xhigh。试中就是真的升档，
    // 客户端拿到的比它要的还强。绝不发更低的一档——那是替客户端降智（§14.6）。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "拒 high",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        )
        .pinned(),
    )
    .await;

    let rejected = json!({"error": {
        "message": "claude-opus-5-5 requires adaptive thinking; omit thinking or use thinking.type=adaptive and output_config.effort",
        "type": "invalid_request_error"
    }});
    // 第一次拒（high），第二次放行（xhigh）。
    upstream.script([Behavior::Json(400, rejected)]);

    let response = messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "enabled", "budget_tokens": 16_384}
        }),
    )
    .await;
    assert_eq!(response.status(), 200, "客户端不该看见这个 400");

    let levels = {
        let seen = upstream.seen.lock().unwrap();
        seen.iter()
            .map(|s| {
                s.body["output_config"]["effort"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        levels,
        vec![Some("high".to_string()), Some("xhigh".to_string())],
        "往上试一档，而不是往下退"
    );

    // 事实已经学到：这个账号拒 high、收 xhigh。下一次要 high 就直接发 xhigh。
    let before = upstream.requests();
    assert_eq!(
        messages(
            &akhub,
            json!({
                "model": MODEL,
                "max_tokens": 4_096,
                "messages": [{"role": "user", "content": "hi"}],
                "thinking": {"type": "enabled", "budget_tokens": 16_384}
            }),
        )
        .await
        .status(),
        200
    );
    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), before + 1, "第二次请求只打一次上游");
    assert_eq!(
        seen[before].body["output_config"]["effort"], "xhigh",
        "学到的升档直接生效：{:?}",
        seen[before].body
    );
}

#[tokio::test]
async fn a_rejected_level_with_no_higher_option_omits_the_level_instead_of_lowering_it() {
    // 客户端要 max（已经最高档），这个账号拒 max。没有更高的档可试，那就
    // **不指定档位**，把强度交回上游自己的默认——而不是发一个更低的档位。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "拒 max",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        )
        .pinned(),
    )
    .await;

    let rejected = json!({"error": {
        "message": "claude-opus-5-5 requires adaptive thinking; omit thinking or use thinking.type=adaptive and output_config.effort",
        "type": "invalid_request_error"
    }});
    upstream.script([Behavior::Json(400, rejected)]);

    let response = messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "max"}
        }),
    )
    .await;
    assert_eq!(response.status(), 200, "重发之后应当成功");

    let seen = upstream.seen.lock().unwrap();
    let levels: Vec<Option<&str>> = seen
        .iter()
        .map(|s| s.body["output_config"]["effort"].as_str())
        .collect();
    assert_eq!(
        levels,
        vec![Some("max"), None],
        "第二次不带档位；绝不换成更低的一档"
    );
    // 思考本身照常要，只是不再指定强度。
    assert_eq!(seen[1].body["thinking"]["type"], "adaptive");
}

#[tokio::test]
async fn a_rejected_level_upgrades_to_a_level_this_account_is_known_to_accept() {
    // 升档优于不指定：客户端要 medium、这个账号拒 medium，而 high 已经被
    // 证明可用——那就发 high。宁可给多一点推理，也不替客户端降智（§14.6）。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "拒 medium",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        )
        .pinned(),
    )
    .await;

    let rejected = json!({"error": {
        "message": "claude-opus-5-5 requires adaptive thinking; omit thinking or use thinking.type=adaptive and output_config.effort",
        "type": "invalid_request_error"
    }});

    // 第一次：客户端要 high，上游接受 → 这个账号上"high 可用"成为事实。
    assert_eq!(
        messages(
            &akhub,
            json!({
                "model": MODEL,
                "max_tokens": 4_096,
                "messages": [{"role": "user", "content": "hi"}],
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "high"}
            }),
        )
        .await
        .status(),
        200
    );

    // 第二次：客户端要 medium，上游拒一次。
    upstream.script([Behavior::Json(400, rejected)]);
    assert_eq!(
        messages(
            &akhub,
            json!({
                "model": MODEL,
                "max_tokens": 4_096,
                "messages": [{"role": "user", "content": "hi"}],
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "medium"}
            }),
        )
        .await
        .status(),
        200
    );

    let seen = upstream.seen.lock().unwrap();
    let levels: Vec<Option<&str>> = seen
        .iter()
        .map(|s| s.body["output_config"]["effort"].as_str())
        .collect();
    assert_eq!(
        levels,
        vec![Some("high"), Some("medium"), Some("high")],
        "medium 被拒后升到已确认可用的 high，而不是降档"
    );
}

#[tokio::test]
async fn a_rejected_level_is_bounded_and_reports_the_upstream_error() {
    // 上游什么都拒：往上试探一档、再改成不指定，然后把上游的原始 400 报出去。
    // 不做无界重试，也不偷偷降档去凑一个 200。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "谁都拒",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        )
        .pinned(),
    )
    .await;

    let rejected = json!({"error": {
        "message": "claude-opus-5-5 requires adaptive thinking; omit thinking or use thinking.type=adaptive and output_config.effort",
        "type": "invalid_request_error"
    }});
    upstream.fallback(Behavior::Json(400, rejected));

    let response = messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "enabled", "budget_tokens": 16_384}
        }),
    )
    .await;
    assert_eq!(response.status(), 400);
    let text = response.text().await.unwrap_or_default();
    assert!(text.contains("adaptive thinking"), "原因要如实转达：{text}");
    let levels = {
        let seen = upstream.seen.lock().unwrap();
        seen.iter()
            .map(|s| {
                s.body["output_config"]["effort"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        levels,
        vec![
            Some("high".to_string()),
            Some("xhigh".to_string()),
            Some("max".to_string()),
            None,
        ],
        "逐档往上试到底，最后改成不指定，然后收手"
    );
    // 每一档只试一次：试探结果被记住了，阶梯不会重走。
    let mut tried = levels.iter().flatten().cloned().collect::<Vec<_>>();
    let total = tried.len();
    tried.sort();
    tried.dedup();
    assert_eq!(tried.len(), total, "不重复试同一档：{levels:?}");
    // 全程没有出现过比客户端要的 high 更低的档位。
    for level in levels.iter().flatten() {
        assert!(
            matches!(level.as_str(), "high" | "xhigh" | "max"),
            "绝不降档：{levels:?}"
        );
    }
}

#[tokio::test]
async fn what_one_account_learns_about_a_level_never_leaks_to_another() {
    // 同一个站的两个域名实现不同档位，是本次现场的形态。实测结果必须记在
    // （账号，上游模型）上：一个账号被拒不能顺手改掉另一个账号的发射形状。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let first = wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        )
        .pinned(),
    )
    .await;
    let second = wire_target(
        &akhub,
        TargetSpec::new(
            "B",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        )
        .pinned(),
    )
    .await;

    let rejected = json!({"error": {
        "message": "claude-opus-5-5 requires adaptive thinking; omit thinking or use thinking.type=adaptive and output_config.effort",
        "type": "invalid_request_error"
    }});
    upstream.script([Behavior::Json(400, rejected)]);

    let response = messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "enabled", "budget_tokens": 16_384}
        }),
    )
    .await;
    assert_eq!(response.status(), 200, "改按可用档位重发之后应当成功");

    let now = std::time::Instant::now();
    let evidence = &akhub.state.runtime.evidence;
    // 请求里的 budget_tokens 16384 换算成 High——被拒的就是这一档。
    let effort = akhub::protocol::canonical::Effort::High;
    let plans = [
        evidence
            .effort_facts(&first.account_id, "claude-opus-5-5", now)
            .plan(effort),
        evidence
            .effort_facts(&second.account_id, "claude-opus-5-5", now)
            .plan(effort),
    ];
    assert_eq!(
        plans
            .iter()
            .filter(|plan| **plan == akhub::upstream::evidence::EffortPlan::Send(effort))
            .count(),
        1,
        "只有被拒的那个账号记住了这一档：{plans:?}"
    );
    // 同一个账号的别的模型不受牵连。
    let learned = if plans[0] == akhub::upstream::evidence::EffortPlan::Send(effort) {
        &second.account_id
    } else {
        &first.account_id
    };
    assert_eq!(
        evidence
            .effort_facts(learned, "claude-sonnet-5", now)
            .plan(effort),
        akhub::upstream::evidence::EffortPlan::Send(effort)
    );
}

#[tokio::test]
async fn an_unattributable_400_moves_to_another_channel_instead_of_failing_the_client() {
    // 现场：上游回 "The request could not be processed. Please check the request
    // parameters."——没说是哪个参数。400 发生在开始生成之前、**一个 token 都没
    // 消耗**，所以换渠道重试几乎免费；直接透给客户端则把上游的瞬时抖动变成
    // 用户可见的失败（§13.7）。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    // 三个同优先级目标：前两个拒，第三个放行。
    for name in ["会拒的1", "会拒的2", "正常的"] {
        wire_target(
            &akhub,
            TargetSpec::new(
                name,
                &upstream.base_url,
                Protocol::AnthropicMessages,
                MODEL,
                "claude-opus-5-5",
                50,
            ),
        )
        .await;
    }

    // 前两个目标都拒，第三个放行。
    let opaque = json!({"error": {
        "message": "The request could not be processed. Please check the request parameters.",
        "type": "invalid_request_error"
    }});
    upstream.script([
        Behavior::Json(400, opaque.clone()),
        Behavior::Json(400, opaque),
    ]);

    let response = messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "adaptive"}
        }),
    )
    .await;
    assert_eq!(
        response.status(),
        200,
        "换渠道之后应当成功，而不是把上游的 400 透给客户端"
    );
    // 请求体确实重传了（这就是这条路唯一的代价）。
    assert!(
        upstream.requests() >= 3,
        "应当在别的目标上真的重试过：{}",
        upstream.requests()
    );
}

#[tokio::test]
async fn an_attributable_400_is_reported_immediately_without_retrying() {
    // "prompt is too long" 明确归因到请求本身：换哪个目标都是同一个结果，
    // 重试只会白传一遍请求体。必须立刻如实返回（§13.7）。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        ),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "B",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        ),
    )
    .await;

    let too_long = json!({"error": {
        "message": "prompt is too long: 1520629 tokens > 1000000 maximum",
        "type": "invalid_request_error"
    }});
    upstream.fallback(Behavior::Json(400, too_long));

    let response = messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "adaptive"}
        }),
    )
    .await;
    assert_eq!(response.status(), 400);
    let text = response.text().await.unwrap_or_default();
    assert!(
        text.contains("prompt is too long"),
        "原因要如实转达：{text}"
    );
    assert_eq!(
        upstream.requests(),
        1,
        "可归因的 400 只打一次上游，不换渠道重试"
    );
}

#[tokio::test]
async fn repeated_unattributable_400s_stop_after_a_bounded_number_of_channels() {
    // 全都拒：换有限个目标之后收手，把上游原话交回客户端，不做无界重试。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    for name in ["A", "B", "C", "D"] {
        wire_target(
            &akhub,
            TargetSpec::new(
                name,
                &upstream.base_url,
                Protocol::AnthropicMessages,
                MODEL,
                "claude-opus-5-5",
                50,
            ),
        )
        .await;
    }

    let opaque = json!({"error": {
        "message": "The request could not be processed. Please check the request parameters.",
        "type": "invalid_request_error"
    }});
    upstream.fallback(Behavior::Json(400, opaque));

    let response = messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "adaptive"}
        }),
    )
    .await;
    assert_eq!(response.status(), 400);
    let text = response.text().await.unwrap_or_default();
    assert!(
        text.contains("could not be processed"),
        "要把上游原话交回客户端：{text}"
    );
    // 首次 + 最多 2 次换渠道。
    assert_eq!(
        upstream.requests(),
        3,
        "有界：首次 + MAX_OPAQUE_REJECTION_RETRIES 次"
    );
}

#[tokio::test]
async fn a_later_unrelated_failure_is_not_masked_by_an_earlier_opaque_400() {
    // 复审发现的缺陷：last_opaque 一旦写下就不再清除，于是"先撞一次无法归因的
    // 400、后面又因为别的原因全失败"时，客户端会拿到那个早已过期的 400，
    // 而真正的失败原因（这里是 429 限流）被吞掉——错误码错了，客户端的重试
    // 行为也跟着错。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    for name in ["A", "B", "C"] {
        wire_target(
            &akhub,
            TargetSpec::new(
                name,
                &upstream.base_url,
                Protocol::AnthropicMessages,
                MODEL,
                "claude-opus-5-5",
                50,
            ),
        )
        .await;
    }

    let opaque = json!({"error": {
        "message": "The request could not be processed. Please check the request parameters.",
        "type": "invalid_request_error"
    }});
    // 第一个目标：无法归因的 400（会触发换渠道）。之后的全部 429。
    upstream.script([Behavior::Json(400, opaque)]);
    upstream.fallback(Behavior::Status(429, None));

    let response = messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "adaptive"}
        }),
    )
    .await;
    // 最后的失败是限流，不是那个 400。
    assert_eq!(
        response.status(),
        429,
        "应当报告最后一次真实的失败原因，而不是早已过期的 400"
    );
}

#[tokio::test]
async fn a_shape_this_round_already_sent_is_not_sent_again() {
    // A→B→A 的往复：档位阶梯的每一跳都与上一次不同，"和上一次不同"这条
    // 判据挡不住它。2026-10-15 现场：high → xhigh → max → xhigh，第三次
    // 重发回到一个刚被拒的档位，白撞一次上游（§14.6）。
    //
    // 编排：第三跳（max）先挂住，测试在中途把 xhigh 写成"同账号另一条并发
    // 请求证明可用"——事实库是（账号，上游模型）粒度的，这会把 xhigh 重新
    // 放回候选。收手的地方就是"本轮已经发过的形状不再发"。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "refuse",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        )
        .pinned(),
    )
    .await;

    let rejected = json!({"error": {
        "message": "claude-opus-5-5 requires adaptive thinking; omit thinking or use thinking.type=adaptive and output_config.effort",
        "type": "invalid_request_error"
    }});
    upstream.script([
        Behavior::Json(400, rejected.clone()),
        Behavior::Json(400, rejected.clone()),
        Behavior::HangThenJson(400, rejected.clone()),
    ]);
    upstream.fallback(Behavior::Json(400, rejected.clone()));

    let pending = tokio::spawn(messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "enabled", "budget_tokens": 16_384}
        }),
    ));
    upstream.wait_for_requests(3).await;
    // 同账号上另一条请求用 xhigh 成功了（§14.6 的事实按账号共享）。
    akhub.state.runtime.evidence.note_effort_accepted(
        &wired.account_id,
        "claude-opus-5-5",
        akhub::protocol::canonical::Effort::XHigh,
        std::time::Instant::now(),
    );
    upstream.release();

    let response = pending.await.unwrap();
    assert_eq!(response.status(), 400);
    let text = response.text().await.unwrap_or_default();
    assert!(text.contains("adaptive thinking"), "原因要如实转达：{text}");
    let levels = {
        let seen = upstream.seen.lock().unwrap();
        seen.iter()
            .map(|s| {
                s.body["output_config"]["effort"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        levels,
        vec![
            Some("high".to_string()),
            Some("xhigh".to_string()),
            Some("max".to_string()),
        ],
        "xhigh 本轮已经发过，就不再发第二次：A→B→A 的往复在这里收手"
    );
    assert_eq!(upstream.requests(), 3, "不重发本轮已经发过的形状");
}

#[tokio::test]
async fn an_exhausted_effort_ladder_switches_target() {
    // 阶梯在一个目标上走完仍被拒——连"不指定档位"也拒——就不该把上游的 400
    // 直接丢给客户端：同一个模型名下还有别的账号/渠道（§14.6）。
    // 2026-10-15 现场：4 条 400 全部落在同一个 target 上，同分组另外两个账号
    // 在同一时刻大量 200。
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "refuse",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            60,
        ),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "healthy",
            &upstream.base_url,
            Protocol::AnthropicMessages,
            MODEL,
            "claude-opus-5-5",
            50,
        ),
    )
    .await;

    let rejected = json!({"error": {
        "message": "claude-opus-5-5 requires adaptive thinking; omit thinking or use thinking.type=adaptive and output_config.effort",
        "type": "invalid_request_error"
    }});
    // 只有第一个账号拒：按凭据决定行为（§4.2.1）。
    let mut by_key = std::collections::HashMap::new();
    by_key.insert("key-refuse".to_string(), Behavior::Json(400, rejected));
    upstream.fallback(Behavior::PerKey(by_key));

    let response = messages(
        &akhub,
        json!({
            "model": MODEL,
            "max_tokens": 4_096,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "enabled", "budget_tokens": 16_384}
        }),
    )
    .await;
    assert_eq!(response.status(), 200, "换到实现正常的那个渠道");
    assert_eq!(
        upstream.requests_with_key("key-refuse"),
        4,
        "先在自己身上把阶梯走完：high → xhigh → max → 不指定"
    );
    assert_eq!(
        upstream.requests_with_key("key-healthy"),
        1,
        "换渠道之后一次就成"
    );

    let mut records = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        records = akhub.state.store.list_request_records(10, 0).await.unwrap();
        if !records.is_empty() {
            break;
        }
    }
    let record = &records[0];
    // 档位阶梯走完的这次失败要留下明细：否则"一次请求打了几次上游"完全不可见。
    // 除了两个目标各一条，档位重发的那三跳也各留一条（§14.6）：不换目标，
    // `attempts` 永远看不见它们。
    assert_eq!(
        record.attempts_detail.len(),
        5,
        "两个目标 + 档位重发的三跳：{:#?}",
        record.attempts_detail
    );
    assert_eq!(record.attempts_detail[0].outcome, "effort_retry");
    assert_eq!(record.attempts_detail[1].outcome, "effort_retry");
    assert_eq!(record.attempts_detail[2].outcome, "effort_retry");
    assert_eq!(record.attempts_detail[3].outcome, "effort_exhausted");
    assert_eq!(record.attempts_detail[4].outcome, "ok");
    // 每一跳发出的是哪个档位：这正是现场"high → xhigh → max → 不指定"的阶梯。
    let levels: Vec<Option<&str>> = record
        .attempts_detail
        .iter()
        .map(|attempt| attempt.effort.as_deref())
        .collect();
    assert_eq!(
        levels,
        vec![Some("high"), Some("xhigh"), Some("max"), None, Some("high")],
        "前四跳是本目标上的阶梯，最后一跳是换渠道后的第一次发送"
    );
    // 重发次数必须自报家门：`attempts` 是 2（两个目标），重发是 3。
    assert_eq!(record.effort_retries, 3, "同一目标内的档位重发要单独计数");
    // 失败尝试也带上了上游状态码：不再"上游拒绝"与"网关拒绝"分不清。
    assert_eq!(record.attempts_detail[0].upstream_status, Some(400));
    assert_eq!(record.attempts_detail[3].upstream_status, Some(400));
    assert_eq!(record.attempts_detail[4].upstream_status, Some(200));
}
