//! 阶段 5 验收（§26.6）：成本页数字口径与校准助手。
//!
//! 核心口径：成本按逻辑模型分组，绝不跨模型加总；加权均倍率按请求级样本
//! 平均；校准按单模型对账，反算结果不受模型组合影响。

mod common;

use akhub::domain::{Multiplier, Protocol};
use common::{Akhub, FakeUpstream, TargetSpec, spawn_akhub_with, wire_target};
use serde_json::{Value, json};
use std::sync::Arc;

/// 起一台 Akhub 并登录管理员会话。
async fn spawn_admin() -> (Akhub, reqwest::Client) {
    let akhub = spawn_akhub_with(akhub::app::Settings::default(), |_| {}).await;
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let response = client
        .request(
            reqwest::Method::POST,
            format!("{}/admin/api/setup", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"username": "admin", "password": "correct horse battery"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    (akhub, client)
}

static REQ_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn uuid_like() -> u64 {
    REQ_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 在请求记录表里补一条成功请求（成本聚合的直接输入）。
async fn record(akhub: &Akhub, target: &common::Wired, logical_model: &str, effective: &str) {
    record_tokens(akhub, target, logical_model, effective, 100, 20).await;
}

/// 带指定 Token 用量的一条成功请求。
async fn record_tokens(
    akhub: &Akhub,
    target: &common::Wired,
    logical_model: &str,
    effective: &str,
    input_tokens: i64,
    output_tokens: i64,
) {
    let now = akhub::storage::now_unix();
    akhub
        .state
        .store
        .insert_request_records(&[akhub::storage::store::RequestRecord {
            request_id: format!("req-{}", uuid_like()),
            started_at: now - 60,
            duration_ms: 10,
            protocol: Protocol::OpenAiChat,
            streaming: false,
            group_id: Some(akhub.group_id.clone()),
            logical_model: Some(logical_model.into()),
            target_id: Some(target.target_id.clone()),
            account_id: Some(target.account_id.clone()),
            upstream_model: Some("glm-4.6".into()),
            request_bytes: 100,
            upstream_status: Some(200),
            http_status: 200,
            error_code: None,
            endpoint: Some("chat_completions".into()),
            degraded: None,
            effective_multiplier: Some(Multiplier::parse(effective).unwrap()),
            cheapest_multiplier: None,
            dearest_multiplier: None,
            attempts: 1,
            queued_ms: 0,
            sticky_hit: false,
            first_token_ms: None,
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            config_version: None,
            attempts_detail: Vec::new(),
        }])
        .await
        .unwrap();
}

#[tokio::test]
async fn cost_page_groups_by_logical_model_and_never_sums_across_models() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;

    // 两个模型各由一个账号支撑，倍率同为 0.5：跨模型加总与模型内加权在这里
    // 必须给出相同的账号占比（各 50%），但模型行之间绝不出现合计行。
    let a = wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "模型一",
            "m1",
            50,
        )
        .multiplier(akhub::domain::MultiplierMode::Manual, "0.5"),
    )
    .await;
    let b = wire_target(
        &akhub,
        TargetSpec::new(
            "账号B",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "模型二",
            "m2",
            50,
        )
        .multiplier(akhub::domain::MultiplierMode::Manual, "0.5"),
    )
    .await;
    record(&akhub, &a, "模型一", "0.5").await;
    record(&akhub, &a, "模型一", "0.5").await;
    record(&akhub, &b, "模型二", "0.5").await;
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records")
        .fetch_one(akhub.state.store.pool())
        .await
        .unwrap();
    assert_eq!(stored, 3, "三条记录都要落库");

    let response = client
        .get(format!("{}/admin/api/cost?period=day", akhub.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let view: Value = response.json().await.unwrap();

    assert_eq!(view["total_requests"], 3);
    let models = view["models"].as_array().unwrap();
    assert_eq!(models.len(), 2, "两个逻辑模型各成一块：{view}");
    let by_name: std::collections::HashMap<&str, &Value> = models
        .iter()
        .map(|m| (m["logical_model"].as_str().unwrap(), m))
        .collect();
    for name in ["模型一", "模型二"] {
        let model = by_name[name];
        let expected = if name == "模型一" { 2 } else { 1 };
        assert_eq!(model["requests"], expected, "{name} 的请求数：{model}");
        assert_eq!(model["accounts"][0]["share"], 1.0);
        assert_eq!(model["weighted_avg_multiplier"], "0.5");
    }
    // 账号占比是全局口径：各 50%，这是允许显示的量。
    let shares = view["account_shares"].as_array().unwrap();
    assert_eq!(shares.len(), 2);
    let total_share: f64 = shares.iter().map(|s| s["share"].as_f64().unwrap()).sum();
    assert!(
        (total_share - 1.0).abs() < 1e-9,
        "占比合计必须为 1：{shares:?}"
    );

    // 响应里绝不能有跨模型的"总成本"或"总节省"字段。
    let text = view.to_string();
    assert!(!text.contains("total_cost"), "成本页不得出现总成本：{text}");
    assert!(
        !text.contains("total_saving"),
        "成本页不得出现总节省：{text}"
    );
}

#[tokio::test]
async fn weighted_multiplier_uses_request_level_samples_and_saving_is_visible() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;

    // 同一模型两个账号：0.5 拿 3 单，0.8 拿 1 单 → 加权 0.575，最贵 0.8，
    // 全用最便宜还能省 (1 - 0.5/0.575) ≈ 13%。
    let a = wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "glm-4.6",
            "m",
            50,
        )
        .multiplier(akhub::domain::MultiplierMode::Manual, "0.5"),
    )
    .await;
    let b = wire_target(
        &akhub,
        TargetSpec::new(
            "账号B",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "glm-4.6",
            "m",
            50,
        )
        .multiplier(akhub::domain::MultiplierMode::Manual, "0.8"),
    )
    .await;
    for _ in 0..3 {
        record(&akhub, &a, "glm-4.6", "0.5").await;
    }
    record(&akhub, &b, "glm-4.6", "0.8").await;

    let view: Value = client
        .get(format!("{}/admin/api/cost?period=day", akhub.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let model = view["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["logical_model"] == "glm-4.6")
        .expect("glm-4.6 应该在成本页里");
    assert_eq!(model["weighted_avg_multiplier"], "0.575");
    assert_eq!(model["cheapest_multiplier"], "0.5");
    assert_eq!(model["dearest_multiplier"], "0.8");
    let saving = model["saving_vs_cheapest"].as_f64().unwrap();
    assert!(
        (saving - (1.0 - 0.5 / 0.575)).abs() < 1e-4,
        "相对最贵/最便宜的口径必须精确到展示位数：{saving}"
    );
    assert_eq!(model["single_target"], false);

    // 账号行显示当前有效倍率与流量占比。
    let accounts = model["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0]["requests"], 3, "按请求数排序");
}

#[tokio::test]
async fn calibration_is_per_model_and_unaffected_by_other_models() {
    // §26.6：校准助手按单模型对账，反算结果不受模型组合影响。
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;

    // 一个账号两个模型：模型 A 记录的有效倍率 0.5，模型 B 记录 0.9。
    let wired_a = wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "模型A",
            "ma",
            50,
        )
        .multiplier(akhub::domain::MultiplierMode::Manual, "0.5"),
    )
    .await;
    let wired_b = wire_target(
        &akhub,
        TargetSpec::new(
            "账号A-2",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "模型B",
            "mb",
            50,
        )
        .multiplier(akhub::domain::MultiplierMode::Manual, "0.9"),
    )
    .await;
    for _ in 0..4 {
        record(&akhub, &wired_a, "模型A", "0.5").await;
    }
    for _ in 0..2 {
        record(&akhub, &wired_b, "模型B", "0.9").await;
    }

    let calibrate = |model: &str, reported: &str| {
        let base = akhub.base_url.clone();
        let key = client.clone();
        let model = model.to_string();
        let reported = reported.to_string();
        let account = wired_a.account_id.clone();
        async move {
            key.request(
                reqwest::Method::POST,
                format!("{base}/admin/api/accounts/{account}/calibrate"),
            )
            .header("x-akhub-csrf", "1")
            .json(&json!({"logical_model": model, "reported": reported}))
            .send()
            .await
            .unwrap()
        }
    };

    // 站点报 0.4：模型 A 的均倍率是 0.5 → 系数 0.4/0.5 = 0.8。
    let response = calibrate("模型A", "0.4").await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["calibration"], "0.8");
    assert_eq!(body["group_avg_multiplier"], "0.5");
    assert_eq!(body["gateway_requests"], 4);
    assert_eq!(body["exclusive"], json!(true));

    // 换模型 B 对账：均倍率 0.9，同一站点报值下系数完全不同——证明对账
    // 没有被模型 A 的数据污染。
    let response = calibrate("模型B", "0.9").await;
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["group_avg_multiplier"], "0.9");
    assert_eq!(body["calibration"], "1");

    // 区间内没流量的模型不能对账。
    let response = calibrate("没流量的模型", "0.5").await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("没有成功请求"));

    // 对账记录留痕。
    let records: Value = client
        .get(format!(
            "{}/admin/api/accounts/{}/calibrations",
            akhub.base_url, wired_a.account_id
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(records["data"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn copy_creates_an_independent_disabled_clone() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({"data": [{"id": "glm-4.6"}]})));

    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "站点A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "glm-4.6",
            "glm-4.6",
            60,
        )
        .multiplier(akhub::domain::MultiplierMode::Manual, "0.7"),
    )
    .await;
    let account_id = wired.account_id.clone();

    // 建选择集与别名，复制后两者都要跟着走。
    let response = client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{account_id}/models/refresh",
                akhub.base_url
            ),
        )
        .header("x-akhub-csrf", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{account_id}/models/select",
                akhub.base_url
            ),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"selected": ["glm-4.6"]}))
        .send()
        .await
        .unwrap();
    client
        .request(
            reqwest::Method::PUT,
            format!("{}/admin/api/accounts/{account_id}/aliases", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"aliases": [
            {"upstream_model": "glm-4.6", "public_name": "glm"}
        ]}))
        .send()
        .await
        .unwrap();

    let response = client
        .request(
            reqwest::Method::POST,
            format!("{}/admin/api/accounts/{account_id}/copy", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201, "{}", response.text().await.unwrap());
    let copy: Value = response.json().await.unwrap();
    assert_eq!(copy["name"], "站点A - 副本");
    assert_eq!(copy["enabled"], json!(false), "副本必须是停用状态");
    assert_ne!(copy["id"], account_id);

    let copy_id = copy["id"].as_str().unwrap().to_string();
    let accounts = akhub.state.store.list_accounts().await.unwrap();
    let original = accounts.iter().find(|a| a.id == account_id).unwrap();
    let copied = accounts.iter().find(|a| a.id == copy_id).unwrap();
    assert_eq!(copied.default_priority, original.default_priority);
    assert_eq!(copied.multiplier_mode, original.multiplier_mode);
    assert_eq!(copied.manual_multiplier, original.manual_multiplier);
    assert_eq!(copied.limits, original.limits);

    // Key 重新加密：两条记录的密文不同但解出的明文相同（§20.4）。
    let sealed_original = akhub
        .state
        .store
        .account_sealed_key(&account_id)
        .await
        .unwrap()
        .unwrap();
    let sealed_copy = akhub
        .state
        .store
        .account_sealed_key(&copy_id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        sealed_original, sealed_copy,
        "复制必须重新加密，不能共享密文"
    );
    let key = akhub.state.cipher.open(&sealed_copy).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&key),
        format!("key-{}", "站点A"),
        "副本的 Key 明文必须一致"
    );

    // 选择集、别名与调度目标随副本独立一份。
    let models = akhub
        .state
        .store
        .list_account_models(&copy_id)
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert!(models[0].selected);
    let aliases = akhub
        .state
        .store
        .list_account_aliases(&copy_id)
        .await
        .unwrap();
    assert_eq!(aliases.len(), 1);
    let targets = akhub.state.store.list_targets().await.unwrap();
    assert_eq!(
        targets.iter().filter(|t| t.account_id == copy_id).count(),
        1,
        "副本要有自己的调度目标"
    );
}

#[tokio::test]
async fn test_connection_sends_a_real_request_without_polluting_statistics() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;

    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "站点A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "glm-4.6",
            "glm-4.6",
            50,
        ),
    )
    .await;

    // 测试通过：真发了一次 hi。
    let response = client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{}/test",
                akhub.base_url, wired.account_id
            ),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"model": "glm-4.6"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["ok"], json!(true), "{body}");
    assert_eq!(body["status"], 200);
    let seen = upstream.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(seen.path, "/v1/chat/completions");
    assert_eq!(seen.body["messages"][0]["content"], "hi");

    // 测试数据不进请求记录、不进端点证据（§6.4）。
    let records: Vec<_> = akhub.state.store.list_request_records(50, 0).await.unwrap();
    assert!(records.is_empty(), "测试请求不能进入统计：{records:?}");

    // 上游拒绝时返回结构化失败而不是 500。
    upstream.fallback(common::Behavior::Status(401, None));
    let response = client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{}/test",
                akhub.base_url, wired.account_id
            ),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"model": "glm-4.6"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["status"], 401);
    let _ = Arc::new(());
}

#[tokio::test]
async fn backup_import_rejects_cross_group_targets_without_changing_configuration() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "backup",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "m",
            "m",
            50,
        ),
    )
    .await;
    let mut other = akhub.state.store.list_groups().await.unwrap()[0].clone();
    other.id = "other-group".into();
    other.name = "other-group".into();
    other.key_digest_hex = "other-digest".into();
    akhub.state.store.insert_group(&other).await.unwrap();
    // 模拟经手工修改的配置：目标与模型未动，账号已经属于另一个分组。
    sqlx::query("UPDATE upstream_accounts SET group_id = ? WHERE id = ?")
        .bind(&other.id)
        .bind(&wired.account_id)
        .execute(akhub.state.store.pool())
        .await
        .unwrap();
    let backup = akhub::security::backup::export_backup(
        &akhub.state.store,
        &akhub.state.cipher,
        "test-backup-password",
    )
    .await
    .unwrap();
    let version = akhub.state.config.version();
    let response = client
        .post(format!("{}/admin/api/backup/import", akhub.base_url))
        .header("x-akhub-csrf", "1")
        .json(&json!({"password":"test-backup-password", "content":String::from_utf8(backup).unwrap()}))
        .send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert!(response.text().await.unwrap().contains("不属于同一分组"));
    assert_eq!(akhub.state.config.version(), version);
    assert_eq!(akhub.state.store.list_groups().await.unwrap().len(), 2);
    assert_eq!(akhub.state.store.list_targets().await.unwrap().len(), 1);
}

#[tokio::test]
async fn backup_roundtrip_restores_the_full_configuration() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({"data": [{"id": "glm-4.6"}]})));

    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "站点A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "glm-4.6",
            "glm-4.6",
            60,
        )
        .multiplier(akhub::domain::MultiplierMode::Manual, "0.7"),
    )
    .await;
    let account_id = wired.account_id.clone();

    // 建选择集，随后导出。
    client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{account_id}/models/refresh",
                akhub.base_url
            ),
        )
        .header("x-akhub-csrf", "1")
        .send()
        .await
        .unwrap();
    client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{account_id}/models/select",
                akhub.base_url
            ),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"selected": ["glm-4.6"]}))
        .send()
        .await
        .unwrap();

    let response = client
        .request(
            reqwest::Method::POST,
            format!("{}/admin/api/backup/export", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"password": "备份口令"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let backup_text = response.text().await.unwrap();
    assert!(
        backup_text.contains("ciphertext"),
        "响应应该是备份信封：{backup_text}"
    );

    // 备份文件里绝不能出现明文 Key（§26.8）。
    assert!(
        !backup_text.contains("key-站点A"),
        "明文 Key 不得出现在备份文件里"
    );

    // 改坏当前配置（删光分组），再恢复。
    for group in akhub.state.store.list_groups().await.unwrap() {
        akhub.state.store.delete_group(&group.id).await.unwrap();
    }
    akhub.state.reload_config().await.unwrap();
    assert!(akhub.state.store.list_groups().await.unwrap().is_empty());

    // 错误密码：明确报错且现有配置不动。
    let response = client
        .request(
            reqwest::Method::POST,
            format!("{}/admin/api/backup/import", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"password": "错误口令", "content": backup_text}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(akhub.state.store.list_groups().await.unwrap().is_empty());

    // 正确密码：整体恢复。
    let response = client
        .request(
            reqwest::Method::POST,
            format!("{}/admin/api/backup/import", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"password": "备份口令", "content": backup_text}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["groups"], 1);
    assert_eq!(body["accounts"], 1);

    let accounts = akhub.state.store.list_accounts().await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "站点A");
    // Key 用本机主密钥重新加密：解出的明文必须与原来一致。
    let sealed = akhub
        .state
        .store
        .account_sealed_key(&accounts[0].id)
        .await
        .unwrap()
        .unwrap();
    let key = akhub.state.cipher.open(&sealed).unwrap();
    assert_eq!(String::from_utf8_lossy(&key), "key-站点A");
    // 选择集跟着回来。
    let models = akhub
        .state
        .store
        .list_account_models(&accounts[0].id)
        .await
        .unwrap();
    assert_eq!(models.len(), 1, "models: {models:?}");
    assert!(models[0].selected, "models: {models:?}");
    let targets = akhub.state.store.list_targets().await.unwrap();
    assert_eq!(targets.len(), 1);
    // 配置快照重建后调度可用。
    let config = akhub.state.config.current();
    let group = config.groups.first().expect("分组应恢复");
    assert_eq!(
        group.models.get("glm-4.6").map(|m| m.targets.len()),
        Some(1)
    );
}

/// 成本页要显示 Token 用量，并在有 Token 时按 Token 计算占比（§6.8）。
#[tokio::test]
async fn cost_page_reports_tokens_and_uses_them_for_shares() {
    let (akhub, client) = spawn_admin().await;
    let upstream_a = FakeUpstream::spawn().await;
    let upstream_b = FakeUpstream::spawn().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream_a.base_url,
            Protocol::OpenAiChat,
            "m1",
            "m1",
            100,
        ),
    )
    .await;
    let b = wire_target(
        &akhub,
        TargetSpec::new(
            "账号B",
            &upstream_b.base_url,
            Protocol::OpenAiChat,
            "m1",
            "m1",
            100,
        ),
    )
    .await;

    // A：3 次请求、每次 10 token；B：1 次请求、90 token。
    // 请求占比 75%/25%，Token 占比 25%/75%——口径必须按 Token 算。
    for _ in 0..3 {
        record_tokens(&akhub, &a, "m1", "0.5", 6, 4).await;
    }
    record_tokens(&akhub, &b, "m1", "0.5", 60, 30).await;

    let view: Value = client
        .get(format!("{}/admin/api/cost?period=day", akhub.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(view["share_basis"], "tokens", "{view}");
    assert_eq!(view["total_tokens"], 120, "{view}");
    assert_eq!(view["total_requests"], 4, "{view}");
    let model = &view["models"][0];
    assert_eq!(model["tokens"], 120, "{model}");
    let accounts = model["accounts"].as_array().unwrap();
    let share_a = accounts
        .iter()
        .find(|row| row["account_id"] == a.account_id)
        .unwrap()["share"]
        .as_f64()
        .unwrap();
    let share_b = accounts
        .iter()
        .find(|row| row["account_id"] == b.account_id)
        .unwrap()["share"]
        .as_f64()
        .unwrap();
    assert!(
        (share_a - 0.25).abs() < 0.001,
        "A 的 Token 占比应为 25%：{share_a}"
    );
    assert!(
        (share_b - 0.75).abs() < 0.001,
        "B 的 Token 占比应为 75%：{share_b}"
    );
}
