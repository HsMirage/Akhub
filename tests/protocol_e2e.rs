//! 阶段 3 验收：六个方向的跨协议转换、能力降级白名单与端点选择
//! （§26.1、§14.3、§14.8）。
//!
//! 全部通过真实 HTTP 打到假上游：请求经过完整的鉴权、资格过滤、端点选择、
//! 中间格式转换、上游调用与响应回译。

mod common;

use akhub::domain::Protocol;
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

    let held = akhub
        .state
        .runtime
        .health
        .try_admit(
            &native_target.account_id,
            &native_target.target_id,
            akhub::domain::Limits {
                max_concurrency: Some(1),
                ..Default::default()
            },
            0,
        )
        .unwrap();
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
async fn a_missing_native_endpoint_falls_back_to_conversion_and_is_remembered() {
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

    // 证据已经记下：后续请求不再重复撞那扇不存在的门。
    assert_eq!(messages(&akhub, body).await.status(), 200);
    let paths: Vec<String> = upstream
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|seen| seen.path.clone())
        .collect();
    assert_eq!(paths.len(), 3);
    assert_eq!(paths[2], "/v1/chat/completions");
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
        akhub.state.runtime.evidence.clear();
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
                &wired.account_id,
                &wired.target_id,
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
