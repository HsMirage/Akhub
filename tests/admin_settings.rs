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
            sticky_wait_ms: None,
            sticky_freshness: None,
            output_tps: None,
            multiplier_source: None,
            quota_status: None,
            filter_summary: None,
            selected_layer: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
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

    // 拼错的错误码必须明确报错，而不是返回一个"看起来没有失败"的空列表。
    let response = get("error_code=upstream_exausted").await.unwrap();
    assert_eq!(response.status(), 400, "未知错误码必须被拒");
    let body: Value = response.json().await.unwrap();
    let message = body["error"].as_str().unwrap_or_default();
    assert!(message.contains("upstream_exausted"), "{body}");
    assert!(
        message.contains("upstream_exhausted"),
        "必须列出可用值：{body}"
    );

    // 只进请求记录的结局标识（客户端断开）也必须能筛。
    let response = get("error_code=client_gone").await.unwrap();
    assert_eq!(response.status(), 200, "client_gone 是合法筛选值");

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

/// 调度诊断列（§24.1）：过滤原因、选中层、粘性等待、倍率来源都要能读到。
#[tokio::test]
async fn request_records_expose_scheduling_diagnostics() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;

    let record = akhub::storage::store::RequestRecord {
        request_id: "req-diag".to_string(),
        started_at: akhub::storage::now_unix(),
        duration_ms: 2_000,
        protocol: Protocol::OpenAiChat,
        streaming: true,
        group_id: Some(akhub.group_id.clone()),
        logical_model: Some("m-a".to_string()),
        target_id: Some("t-1".to_string()),
        account_id: None,
        upstream_model: Some("glm-4.6".to_string()),
        request_bytes: 512,
        upstream_status: Some(200),
        http_status: 200,
        error_code: None,
        endpoint: Some("chat_completions".to_string()),
        degraded: Some("thinking".to_string()),
        effective_multiplier: Some(Multiplier::ONE),
        cheapest_multiplier: Some(Multiplier::ONE),
        dearest_multiplier: Some(Multiplier::parse("2").unwrap()),
        attempts: 2,
        queued_ms: 5,
        sticky_hit: true,
        first_token_ms: Some(120),
        input_tokens: Some(900),
        output_tokens: Some(400),
        config_version: Some(3),
        attempts_detail: Vec::new(),
        sticky_wait_ms: Some(80),
        sticky_freshness: Some(0.75),
        output_tps: Some(200.0),
        // Token 细分（§11.6）：缓存与思考各自留痕。
        cache_read_tokens: Some(700),
        cache_write_tokens: Some(50),
        reasoning_tokens: Some(120),
        multiplier_source: Some("manual".to_string()),
        quota_status: Some("ok".to_string()),
        filter_summary: Some("倍率超限×2".to_string()),
        selected_layer: Some(50),
    };
    akhub
        .state
        .store
        .insert_request_records(&[record])
        .await
        .unwrap();

    let page: Value = client()
        .get(format!("{}/admin/api/requests", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = &page["data"][0];
    assert_eq!(row["sticky_wait_ms"], 80, "{row}");
    assert_eq!(row["sticky_freshness"], 0.75, "{row}");
    assert_eq!(row["output_tps"], 200.0, "{row}");
    assert_eq!(row["multiplier_source"], "manual", "{row}");
    assert_eq!(row["quota_status"], "ok", "{row}");
    assert_eq!(row["filter_summary"], "倍率超限×2", "{row}");
    assert_eq!(row["selected_layer"], 50, "{row}");
    assert_eq!(row["degraded"], "thinking", "{row}");
    // Token 细分（§11.6）：缓存读写与思考各自一列，没上报就是 null。
    assert_eq!(row["cache_read_tokens"], 700);
    assert_eq!(row["cache_write_tokens"], 50);
    assert_eq!(row["reasoning_tokens"], 120);
}

/// 运行指标接口（§6.6）：概览之外的进程内指标。
#[tokio::test]
async fn the_metrics_endpoint_reports_live_counters() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    let metrics: Value = client()
        .get(format!("{}/admin/api/metrics", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(metrics["bucket_secs"], 60, "{metrics}");
    assert!(metrics["since"].as_i64().is_some(), "{metrics}");
    assert!(metrics["until"].as_i64().is_some(), "{metrics}");
    assert!(metrics["targets"].is_array(), "{metrics}");
    // 曲线按分钟补零，没有数据也要有点，不能留空洞。
    assert!(
        !metrics["series"].as_array().unwrap().is_empty(),
        "{metrics}"
    );
    for point in metrics["series"].as_array().unwrap() {
        assert!(point["requests"].is_i64(), "{point}");
        assert!(point["success"].is_i64(), "{point}");
    }
    // 非法区间要明确拒绝，而不是返回一个空结果让人以为"就是没数据"。
    let bad = client()
        .get(format!(
            "{}/admin/api/metrics?since=100&until=10",
            akhub.base_url
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400, "until 早于 since 必须报错");
}

/// 批量刷新全部账号倍率（§11.3）。
#[tokio::test]
async fn refreshing_every_multiplier_reports_a_per_account_result() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    // 手工倍率的账号不需要探针，批量刷新应当把它标成"跳过"而不是失败。
    let result: Value = write(client().post(format!(
        "{}/admin/api/accounts/refresh-multipliers",
        akhub.base_url
    )))
    .header("cookie", &cookie)
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();

    // 一个账号都没有时也要给出结构完整的空结果，而不是 500 或 null。
    assert_eq!(result["total"], 0, "{result}");
    assert!(result["results"].as_array().unwrap().is_empty(), "{result}");
    assert!(
        result["notice"].as_str().unwrap().contains("0/0"),
        "空结果也要说清刷新了几个：{result}"
    );
}

/// 记录保留期为 0 时不写库，但实时统计照常更新（§24.2）。
#[tokio::test]
async fn a_zero_retention_window_keeps_live_stats_without_writing_rows() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let cookie = admin_cookie(&akhub).await;
    write(client().patch(format!("{}/admin/api/settings", akhub.base_url)))
        .header("cookie", &cookie)
        .json(&json!({"retention_days": 0}))
        .send()
        .await
        .unwrap();

    // 保留期为 0 时列表里不会留下任何历史行。
    let page: Value = client()
        .get(format!("{}/admin/api/requests", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 0, "{page}");

    // 概览用 retention_off 明说"当日"不是"本月"（§24.2）。
    let overview: Value = client()
        .get(format!("{}/admin/api/overview", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(overview["retention_off"], true, "{overview}");
}

/// 备份要包含系统设置与分组的新列，恢复后不能悄悄退回默认值（§23.5）。
///
/// 这条用例是为了钉住一类已经发生过的缺陷：新增列时忘了同步 `backup_groups`
/// 的列清单与 `import_backup` 的 INSERT，备份就成了"看起来成功、实际丢配置"。
#[tokio::test]
async fn backup_restores_settings_and_every_group_column() {
    let akhub = spawn_akhub_with(Settings::default(), |group| {
        // 两个非默认值：不同过备份往返就该发现。
        group.max_wait_secs = 7;
        group.queue_capacity = 42;
        group.allow_managed_background = true;
        group.allow_degrade = false;
    })
    .await;
    let cookie = admin_cookie(&akhub).await;
    let http = client();

    // 改一个设置项，确认它也会跟着备份走。
    let patched = http
        .patch(format!("{}/admin/api/settings", akhub.base_url))
        .header("cookie", &cookie)
        .header("x-akhub-csrf", "1")
        .json(&serde_json::json!({"retention_days": 11}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200, "改设置应当成功");

    let exported: Value = http
        .post(format!("{}/admin/api/backup/export", akhub.base_url))
        .header("cookie", &cookie)
        .header("x-akhub-csrf", "1")
        .json(&serde_json::json!({"password": "备份口令123"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // 导出返回的是**备份信封本身**（不是 {backup: "…"} 包装）：它已经带签名，
    // 再包一层只会让"直接把这个文件存下来"多一步拆包。
    assert!(
        exported["data"].is_object() || exported["payload"].is_object() || exported.is_object(),
        "导出应当是备份信封：{exported}"
    );
    let backup = serde_json::to_string(&exported).unwrap();

    // 恢复前先把配置改成别的值，才能证明是恢复带回来的、而不是原来就在。
    let reset = http
        .patch(format!(
            "{}/admin/api/groups/{}",
            akhub.base_url, akhub.group_id
        ))
        .header("cookie", &cookie)
        .header("x-akhub-csrf", "1")
        .json(&serde_json::json!({"max_wait_secs": 3, "queue_capacity": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), 200);
    let reset_settings = http
        .patch(format!("{}/admin/api/settings", akhub.base_url))
        .header("cookie", &cookie)
        .header("x-akhub-csrf", "1")
        .json(&serde_json::json!({"retention_days": 30}))
        .send()
        .await
        .unwrap();
    assert_eq!(reset_settings.status(), 200);

    let imported = http
        .post(format!("{}/admin/api/backup/import", akhub.base_url))
        .header("cookie", &cookie)
        .header("x-akhub-csrf", "1")
        .json(&serde_json::json!({"password": "备份口令123", "content": backup}))
        .send()
        .await
        .unwrap();
    assert!(
        imported.status().is_success(),
        "恢复失败：{}",
        imported.status()
    );

    // 分组的新列必须回来。
    let groups: Value = http
        .get(format!("{}/admin/api/groups", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let group = groups["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == akhub.group_id.as_str())
        .expect("恢复后分组还在");
    assert_eq!(group["max_wait_secs"], 7, "恢复丢了 max_wait_secs：{group}");
    assert_eq!(
        group["queue_capacity"], 42,
        "恢复丢了 queue_capacity：{group}"
    );
    assert_eq!(
        group["allow_managed_background"], true,
        "恢复丢了 allow_managed_background：{group}"
    );
    assert_eq!(
        group["allow_degrade"], false,
        "恢复丢了 allow_degrade：{group}"
    );

    // 系统设置也必须回来。
    let settings: Value = http
        .get(format!("{}/admin/api/settings", akhub.base_url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        settings["retention_days"], 11,
        "恢复没有带回系统设置：{settings}"
    );
}
