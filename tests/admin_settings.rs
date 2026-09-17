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

/// 造一条请求记录，用来验证筛选与概览聚合。
#[allow(clippy::too_many_arguments)]
async fn insert_record(
    akhub: &common::Akhub,
    request_id: &str,
    logical_model: &str,
    target_id: Option<&str>,
    http_status: i64,
    error_code: Option<&str>,
    duration_ms: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
) {
    akhub
        .state
        .store
        .insert_request_records(&[akhub::storage::store::RequestRecord {
            request_id: request_id.to_string(),
            started_at: akhub::storage::now_unix(),
            duration_ms,
            protocol: Protocol::OpenAiChat,
            streaming: false,
            group_id: Some(akhub.group_id.clone()),
            logical_model: Some(logical_model.to_string()),
            target_id: target_id.map(str::to_string),
            account_id: None,
            upstream_model: None,
            request_bytes: 100,
            upstream_status: Some(http_status),
            http_status,
            error_code: error_code.map(str::to_string),
            endpoint: Some("chat_completions".to_string()),
            degraded: None,
            effective_multiplier: Some(Multiplier::ONE),
            cheapest_multiplier: None,
            dearest_multiplier: None,
            attempts: 1,
            queued_ms: 0,
            sticky_hit: false,
            first_token_ms: Some(10),
            input_tokens,
            output_tokens,
            config_version: Some(1),
            attempts_detail: Vec::new(),
        }])
        .await
        .unwrap();
}

/// 请求记录支持按模型/状态/错误码筛选，并返回命中总数（§6.6）。
#[tokio::test]
async fn request_records_support_filters_and_total() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    insert_record(
        &akhub,
        "req-a1",
        "m-a",
        Some("t-1"),
        200,
        None,
        12,
        Some(5),
        Some(7),
    )
    .await;
    insert_record(
        &akhub,
        "req-a2",
        "m-a",
        Some("t-1"),
        502,
        Some("upstream_exhausted"),
        30,
        None,
        None,
    )
    .await;
    insert_record(
        &akhub,
        "req-b1",
        "m-b",
        Some("t-2"),
        200,
        None,
        40,
        Some(1),
        Some(1),
    )
    .await;

    let http = client();
    let get = |query: &str| {
        http.get(format!("{}/admin/api/requests?{query}", akhub.base_url))
            .header("cookie", &cookie)
            .send()
    };

    // 按模型筛选。
    let body: Value = get("logical_model=m-a")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["total"], 2, "{body}");
    assert_eq!(body["data"].as_array().unwrap().len(), 2);

    // 只看失败。
    let body: Value = get("status=error").await.unwrap().json().await.unwrap();
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["data"][0]["request_id"], "req-a2");

    // 按错误码 + 目标筛选。
    let body: Value = get("error_code=upstream_exhausted&target_id=t-1")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["total"], 1, "{body}");

    // 按请求 ID 精确命中。
    let body: Value = get("request_id=req-b1")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["data"][0]["logical_model"], "m-b");
}

/// 概览返回运行指标：请求量、成功率、延迟分位、队列超时、最近错误与配置变化（§6.2）。
#[tokio::test]
async fn overview_reports_runtime_metrics() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    for (index, duration) in [10_i64, 20, 30, 40].iter().enumerate() {
        insert_record(
            &akhub,
            &format!("req-ok-{index}"),
            "m-a",
            None,
            200,
            None,
            *duration,
            Some(1),
            Some(1),
        )
        .await;
    }
    insert_record(
        &akhub,
        "req-timeout",
        "m-a",
        None,
        429,
        Some("queue_timeout"),
        500,
        None,
        None,
    )
    .await;

    let overview: Value = client()
        .get(format!("{}/admin/api/overview", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(overview["window_secs"], 86400, "{overview}");
    assert_eq!(overview["requests"], 5, "{overview}");
    assert_eq!(overview["queue_timeouts"], 1, "{overview}");
    assert_eq!(overview["in_flight"], 0, "{overview}");
    let rate = overview["success_rate"].as_f64().unwrap();
    assert!((rate - 0.8).abs() < 0.001, "成功率应为 4/5：{rate}");
    assert!(overview["p50_latency_ms"].as_i64().unwrap() >= 10);
    assert!(overview["p95_latency_ms"].as_i64().unwrap() >= 500);
    assert_eq!(
        overview["recent_errors"].as_array().unwrap().len(),
        1,
        "{overview}"
    );
    assert!(
        overview["recent_changes"].as_array().is_some(),
        "配置变化列表必须存在"
    );
}

/// 分组"可选模型"接口：把该分组下所有账号目录里的模型汇总去重（§6.5）。
#[tokio::test]
async fn group_available_models_aggregates_account_catalogs() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({
        "object": "list",
        "data": [{"id": "glm-5.3-flash"}, {"id": "deepseek-v4-flash"}]
    })));
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "glm-5.3-flash",
            "glm-5.3-flash",
            50,
        ),
    )
    .await;

    // 拉一次目录，让 account_models 里有可选项。
    let http = client();
    write(
        http.post(format!(
            "{}/admin/api/accounts/{}/models/refresh",
            akhub.base_url, wired.account_id
        ))
        .header("cookie", &cookie),
    )
    .send()
    .await
    .unwrap();

    let body: Value = http
        .get(format!(
            "{}/admin/api/groups/{}/available-models",
            akhub.base_url, akhub.group_id
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let models = body["models"].as_array().expect("models 数组");
    let names: Vec<&str> = models
        .iter()
        .filter_map(|m| m["public_name"].as_str())
        .collect();
    assert!(names.contains(&"glm-5.3-flash"), "{body}");
    assert!(names.contains(&"deepseek-v4-flash"), "{body}");
    let first = models
        .iter()
        .find(|m| m["public_name"] == "glm-5.3-flash")
        .unwrap();
    assert!(
        first["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "账号A"),
        "要能看出是哪个账号提供的：{first}"
    );
}

/// 概览趋势：按小时聚合、补齐空桶，请求数正确（§6.2）。
#[tokio::test]
async fn overview_includes_an_hourly_trend_with_empty_buckets_filled() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    // 造两条"当前小时"的记录（其它桶应为 0）。
    insert_record(
        &akhub,
        "trend-1",
        "m-a",
        None,
        200,
        None,
        10,
        Some(1),
        Some(1),
    )
    .await;
    insert_record(
        &akhub,
        "trend-2",
        "m-a",
        None,
        500,
        Some("upstream_exhausted"),
        20,
        None,
        None,
    )
    .await;

    let overview: Value = client()
        .get(format!("{}/admin/api/overview", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(overview["trend_bucket_secs"], 3600, "{overview}");
    let trend = overview["trend"].as_array().expect("trend 数组");
    assert!(
        trend.len() >= 24,
        "24 小时要给出至少 24 个桶，实际 {}",
        trend.len()
    );
    for pair in trend.windows(2) {
        let gap =
            pair[1]["bucket_start"].as_i64().unwrap() - pair[0]["bucket_start"].as_i64().unwrap();
        assert_eq!(gap, 3600, "桶间距必须等于桶宽：{pair:?}");
    }
    let total: i64 = trend
        .iter()
        .map(|point| point["requests"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(total, 2, "趋势里的请求总数要与写入的两条一致");
    let successes: i64 = trend
        .iter()
        .map(|point| point["success"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(successes, 1, "只有 2xx 计入成功");
}
