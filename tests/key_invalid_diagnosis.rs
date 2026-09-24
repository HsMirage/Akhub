//! 403 与"Key 失效"的边界（§12.3、§4.2.1）。
//!
//! 回归的对象是一类真实故障：上游用 403 表达"分组被停用 / 权限不足 / WAF
//! 拦截"，网关只看状态码就把它当成"这把 Key 失效"，而失效标记只可能被**一次
//! 成功**清除——可这把 Key 已经被排除在抽签之外。于是面板上出现"Key 失效，
//! 但 Key 是好的"，而且再也没有出口。这里钉住四条出口：正文判据、连续确认、
//! 手动测试、显式清除。
//!
//! 同时钉住**没有被改坏**的那几支：402 额度耗尽、429 限流，以及 401/403 带
//! `Retry-After` 时的额度冷却。它们共享同一条凭据级失败路径，改动很容易顺手
//! 把它们吞掉（开发中确实吞过一次）。

mod common;

use akhub::domain::{Limits, Protocol};
use akhub::health::{Caller, Unavailable};
use common::{Behavior, FakeUpstream, TargetSpec, chat, client, spawn_akhub, wire_target};
use serde_json::json;

const CHAT: Protocol = Protocol::OpenAiChat;
const MODEL: &str = "glm-4.6";

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

async fn key_health(akhub: &common::Akhub, a: &common::Wired) -> Result<(), Unavailable> {
    akhub.state.runtime.health.check(
        Caller {
            account_id: &a.account_id,
            key_id: Some(&a.credential_digest),
            target_id: &a.target_id,
        },
        Limits::default(),
    )
}

async fn send(akhub: &common::Akhub) -> u16 {
    chat(akhub, json!({"model": MODEL, "messages": []}))
        .await
        .status()
        .as_u16()
}

/// 403 说"分组被停用"：那是权限问题，不是这把 Key 坏了。
///
/// 判据必须看正文。只看状态码的话，每一次这样的 403 都被记成"凭据失效"。
/// 这里刻意把同样的 403 打满熔断阈值次数：如果判据退回"看状态码"，这把 Key
/// 早就越过连续确认阈值而硬停——断言因此能在**只看状态码**时失败。
#[tokio::test]
async fn a_403_about_a_disabled_group_does_not_hard_stop_the_key() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, CHAT, MODEL, MODEL, 100),
    )
    .await;

    up.fallback(Behavior::Json(
        403,
        json!({"error": {"message": "当前分组已被停用，请联系管理员"}}),
    ));
    // 超过连续确认阈值（2）足够多次：把每一次都冤枉判成"凭据不对"的实现，
    // 在这里必然已经把这把 Key 钉死。
    for _ in 0..6 {
        send(&akhub).await;
    }

    // 只看**这把 Key** 的资格：目标级的复合判定会被"重复 403 触发目标冷却"
    // 影响（那是 §12.3 既有的、正确的行为），不能拿来证明凭据有没有被冤枉。
    let id = akhub::credential::credential_id(&a.account_id, &a.credential_digest);
    let key = akhub.state.runtime.health.key(&id);
    assert_eq!(key.status().as_str(), "active");
    assert!(
        !key.auth_proves_invalid(),
        "分组停用不该在这把 Key 上留下'凭据曾失效'的标记"
    );
    assert_eq!(
        key.check(Limits::default(), tokio::time::Instant::now()),
        Ok(()),
        "403 说的是分组停用，不是凭据失效：不该把这把 Key 硬停（§12.3 只认'明确证明 Key 失效'）"
    );
}

/// 403 的正文明确说"Invalid API key"：这才是失效证据，且要连续确认。
#[tokio::test]
async fn a_403_that_names_the_key_stops_it_after_the_confirmation_window() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, CHAT, MODEL, MODEL, 100),
    )
    .await;

    // 两次上游拒绝都必须来自**真实的两次尝试**：用 fallback 兜底会让第一次
    // 就完成判定，测出来的其实是"一次就停"（曾经就是这么写错的）。
    up.fallback(Behavior::Json(
        403,
        json!({"error": {"message": "Invalid API key provided"}}),
    ));

    // 第一次：只是一次怀疑，Key 还能继续服务。
    send(&akhub).await;
    assert!(
        key_health(&akhub, &a).await.is_ok(),
        "偶发一次判定不该把 Key 钉死"
    );

    // 第二次：确认到位，硬停。
    send(&akhub).await;
    assert_eq!(
        key_health(&akhub, &a).await,
        Err(Unavailable::KeyInvalid),
        "连续两次明确说凭据不对，才真正失效（§12.3）"
    );
}

/// §12.3："手动测试成功后恢复"。
///
/// 测试连接走独立路径、不写健康状态，但"这一把确实通了"正是解除硬停最可信
/// 的依据。不在这里放行，面板会一直显示一个已经被证伪的故障。
#[tokio::test]
async fn a_successful_manual_test_clears_the_hard_stop() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, CHAT, MODEL, MODEL, 100),
    )
    .await;
    let cookie = admin_cookie(&akhub).await;
    let http = client();

    // 上游连续两次明确拒绝：这把 Key 被硬停。
    up.fallback(Behavior::Json(
        403,
        json!({"error": {"message": "Invalid API key provided"}}),
    ));
    send(&akhub).await;
    send(&akhub).await;
    assert_eq!(key_health(&akhub, &a).await, Err(Unavailable::KeyInvalid));

    // 上游恢复正常，管理员点"测试连接"。
    up.fallback(Behavior::Ok);
    let tested: serde_json::Value = http
        .post(format!(
            "{}/admin/api/accounts/{}/test",
            akhub.base_url, a.account_id
        ))
        .header("cookie", &cookie)
        .header("x-akhub-csrf", "1")
        .json(&json!({"model": MODEL}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tested["ok"], json!(true), "测试必须通过：{tested}");

    assert!(
        key_health(&akhub, &a).await.is_ok(),
        "§12.3：手动测试成功后必须解除硬停"
    );
}

/// 面板上的"清除失效标记"：不重新粘凭据也能放行一把被误判的 Key。
#[tokio::test]
async fn clearing_the_marker_by_hand_releases_the_key() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, CHAT, MODEL, MODEL, 100),
    )
    .await;
    let cookie = admin_cookie(&akhub).await;
    let http = client();

    up.fallback(Behavior::Json(
        403,
        json!({"error": {"message": "Invalid API key provided"}}),
    ));
    send(&akhub).await;
    send(&akhub).await;
    assert_eq!(key_health(&akhub, &a).await, Err(Unavailable::KeyInvalid));

    let cleared: serde_json::Value = http
        .post(format!(
            "{}/admin/api/accounts/{}/keys/{}/clear-faults",
            akhub.base_url, a.account_id, a.key_id
        ))
        .header("cookie", &cookie)
        .header("x-akhub-csrf", "1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["ok"], json!(true), "{cleared}");
    assert!(
        key_health(&akhub, &a).await.is_ok(),
        "显式清除之后这把 Key 必须立刻回到可用状态"
    );

    // 面板上的那个徽标也要跟着回到正常：管理员看的就是它。
    let account: serde_json::Value = http
        .get(format!("{}/admin/api/accounts?limit=50", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = account["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!(a.account_id))
        .unwrap();
    assert_eq!(row["health"]["status"], json!("active"), "{row}");
    assert_eq!(
        row["health"]["keys"][0]["auth_proves_invalid"],
        json!(false),
        "清除之后不该还留着'曾被判定失效'的说明"
    );
}

/// 402 额度耗尽：进额度冷却、尊重上游给的恢复时间，**绝不**留失效标记。
///
/// 这条与 403 判据共用同一条凭据级失败路径。把判据写成"任何非 2xx 且正文没提
/// 凭据就降级成故障"时，402 会被吞成普通故障——额度熔断与 `Retry-After` 一起
/// 丢失，这条断言就是为它写的。
#[tokio::test]
async fn a_402_quota_failure_cools_the_key_and_keeps_the_retry_after() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, CHAT, MODEL, MODEL, 100),
    )
    .await;

    up.fallback(Behavior::Status(402, Some(120)));
    send(&akhub).await;

    assert_eq!(
        key_health(&akhub, &a).await,
        Err(Unavailable::QuotaExhausted),
        "402 必须把额度熔断记在这把 Key 上，而不是降级成普通故障"
    );
    let id = akhub::credential::credential_id(&a.account_id, &a.credential_digest);
    assert!(
        !akhub.state.runtime.health.key(&id).auth_proves_invalid(),
        "额度不足不等于凭据不对，不该留下失效标记"
    );
    // 上游给了 120 秒：冷却剩余时间要反映它，而不是退回自己的指数退避。
    let remaining = akhub
        .state
        .runtime
        .health
        .key(&id)
        .cooldown_remaining(tokio::time::Instant::now())
        .expect("402 之后应当处于冷却中");
    assert!(
        remaining > std::time::Duration::from_secs(100),
        "必须采纳上游的 Retry-After（120s），实际剩余 {remaining:?}"
    );
}

/// 402 **不带** `Retry-After`：仍然是额度问题，不是凭据问题。
///
/// 这一支是最容易被判据顺手吞掉的形状：没有恢复时间时，"额度耗尽"与"凭据不对"
/// 的区别只来自状态码本身。判据一旦对所有状态码生效，它就会退化成普通故障——
/// 额度熔断消失，Key 还被反复重试。
#[tokio::test]
async fn a_402_without_retry_after_is_still_a_quota_failure() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, CHAT, MODEL, MODEL, 100),
    )
    .await;

    up.fallback(Behavior::Status(402, None));
    send(&akhub).await;

    assert_eq!(
        key_health(&akhub, &a).await,
        Err(Unavailable::QuotaExhausted),
        "没有恢复时间的 402 也要记成额度冷却，不能被判据降级成普通故障"
    );
    let id = akhub::credential::credential_id(&a.account_id, &a.credential_digest);
    assert!(
        !akhub.state.runtime.health.key(&id).auth_proves_invalid(),
        "额度不足不等于凭据不对"
    );
}

/// 401 + `Retry-After`：上游说的是"过一会儿再来"，不是"这把 Key 废了"。
///
/// §12.3 的既有口径是"明确额度不足且带恢复时间时按恢复时间冷却"。这条规则此前
/// 被 401/403 一视同仁的硬停盖掉了——一个按分钟限流的签到站回一次 401 就能把
/// 这把 Key 钉死。这里从网关入口钉住它。
#[tokio::test]
async fn a_401_with_retry_after_cools_the_key_instead_of_stopping_it() {
    let up = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &up.base_url, CHAT, MODEL, MODEL, 100),
    )
    .await;

    up.fallback(Behavior::Status(401, Some(30)));
    // 打满确认次数：如果这里做的是硬停，这时必然已经失效。
    for _ in 0..3 {
        send(&akhub).await;
    }

    assert_eq!(
        key_health(&akhub, &a).await,
        Err(Unavailable::QuotaExhausted),
        "带恢复时间的 401 应当进额度冷却，而不是永久硬停（§12.3）"
    );
    let id = akhub::credential::credential_id(&a.account_id, &a.credential_digest);
    assert!(
        !akhub.state.runtime.health.key(&id).auth_proves_invalid(),
        "冷却是时间问题，不该留下'凭据曾经失效'的标记"
    );
}
