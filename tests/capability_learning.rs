//! 能力学习的粒度（§16.7）。
//!
//! 回归的对象是现场事故：上游对 **claude-opus-5-5** 回
//! "does not support forced tool_choice; use auto or none"。这条 400 的措辞里
//! 同时含 "not support" 与 "tool"，旧实现据此记成"这个账号模型不支持
//! function_calling"，于是**所有带工具的请求**被 24 小时连坐屏蔽；而直连上游
//! 的调用没有这份记忆，看起来就像"只有经过 Akhub 才坏"。
//!
//! 这里钉住四件事：
//!
//! 1. 归因取**最窄**的那项能力：强制工具选择失败不会牵连普通工具调用；
//! 2. 一次失败不足以封禁，要攒够词表规定的独立证据；
//! 3. 真的被封禁时，请求记录与对外文案能指出具体是哪项能力；
//! 4. 封禁在后台可见，并且有手动出口（§23.5）。

mod common;

use akhub::domain::Protocol;
use common::{Behavior, FakeUpstream, TargetSpec, client, messages, spawn_akhub, wire_target};
use serde_json::{Value, json};

const MESSAGES: Protocol = Protocol::AnthropicMessages;
const MODEL: &str = "claude-opus-5-5";
/// 上游的真实措辞（现场抓到的原文）。
const FORCED_REJECTION: &str =
    "claude-opus-5-5 does not support forced tool_choice; use auto or none";

fn tool() -> Value {
    json!({
        "name": "get_weather",
        "description": "Get the weather",
        "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
    })
}

/// 带工具的 Messages 请求。`forced` 为真时用强制型工具选择。
fn body(forced: bool) -> Value {
    let mut body = json!({
        "model": MODEL,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "巴黎天气怎么样？"}],
        "tools": [tool()],
    });
    body["tool_choice"] = if forced {
        json!({"type": "any"})
    } else {
        json!({"type": "auto"})
    };
    body
}

async fn send(akhub: &common::Akhub, forced: bool) -> (u16, Value) {
    let response = messages(akhub, body(forced)).await;
    let status = response.status().as_u16();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, value)
}

/// 手工建一个管理员会话，返回可直接使用的 Cookie。
async fn admin_cookie(akhub: &common::Akhub) -> String {
    let hash = akhub::auth::session::hash_password("测试密码-足够长-123").unwrap();
    akhub
        .state
        .store
        .create_admin("admin", &hash)
        .await
        .unwrap();
    let token = akhub.state.sessions.create("admin").unwrap();
    format!("akhub_session={token}")
}

/// 上游对强制型工具选择的拒绝**只**归因到 `forced_tool_choice`。
///
/// 两条断言缺一不可：普通工具调用必须照常打到上游；强制型请求必须被拦在我们
/// 自己的文案上，而不是再去撞一次同一堵墙。
#[tokio::test]
async fn a_forced_tool_choice_rejection_never_bans_plain_tool_calls() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, MESSAGES, MODEL, MODEL, 100),
    )
    .await;

    // 前两次：上游按真实措辞拒绝，请求体原样透传给客户端。
    up.script([
        Behavior::Json(400, json!({"error": {"message": FORCED_REJECTION}})),
        Behavior::Json(400, json!({"error": {"message": FORCED_REJECTION}})),
    ]);

    let (status, _) = send(&akhub, true).await;
    assert_eq!(status, 400, "第一次失败应当是上游的 400 原样透传");

    // 只失败一次还不够：第二次仍然要真的打到上游（否则"一次抖动就封禁"）。
    let before = up.requests();
    let (status, _) = send(&akhub, true).await;
    assert_eq!(status, 400);
    assert_eq!(
        up.requests(),
        before + 1,
        "证据没攒够之前不该拦，第二次必须仍然经过上游"
    );

    let now = std::time::Instant::now();
    assert!(
        akhub.state.runtime.capabilities.is_unsupported(
            &wired.account_id,
            MODEL,
            "forced_tool_choice",
            now
        ),
        "两次独立证据之后，窄能力应当已经生效"
    );
    assert!(
        !akhub.state.runtime.capabilities.is_unsupported(
            &wired.account_id,
            MODEL,
            "function_calling",
            now
        ),
        "强制工具选择被拒**不能**连坐整类工具调用（现场事故的根因）"
    );

    // 第三次：我们自己的回答，且文案说得清是哪项能力；上游不该再被打扰。
    // 状态码是 503 而不是 400：这是**会过期**的内存限制，属于"暂时没有可用目标"，
    // 说成不可重试会把客户端劝退。
    let before = up.requests();
    let (status, error) = send(&akhub, true).await;
    assert_eq!(status, 503);
    assert_eq!(up.requests(), before, "已经确认过的组合不该继续撞上游");
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("forced_tool_choice"),
        "文案要指出具体能力，实际是：{message}"
    );

    // 关键断言：普通工具调用（tool_choice = auto）照常可用。
    let (status, ok) = send(&akhub, false).await;
    assert_eq!(
        status, 200,
        "只用 auto 的请求被连坐屏蔽了，这正是现场事故：{ok}"
    );
    assert_eq!(ok["model"], json!(MODEL));
}

/// 证据不够时不该封禁：单次失败不能变成一条 24 小时的禁令。
#[tokio::test]
async fn one_rejection_is_not_enough_to_block_the_target() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, MESSAGES, MODEL, MODEL, 100),
    )
    .await;

    up.script([Behavior::Json(
        400,
        json!({"error": {"message": FORCED_REJECTION}}),
    )]);
    let (status, _) = send(&akhub, true).await;
    assert_eq!(status, 400);

    // 第二次换个成功响应：它必须能打到上游。旧实现第一次就封了，这里会得到
    // 我们自己造的 400，而不是上游的 200。
    let before = up.requests();
    let (status, _) = send(&akhub, true).await;
    assert_eq!(status, 200);
    assert_eq!(up.requests(), before + 1);
}

/// 屏蔽必须在后台可见，并且有手动出口（§23.5）。
#[tokio::test]
async fn scheduling_blocks_are_visible_and_releasable_from_the_admin_api() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, MESSAGES, MODEL, MODEL, 100),
    )
    .await;
    let cookie = admin_cookie(&akhub).await;
    let http = client();

    up.fallback(Behavior::Json(
        400,
        json!({"error": {"message": FORCED_REJECTION}}),
    ));
    send(&akhub, true).await;
    send(&akhub, true).await;

    let listed: Value = http
        .get(format!("{}/admin/api/scheduling-blocks", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let capabilities = listed["capabilities"].as_array().unwrap();
    assert!(
        capabilities.iter().any(|entry| {
            entry["capability"] == json!("forced_tool_choice")
                && entry["effective"] == json!(true)
                && entry["account_id"] == json!(wired.account_id)
        }),
        "生效中的窄能力限制必须出现在屏蔽列表里：{listed}"
    );
    assert!(
        !capabilities
            .iter()
            .any(|entry| entry["capability"] == json!("function_calling")),
        "普通工具调用不该被这条证据牵连：{listed}"
    );

    // 手动放行之后，请求必须重新打到上游。
    let cleared: Value = http
        .post(format!(
            "{}/admin/api/scheduling-blocks/clear",
            akhub.base_url
        ))
        .header("cookie", &cookie)
        .header("x-akhub-csrf", "1")
        .json(&json!({
            "scope": "capability",
            "account_id": wired.account_id,
            "model": MODEL,
            "capability": "forced_tool_choice",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["capabilities_cleared"], json!(1), "{cleared}");

    let before = up.requests();
    let (status, _) = send(&akhub, true).await;
    assert_eq!(status, 400, "上游仍然拒绝，所以还是 400");
    assert_eq!(up.requests(), before + 1, "放行之后必须重新向上游取证");

    // 放行同样适用于另一类屏蔽：端点缺失证据。
    let evidence: Value = http
        .get(format!("{}/admin/api/scheduling-blocks", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        evidence["evidence"].is_array(),
        "端点证据也要一并摊开：{evidence}"
    );
}

/// 同协议目标不该被整段 JSON 里的字段名骗到：请求体里带 `tool_choice` 这个
/// **字段**，但上游拒绝的是别的东西时，不记任何能力证据。
#[tokio::test]
async fn an_unrelated_rejection_is_not_read_as_a_capability_gap() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, MESSAGES, MODEL, MODEL, 100),
    )
    .await;

    up.fallback(Behavior::Json(
        400,
        json!({"error": {"message": "max_tokens is not supported by this model"}}),
    ));
    let (status, _) = send(&akhub, true).await;
    assert_eq!(status, 400);

    let now = std::time::Instant::now();
    for capability in ["function_calling", "forced_tool_choice", "reasoning"] {
        assert!(
            !akhub.state.runtime.capabilities.is_unsupported(
                &wired.account_id,
                MODEL,
                capability,
                now
            ),
            "与能力无关的 400 不该记成 {capability} 的限制"
        );
    }
    assert!(
        akhub.state.runtime.capabilities.snapshot(now).is_empty(),
        "没有任何能力证据该被记下来（包括还在攒证据的）"
    );
}

/// 现场事故的完整形状：一个渠道被停用 + 另一个渠道上有一条学到的能力限制。
///
/// 旧实现的三个毛病都在这一条里：文案报"已停用"（把管理员引到配置上）、
/// 状态码 400（告诉客户端别再重试）、以及最根本的——那条限制本来就不该记。
#[tokio::test]
async fn a_disabled_channel_next_to_a_learned_ban_names_the_real_blocker_and_stays_retryable() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let primary = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, MESSAGES, MODEL, MODEL, 100),
    )
    .await;
    let secondary = wire_target(
        &akhub,
        TargetSpec::new("B", &up.base_url, MESSAGES, MODEL, MODEL, 100),
    )
    .await;
    let cookie = admin_cookie(&akhub).await;
    let http = client();

    // 管理员停用第二个渠道（现场就是"关掉一个渠道"）。
    let disabled = http
        .patch(format!(
            "{}/admin/api/targets/{}",
            akhub.base_url, secondary.target_id
        ))
        .header("cookie", &cookie)
        .header("x-akhub-csrf", "1")
        .json(&json!({"enabled": false}))
        .send()
        .await
        .unwrap();
    assert!(
        disabled.status().is_success(),
        "停用目标失败：{}",
        disabled.status()
    );

    // 上游按真实措辞拒绝强制工具选择：只剩 A 可调度，两次证据都落在它身上。
    up.fallback(Behavior::Json(
        400,
        json!({"error": {"message": FORCED_REJECTION}}),
    ));
    send(&akhub, true).await;
    send(&akhub, true).await;

    let (status, error) = send(&akhub, true).await;
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(
        status, 503,
        "会过期的内存限制不能报成不可重试的 400：{message}"
    );
    assert!(
        message.contains("forced_tool_choice") && message.contains('A'),
        "文案要点名账号与能力：{message}"
    );
    assert!(
        !message.contains("已停用"),
        "把能力限制说成'已停用'会把管理员引到错误方向：{message}"
    );
    assert!(
        !akhub.state.runtime.capabilities.is_unsupported(
            &primary.account_id,
            MODEL,
            "function_calling",
            std::time::Instant::now()
        ),
        "普通工具调用不能被连坐"
    );
}

/// 放宽限制之后，所有候选仍然不合格时，对外文案要指出真正拦路的能力，
/// 而不是邻居目标的"已停用"（§18.3）。
#[tokio::test]
async fn the_failure_message_names_the_real_blocker() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, MESSAGES, MODEL, MODEL, 100),
    )
    .await;

    up.fallback(Behavior::Json(
        400,
        json!({"error": {"message": FORCED_REJECTION}}),
    ));
    send(&akhub, true).await;
    send(&akhub, true).await;

    let (status, error) = send(&akhub, true).await;
    assert_eq!(status, 503, "学到的限制会过期，保留可重试语义");
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("forced_tool_choice") && message.contains(MODEL),
        "文案必须点名能力与模型：{message}"
    );
    assert!(
        !message.contains("已停用"),
        "把能力封禁说成'已停用'会把管理员引到错误方向：{message}"
    );
}
