//! 阶段 4 验收（§26.2、§27 阶段 4）：Responses 状态链。
//!
//! 覆盖三个验收点：`previous_response_id` 在允许保存状态时可跨同组上游重建；
//! `store:false` 行为符合 §15.2 的边界说明；状态过期明确报错而不是被当作
//! 新对话发送。全部通过真实 HTTP 打到假上游。

mod common;

use akhub::app::Settings;
use akhub::domain::Protocol;
use common::{Akhub, FakeUpstream, TargetSpec, client, spawn_akhub_with, wire_target};
use serde_json::{Value, json};

const MODEL: &str = "m1";

fn turn_body(input: Value, extra: Option<Value>) -> Value {
    let mut body = json!({
        "model": MODEL,
        "input": input,
        "max_output_tokens": 128,
    });
    if let (Some(extra), Some(object)) = (extra, body.as_object_mut())
        && let Some(map) = extra.as_object()
    {
        for (key, value) in map {
            object.insert(key.clone(), value.clone());
        }
    }
    body
}

/// 起一台 Akhub，接两个 Responses 账号：A 优先级 60，B 优先级 50。
/// 正常时 A 拿走全部流量；A 失败后轮到 B。
async fn spawn_two_targets() -> (Akhub, FakeUpstream, FakeUpstream) {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let a = FakeUpstream::spawn().await;
    let b = FakeUpstream::spawn().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &a.base_url,
            Protocol::OpenAiResponses,
            MODEL,
            "up-a",
            60,
        )
        .pinned(),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "B",
            &b.base_url,
            Protocol::OpenAiResponses,
            MODEL,
            "up-b",
            50,
        )
        .pinned(),
    )
    .await;
    (akhub, a, b)
}

/// 第一轮：发起新对话，返回客户端看到的响应。
async fn first_turn(akhub: &Akhub, body: Value) -> reqwest::Response {
    client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_client_only_ever_sees_gateway_response_ids() {
    let (akhub, _upstream, _b) = spawn_two_targets().await;
    let response = first_turn(&akhub, turn_body("第一轮问题".into(), None)).await;
    let body: Value = response.json().await.unwrap();
    let gateway_id = body["id"].as_str().unwrap();
    assert!(
        gateway_id.starts_with("resp_akh_"),
        "客户端必须拿到网关 ID：{gateway_id}"
    );
    assert!(
        !body.to_string().contains("resp_1"),
        "上游 ID 泄漏了：{body}"
    );

    // 定位映射已经保存：网关 ID 能查到上游 ID。
    let now = akhub::storage::now_unix();
    let row = akhub
        .state
        .store
        .response_state(gateway_id, &akhub.group_id)
        .await
        .unwrap()
        .expect("状态行必须已保存");
    assert_eq!(row.upstream_id.as_deref(), Some("resp_1"));
    assert!(row.expires_at > now);
    assert!(row.sealed_body.is_some(), "store 默认开启，正文必须保存");
}

#[tokio::test]
async fn a_stored_reference_rebuilds_the_conversation_on_another_upstream() {
    let (akhub, a, b) = spawn_two_targets().await;
    let first = first_turn(&akhub, turn_body("第一轮问题".into(), None))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let gateway_id = first["id"].as_str().unwrap().to_string();

    // A 硬失败，迫使调度走到 B。
    a.fallback(common::Behavior::Status(500, None));
    let second = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&turn_body(
            "第二轮问题".into(),
            Some(json!({"previous_response_id": gateway_id})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200, "保存了正文就必须能在 B 上重建");
    let second_body: Value = second.json().await.unwrap();
    assert!(second_body["id"].as_str().unwrap().starts_with("resp_akh_"));

    // B 收到的是完整对话：第一轮的问题与答案 + 第二轮的新问题。
    let seen = b.seen.lock().unwrap().last().unwrap().body.clone();
    let text = seen.to_string();
    assert!(text.contains("第一轮问题"), "丢了第一轮输入：{text}");
    assert!(text.contains("你好"), "丢了第一轮输出：{text}");
    assert!(text.contains("第二轮问题"), "丢了第二轮输入：{text}");
    assert!(
        seen.get("previous_response_id").is_none(),
        "跨上游重建后引用字段必须消失：{seen}"
    );
}

#[tokio::test]
async fn a_native_reference_is_rewritten_and_pinned_to_the_original_account() {
    let (akhub, a, _b) = spawn_two_targets().await;
    let first = first_turn(&akhub, turn_body("第一轮问题".into(), None))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let gateway_id = first["id"].as_str().unwrap().to_string();

    // A 正常：请求粘回原账号，引用改写成上游真 ID。
    let second = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&turn_body(
            "第二轮问题".into(),
            Some(json!({"previous_response_id": gateway_id})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    let seen = a.seen.lock().unwrap().last().unwrap().body.clone();
    assert_eq!(
        seen["previous_response_id"].as_str(),
        Some("resp_1"),
        "原生续链应该用上游自己的 ID：{seen}"
    );
    assert!(seen.to_string().contains("第二轮问题"));
    // 原生续链时上游自己掌握历史，Akhub 不重放第一轮内容。
    assert!(
        !seen.to_string().contains("第一轮问题"),
        "原生续链不应重放历史：{seen}"
    );
}

#[tokio::test]
async fn store_false_keeps_native_continuation_but_never_switches() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let a = FakeUpstream::spawn().await;
    let b = FakeUpstream::spawn().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &a.base_url,
            Protocol::OpenAiResponses,
            MODEL,
            "up-a",
            60,
        )
        .pinned(),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "B",
            &b.base_url,
            Protocol::OpenAiResponses,
            MODEL,
            "up-b",
            50,
        )
        .pinned(),
    )
    .await;

    let first = first_turn(
        &akhub,
        turn_body("第一轮问题".into(), Some(json!({"store": false}))),
    )
    .await
    .json::<Value>()
    .await
    .unwrap();
    let gateway_id = first["id"].as_str().unwrap().to_string();
    let row = akhub
        .state
        .store
        .response_state(&gateway_id, &akhub.group_id)
        .await
        .unwrap()
        .expect("store:false 仍要保存最小定位映射");
    assert!(row.sealed_body.is_none(), "store:false 不保存正文");

    // 同账号续链照常。
    let second = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&turn_body(
            "第二轮问题".into(),
            Some(json!({"previous_response_id": gateway_id})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    let seen = a.seen.lock().unwrap().last().unwrap().body.clone();
    assert_eq!(seen["previous_response_id"].as_str(), Some("resp_1"));

    // A 失败后不能把缺历史的请求丢给 B：明确报状态过期。
    a.fallback(common::Behavior::Status(500, None));
    let third = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&turn_body(
            "第三轮问题".into(),
            Some(json!({"previous_response_id": gateway_id})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(third.status(), 404, "store:false 不能跨上游，必须明确报错");
    let body: Value = third.json().await.unwrap();
    assert_eq!(body["error"]["code"], "response_state_expired");
    // 第三轮绝不曾发给 B。
    assert_eq!(b.requests(), 0, "缺历史的请求不能当作新对话发送");
}

#[tokio::test]
async fn an_unknown_reference_fails_with_response_state_expired() {
    let (akhub, a, b) = spawn_two_targets().await;
    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&turn_body(
            "问题".into(),
            Some(json!({"previous_response_id": "resp_akh_不存在的"})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "response_state_expired");
    assert_eq!(a.requests() + b.requests(), 0, "缺失历史的请求绝不外发");
}

#[tokio::test]
async fn another_group_cannot_reference_our_states() {
    let (akhub, _a, _b) = spawn_two_targets().await;
    let first = first_turn(&akhub, turn_body("第一轮问题".into(), None))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let gateway_id = first["id"].as_str().unwrap().to_string();

    // 第二个分组：自己的 Key，共享同一台 Akhub。
    let (other_key, prefix) = akhub::security::generate_group_key().unwrap();
    let other_group = akhub::domain::Group {
        id: "grp_other".into(),
        name: "其他组".into(),
        key_prefix: prefix,
        key_digest_hex: akhub.state.key_digest.digest_hex(&other_key),
        multiplier_limit: akhub::domain::Multiplier::ONE,
        weights: Default::default(),
        queue_capacity: 10,
        allow_degrade: true,
        created_at: time::OffsetDateTime::now_utc(),
    };
    akhub.state.store.insert_group(&other_group).await.unwrap();
    akhub.state.reload_config().await.unwrap();

    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(other_key.to_string())
        .json(&turn_body(
            "偷看".into(),
            Some(json!({"previous_response_id": gateway_id})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404, "跨组引用一律按不存在处理");
}

#[tokio::test]
async fn expired_states_report_response_state_expired() {
    let akhub = spawn_akhub_with(
        Settings {
            response_state_days: 1,
            ..Settings::default()
        },
        |_| {},
    )
    .await;
    let upstream = FakeUpstream::spawn().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            Protocol::OpenAiResponses,
            MODEL,
            "up",
            50,
        )
        .pinned(),
    )
    .await;

    let first = first_turn(&akhub, turn_body("问题".into(), None))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let gateway_id = first["id"].as_str().unwrap().to_string();

    // 直接把过期时间拨到过去：模拟保留期流逝。
    sqlx::query("UPDATE response_states SET expires_at = 0")
        .execute(akhub.state.store.pool())
        .await
        .unwrap();

    let second = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&turn_body(
            "续问".into(),
            Some(json!({"previous_response_id": gateway_id})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 404);
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["error"]["code"], "response_state_expired");
    // 过期引用不当作新对话：上游没有收到第二次请求。
    assert_eq!(upstream.requests(), 1);
}

#[tokio::test]
async fn streamed_responses_carry_gateway_ids_and_support_the_chain() {
    let (akhub, upstream, _b) = spawn_two_targets().await;
    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&turn_body(
            "第一轮问题".into(),
            Some(json!({"stream": true})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("event: response.created"));
    // 流里每个 response.id 都是网关 ID，且数量一致。
    assert!(
        text.contains("\"id\":\"resp_akh_"),
        "流式响应的 id 必须是网关 ID：{text}"
    );
    assert!(!text.contains("\"id\":\"resp_1\""), "上游 ID 泄漏：{text}");

    // 网关 ID 已登记，正文已保存。
    let created = text
        .split("data: ")
        .find_map(|frame| {
            let value: Value = serde_json::from_str(frame.trim()).ok()?;
            let id = value.get("response")?.get("id")?.as_str()?;
            Some(id.to_string())
        })
        .expect("response.created 必须携带 id");
    let row = akhub
        .state
        .store
        .response_state(&created, &akhub.group_id)
        .await
        .unwrap()
        .expect("流式响应的状态必须已保存");
    assert!(row.sealed_body.is_some());

    // 第二轮走原生续链。
    let second = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&turn_body(
            "第二轮问题".into(),
            Some(json!({"previous_response_id": created})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    let seen = upstream.seen.lock().unwrap().last().unwrap().body.clone();
    assert_eq!(seen["previous_response_id"].as_str(), Some("resp_1"));
}

#[tokio::test]
async fn retrieve_and_delete_work_through_gateway_ids() {
    let (akhub, _a, _b) = spawn_two_targets().await;
    let first = first_turn(&akhub, turn_body("第一轮问题".into(), None))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let gateway_id = first["id"].as_str().unwrap().to_string();
    let http = client();

    // 查询：返回保存的正文形状，ID 是网关 ID。
    let got = http
        .get(format!("{}/v1/responses/{gateway_id}", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let body: Value = got.json().await.unwrap();
    assert_eq!(body["id"].as_str(), Some(gateway_id.as_str()));
    assert!(body["input"].as_array().is_some(), "{body}");

    // 删除：本地状态随之消失，之后的引用按过期处理。
    let deleted = http
        .delete(format!("{}/v1/responses/{gateway_id}", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 200);
    let body: Value = deleted.json().await.unwrap();
    assert_eq!(body["deleted"], true);
    assert!(
        akhub
            .state
            .store
            .response_state(&gateway_id, &akhub.group_id)
            .await
            .unwrap()
            .is_none()
    );

    let again = http
        .get(format!("{}/v1/responses/{gateway_id}", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 404);
}

#[tokio::test]
async fn cross_protocol_failover_rebuilds_the_history_into_the_other_protocol() {
    // 入口 Responses（A 账号 Responses 上游），第二目标 B 是 Chat 上游：
    // A 失败后重建的历史要转换成 Chat 形状发出去。
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let a = FakeUpstream::spawn().await;
    let b = FakeUpstream::spawn().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &a.base_url,
            Protocol::OpenAiResponses,
            MODEL,
            "up-a",
            60,
        )
        .pinned(),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("B", &b.base_url, Protocol::OpenAiChat, MODEL, "up-b", 50).pinned(),
    )
    .await;

    let first = first_turn(&akhub, turn_body("第一轮问题".into(), None))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let gateway_id = first["id"].as_str().unwrap().to_string();

    a.fallback(common::Behavior::Status(500, None));
    let second = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&turn_body(
            "第二轮问题".into(),
            Some(json!({"previous_response_id": gateway_id})),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);

    let seen = b.seen.lock().unwrap().last().unwrap().body.clone();
    let text = seen.to_string();
    assert!(
        seen.get("messages").and_then(Value::as_array).is_some(),
        "{seen}"
    );
    assert!(
        text.contains("第一轮问题"),
        "Chat 请求丢了第一轮输入：{text}"
    );
    assert!(
        text.contains("第二轮问题"),
        "Chat 请求丢了第二轮输入：{text}"
    );
}
