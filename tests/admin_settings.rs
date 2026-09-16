//! 管理员账号与系统设置的验收（§6.7、§23.2）。
//!
//! 覆盖三件事：系统设置能在后台改、改完持久化并热生效；管理员能改密码且
//! 旧会话立即失效；账号的"刷新倍率"按钮同步返回真实探测结果。

mod common;

use akhub::app::Settings;
use akhub::auth::session;
use akhub::domain::{Limits, Multiplier, MultiplierMode, Protocol};
use common::{FakeUpstream, TargetSpec, client, spawn_akhub_with, wire_target};
use serde_json::{Value, json};
use time::OffsetDateTime;

/// 建一个管理员会话，返回可直接使用的 Cookie 值。
async fn admin_cookie(akhub: &common::Akhub) -> String {
    let hash = session::hash_password("测试密码-足够长-123").unwrap();
    akhub
        .state
        .store
        .create_admin("admin", &hash)
        .await
        .unwrap();
    let token = akhub.state.sessions.create("admin").unwrap();
    format!("akhub_session={token}")
}

fn write(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.header("x-akhub-csrf", "1")
}

#[tokio::test]
async fn settings_can_be_edited_persisted_and_hot_applied() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    let http = client();

    // 改两个字段：请求超时与保留天数。
    let updated: Value = write(
        http.patch(format!("{}/admin/api/settings", akhub.base_url))
            .header("cookie", &cookie)
            .json(&json!({"request_timeout_secs": 42, "retention_days": 7})),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(updated["request_timeout_secs"], 42, "{updated}");
    assert_eq!(updated["retention_days"], 7, "{updated}");
    assert_eq!(
        updated["restart_required"][0], "shutdown_grace_secs",
        "关闭宽限期要等重启"
    );

    // 热生效：运行中的状态立刻是新值。
    assert_eq!(akhub.state.settings.get().request_timeout.as_secs(), 42);

    // 越界拒绝。
    let rejected = write(
        http.patch(format!("{}/admin/api/settings", akhub.base_url))
            .header("cookie", &cookie)
            .json(&json!({"request_timeout_secs": 1})),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(rejected.status(), 400);
    assert_eq!(akhub.state.settings.get().request_timeout.as_secs(), 42);

    // 重启后仍然是新值。
    let dir = akhub.data_dir().to_path_buf();
    let port = akhub.base_url.clone();
    std::mem::forget(akhub);
    let restarted = akhub::app::AppState::bootstrap(&dir, Settings::default())
        .await
        .unwrap();
    assert_eq!(restarted.settings.get().request_timeout.as_secs(), 42);
    assert_eq!(restarted.settings.get().retention_days, 7);
    drop(restarted);
    let _ = port;
}

#[tokio::test]
async fn changing_the_admin_password_kills_old_sessions() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    let http = client();

    // 另一台设备上的旧会话。
    let other = akhub.state.sessions.create("admin").unwrap();

    // 当前密码错 → 401。
    let wrong = write(
        http.post(format!("{}/admin/api/auth/password", akhub.base_url))
            .header("cookie", &cookie)
            .json(&json!({"current_password": "不对", "new_password": "新密码-足够长-456789"})),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(wrong.status(), 401);

    // 太短 → 400。
    let short = write(
        http.post(format!("{}/admin/api/auth/password", akhub.base_url))
            .header("cookie", &cookie)
            .json(&json!({"current_password": "测试密码-足够长-123", "new_password": "短"})),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(short.status(), 400);

    // 正常修改。
    let changed = write(
        http.post(format!("{}/admin/api/auth/password", akhub.base_url))
            .header("cookie", &cookie)
            .json(&json!({
                "current_password": "测试密码-足够长-123",
                "new_password": "新密码-足够长-456789"
            })),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(changed.status(), 200, "改密码必须成功");
    assert!(
        changed.headers().get("set-cookie").is_some(),
        "当前浏览器应拿到新会话"
    );

    // 旧会话（另一台设备）立即失效。
    let stale = http
        .get(format!("{}/admin/api/overview", akhub.base_url))
        .header("cookie", format!("akhub_session={other}"))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 401, "改密码后旧会话必须失效");
}

#[tokio::test]
async fn the_refresh_button_returns_a_real_probe_result() {
    let upstream = FakeUpstream::spawn().await;
    // 现场形状的 Key 级计费响应。
    upstream.set_billing(Some(json!({
        "object": "sub2api.key_billing",
        "schema_version": 1,
        "billing_scope": "token",
        "group_rate_multiplier": 0.5,
        "resolved_rate_multiplier": 0.5,
        "peak_rate_enabled": false,
        "effective_rate_multiplier": 0.5,
        "observed_at": "2026-09-15T17:24:50.924429747Z"
    })));

    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "sub2api",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "m1",
            "m1",
            50,
        )
        .limits(Limits::default())
        .multiplier(MultiplierMode::Sub2Api, "1"),
    )
    .await;
    let cookie = admin_cookie(&akhub).await;

    let response = write(
        client()
            .post(format!(
                "{}/admin/api/accounts/{}/refresh-multiplier",
                akhub.base_url, wired.account_id
            ))
            .header("cookie", &cookie),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 200, "刷新必须同步完成");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["refreshed"], true, "{body}");
    assert_eq!(
        body["effective_multiplier"],
        json!("0.5"),
        "返回真实探测到的倍率：{body}"
    );

    // 探测失败时把错误带回来，而不是假装成功。
    upstream.set_billing(None);
    let failed = write(
        client()
            .post(format!(
                "{}/admin/api/accounts/{}/refresh-multiplier",
                akhub.base_url, wired.account_id
            ))
            .header("cookie", &cookie),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(failed.status(), 502, "探针失败要如实报错");
    let _: OffsetDateTime = OffsetDateTime::now_utc();
    let _ = Multiplier::ONE;
}

/// 站点级凭据：一个 Base URL 配一次，账号不必再填令牌与用户 ID（§6.4）。
#[tokio::test]
async fn a_site_credential_lets_accounts_skip_their_own_token() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(json!({
        "success": true,
        "data": {"gpt-boom": {"ratio": 0.1, "desc": "特价"}}
    })));
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    let http = client();

    // 1) 保存站点凭据（令牌只进不回）。
    let saved = write(
        http.post(format!("{}/admin/api/new-api-sites", akhub.base_url))
            .header("cookie", &cookie)
            .json(&json!({
                "base_url": upstream.base_url,
                "user_id": "1",
                "access_token": "site-token"
            })),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(saved.status(), 200);

    // 2) 建账号：不填令牌与用户 ID，也能开自动倍率。
    let created: Value = write(
        http.post(format!("{}/admin/api/accounts", akhub.base_url))
            .header("cookie", &cookie)
            .json(&json!({
                "group_id": akhub.group_id,
                "name": "站点凭据账号",
                "upstream_type": "openai_compatible",
                "base_url": upstream.base_url,
                "api_key": "sk-abc",
                "preferred_protocol": "openai_chat",
                "multiplier_mode": "new_api",
                "new_api_group": "gpt-boom",
                "allow_private_network": true
            })),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let id = created["id"].as_str().expect("账号应创建成功").to_string();
    assert_eq!(created["uses_site_credentials"], true, "{created}");
    assert_eq!(created["has_new_api_token"], false, "{created}");

    // 3) 刷新走站点凭据，读到 gpt-boom 的 0.1。
    let refreshed: Value = write(
        http.post(format!(
            "{}/admin/api/accounts/{id}/refresh-multiplier",
            akhub.base_url
        ))
        .header("cookie", &cookie),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(refreshed["effective_multiplier"], "0.1", "{refreshed}");
}
