//! 阶段 4 验收（§26.2、§27 阶段 4）：模型发现与选择集。
//!
//! 核心验收点：8 账号 × 20 模型的配置从约 170 次手工操作降到约 10 次——
//! 建账号、点一次"获取模型"、批量勾选，调度目标自动生成与归并（§16.3）。
//! 别名归并、消失标记、手动模型与流量确认也在本文件覆盖。

mod common;

use akhub::domain::Protocol;
use common::{Akhub, FakeUpstream, TargetSpec, spawn_akhub_with, wire_target};
use serde_json::{Value, json};
use std::collections::HashMap;

/// 起一台 Akhub 并登录管理员会话，返回 (地址, 带 Cookie 的客户端)。
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

/// 建一个指向假上游的账号，返回账号 ID。
async fn create_account(
    akhub: &Akhub,
    client: &reqwest::Client,
    upstream: &FakeUpstream,
    name: &str,
) -> String {
    let response = client
        .request(
            reqwest::Method::POST,
            format!("{}/admin/api/accounts", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({
            "group_id": akhub.group_id,
            "name": name,
            "upstream_type": "openai_compatible",
            "base_url": upstream.base_url,
            "api_key": format!("key-{name}"),
            "preferred_protocol": Protocol::OpenAiChat.as_str(),
            "allow_private_network": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        201,
        "建账号失败：{}",
        response.text().await.unwrap()
    );
    let body: Value = response.json().await.unwrap();
    body["id"].as_str().unwrap().to_string()
}

/// 调"获取模型"并断言成功。
async fn refresh(client: &reqwest::Client, akhub: &Akhub, account_id: &str) -> Vec<Value> {
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
    assert_eq!(
        response.status(),
        200,
        "拉取模型失败：{}",
        response.text().await.unwrap()
    );
    response.json().await.unwrap()
}

/// 批量应用选择集。
async fn select(
    client: &reqwest::Client,
    akhub: &Akhub,
    account_id: &str,
    selected: &[&str],
    force: bool,
) -> reqwest::Response {
    client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{account_id}/models/select",
                akhub.base_url
            ),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({
            "selected": selected,
            "force": force,
        }))
        .send()
        .await
        .unwrap()
}

async fn logical_models(akhub: &Akhub) -> HashMap<String, Vec<(String, String)>> {
    // 从存储直读，保证断言的是落库结果而不是视图缓存。
    let models = akhub.state.store.list_logical_models().await.unwrap();
    let targets = akhub.state.store.list_targets().await.unwrap();
    let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for model in &models {
        for target in &targets {
            if target.logical_model_id == model.id {
                map.entry(model.name.clone())
                    .or_default()
                    .push((target.account_id.clone(), target.upstream_model.clone()));
            }
        }
    }
    map
}

#[tokio::test]
async fn fetching_models_then_selecting_builds_merged_dispatch_targets() {
    let (akhub, client) = spawn_admin().await;
    let a = FakeUpstream::spawn().await;
    let b = FakeUpstream::spawn().await;
    // 两个站点都有 glm-4.6；A 独有 claude-sonnet-4-5。
    a.set_models(Some(json!({
        "data": [{"id": "glm-4.6"}, {"id": "claude-sonnet-4-5"}]
    })));
    b.set_models(Some(json!({
        "data": [{"id": "glm-4.6"}, {"id": "gpt-4o"}]
    })));

    let account_a = create_account(&akhub, &client, &a, "站点A").await;
    let account_b = create_account(&akhub, &client, &b, "站点B").await;

    let entries = refresh(&client, &akhub, &account_a).await;
    assert_eq!(entries.len(), 2);

    // A 全选。
    let response = select(
        &client,
        &akhub,
        &account_a,
        &["glm-4.6", "claude-sonnet-4-5"],
        false,
    )
    .await;
    assert_eq!(response.status(), 200);

    // B 勾选 glm-4.6 时，同名逻辑模型已存在 → 归并为新候选目标（§16.3）。
    refresh(&client, &akhub, &account_b).await;
    let response = select(&client, &akhub, &account_b, &["glm-4.6"], false).await;
    assert_eq!(response.status(), 200);

    let models = logical_models(&akhub).await;
    let glm = models.get("glm-4.6").expect("glm-4.6 逻辑模型应自动创建");
    assert_eq!(
        glm.len(),
        2,
        "两个账号的 glm-4.6 应归并到同一逻辑模型：{models:?}"
    );
    let sonnet = models.get("claude-sonnet-4-5").expect("独有模型也要建");
    assert_eq!(sonnet.len(), 1);
    assert!(!models.contains_key("gpt-4o"), "没勾的不建目标");

    // 目标优先级继承账号默认人工优先级（§16.3）：账号默认 50。
    let targets = akhub.state.store.list_targets().await.unwrap();
    assert!(targets.iter().all(|t| t.priority_override.is_none()));
    let config = akhub.state.config.current();
    let view = config
        .groups
        .iter()
        .find(|g| g.group.id == akhub.group_id)
        .unwrap()
        .models
        .get("glm-4.6")
        .unwrap();
    assert_eq!(view.targets.len(), 2);
    assert!(view.targets.iter().all(|t| t.priority == 50));
}

#[tokio::test]
async fn aliases_merge_the_same_model_from_different_sites() {
    let (akhub, client) = spawn_admin().await;
    let a = FakeUpstream::spawn().await;
    let b = FakeUpstream::spawn().await;
    a.set_models(Some(
        json!({"data": [{"id": "claude-sonnet-4-5-20250929"}]}),
    ));
    b.set_models(Some(
        json!({"data": [{"id": "anthropic/claude-sonnet-4.5"}]}),
    ));

    let account_a = create_account(&akhub, &client, &a, "站点A").await;
    let account_b = create_account(&akhub, &client, &b, "站点B").await;

    // 别名在下一次拉取时生效（§16.4）。
    let response = client
        .request(
            reqwest::Method::PUT,
            format!("{}/admin/api/accounts/{account_a}/aliases", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"aliases": [
            {"upstream_model": "claude-sonnet-4-5-20250929", "public_name": "claude-sonnet-4-5"}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204, "{}", response.text().await.unwrap());
    let response = client
        .request(
            reqwest::Method::PUT,
            format!("{}/admin/api/accounts/{account_b}/aliases", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"aliases": [
            {"upstream_model": "anthropic/claude-sonnet-4.5", "public_name": "claude-sonnet-4-5"}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);

    for (account, name) in [(&account_a, "站点A"), (&account_b, "站点B")] {
        let entries = refresh(&client, &akhub, account).await;
        assert_eq!(entries[0]["public_name"], "claude-sonnet-4-5", "{name}");
    }

    // 各自勾选，归并到同一个逻辑模型。
    select(&client, &akhub, &account_a, &["claude-sonnet-4-5"], false).await;
    select(&client, &akhub, &account_b, &["claude-sonnet-4-5"], false).await;
    let models = logical_models(&akhub).await;
    let merged = models
        .get("claude-sonnet-4-5")
        .expect("别名应归并到同一逻辑模型");
    assert_eq!(merged.len(), 2, "{merged:?}");
    assert!(!models.contains_key("claude-sonnet-4-5-20250929"));

    // 下游看到的是对外名（§16.4）。
    let response = client
        .get(format!("{}/v1/models", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    let names: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(names, vec!["claude-sonnet-4-5"]);
}

#[tokio::test]
async fn a_failed_fetch_keeps_the_existing_catalog() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({"data": [{"id": "glm-4.6"}]})));
    let account = create_account(&akhub, &client, &upstream, "站点A").await;
    refresh(&client, &akhub, &account).await;
    select(&client, &akhub, &account, &["glm-4.6"], false).await;

    // 上游挂了：保留原列表和选择集，只报错（§16.1）。
    upstream.set_models(None);
    let response = client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{account}/models/refresh",
                akhub.base_url
            ),
        )
        .header("x-akhub-csrf", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);

    let catalog: Vec<Value> = client
        .get(format!(
            "{}/admin/api/accounts/{account}/models",
            akhub.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0]["selected"], true);
    assert_eq!(
        logical_models(&akhub).await.get("glm-4.6").map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn vanished_models_are_marked_and_manual_models_survive() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({
        "data": [{"id": "glm-4.6"}, {"id": "gpt-4o"}]
    })));
    let account = create_account(&akhub, &client, &upstream, "站点A").await;
    refresh(&client, &akhub, &account).await;
    select(&client, &akhub, &account, &["glm-4.6", "gpt-4o"], false).await;

    // glm-4.6 从上游消失（§16.5）：保留记录、标记 missing、目标停用。
    upstream.set_models(Some(json!({"data": [{"id": "gpt-4o"}]})));
    let entries = refresh(&client, &akhub, &account).await;
    let glm = entries
        .iter()
        .find(|e| e["upstream_model"] == "glm-4.6")
        .expect("已选的消失模型必须保留");
    assert_eq!(glm["missing"], true);
    assert_eq!(glm["selected"], true);

    let targets = akhub.state.store.list_targets().await.unwrap();
    let glm_target = targets
        .iter()
        .find(|t| t.upstream_model == "glm-4.6")
        .unwrap();
    assert!(!glm_target.enabled, "消失的模型必须停止新请求");

    // 重新出现自动解除（§16.5）。
    upstream.set_models(Some(json!({"data": [{"id": "glm-4.6"}, {"id": "gpt-4o"}]})));
    let entries = refresh(&client, &akhub, &account).await;
    let glm = entries
        .iter()
        .find(|e| e["upstream_model"] == "glm-4.6")
        .unwrap();
    assert_eq!(glm["missing"], false);
    let targets = akhub.state.store.list_targets().await.unwrap();
    let glm_target = targets
        .iter()
        .find(|t| t.upstream_model == "glm-4.6")
        .unwrap();
    assert!(glm_target.enabled, "重现后自动恢复");

    // 模型列表接口不可用时手动添加（§16.5）。
    upstream.set_models(None);
    let response = client
        .request(
            reqwest::Method::POST,
            format!("{}/admin/api/accounts/{account}/models", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"upstream_model": "自定义-内部模型"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204, "{}", response.text().await.unwrap());
    let targets = akhub.state.store.list_targets().await.unwrap();
    assert!(
        targets
            .iter()
            .any(|t| t.upstream_model == "自定义-内部模型"),
        "手动模型立即成为调度目标"
    );

    // 再拉取失败不会覆盖手动模型；恢复后手动模型也不该被一次拉取删掉。
    let response = client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{account}/models/refresh",
                akhub.base_url
            ),
        )
        .header("x-akhub-csrf", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    upstream.set_models(Some(json!({"data": [{"id": "gpt-4o"}]})));
    refresh(&client, &akhub, &account).await;
    let targets = akhub.state.store.list_targets().await.unwrap();
    assert!(
        targets
            .iter()
            .any(|t| t.upstream_model == "自定义-内部模型"),
        "真实拉取不认识的手动模型不能被覆盖删除"
    );
}

#[tokio::test]
async fn unselecting_a_traffic_heavy_model_requires_confirmation() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({"data": [{"id": "glm-4.6"}]})));
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "旧目标",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "glm-4.6",
            "glm-4.6",
            50,
        ),
    )
    .await;
    let account = wired.account_id.clone();

    // 最近 24 小时有一条成功流量（§16.3 的警告窗口）。
    let now = akhub::storage::now_unix();
    akhub
        .state
        .store
        .insert_request_records(&[akhub::storage::store::RequestRecord {
            request_id: "req-1".into(),
            started_at: now - 60,
            duration_ms: 10,
            protocol: Protocol::OpenAiChat,
            streaming: false,
            group_id: Some(akhub.group_id.clone()),
            logical_model: Some("glm-4.6".into()),
            target_id: Some(wired.target_id.clone()),
            account_id: Some(account.clone()),
            upstream_model: Some("glm-4.6".into()),
            request_bytes: 100,
            upstream_status: Some(200),
            http_status: 200,
            error_code: None,
            endpoint: Some("chat_completions".into()),
            degraded: None,
            effective_multiplier: None,
            cheapest_multiplier: None,
            dearest_multiplier: None,
            attempts: 1,
            queued_ms: 0,
            sticky_hit: false,
            first_token_ms: None,
            input_tokens: None,
            output_tokens: None,
            config_version: None,
            attempts_detail: Vec::new(),
        }])
        .await
        .unwrap();

    // 目录里先有这条记录。
    upstream.set_models(Some(json!({"data": [{"id": "glm-4.6"}]})));
    refresh(&client, &akhub, &account).await;

    // 取消勾选：409 + 调用次数，目录不变。
    let response = select(&client, &akhub, &account, &[], false).await;
    assert_eq!(response.status(), 409);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["warnings"][0]["public_name"], "glm-4.6");
    assert_eq!(body["warnings"][0]["calls"], 1);
    assert_eq!(
        logical_models(&akhub).await.get("glm-4.6").map(Vec::len),
        Some(1),
        "未确认前绝不改动"
    );

    // force 确认后目标移除。
    let response = select(&client, &akhub, &account, &[], true).await;
    assert_eq!(response.status(), 200);
    assert!(!logical_models(&akhub).await.contains_key("glm-4.6"));
}

#[tokio::test]
async fn eight_accounts_times_twenty_models_take_about_ten_operations() {
    // §16.3 验收：8 账号 × 20 模型的配置从约 170 次手工操作降到约 10 次。
    // 每个账号：建账号(1) + 拉取(1) + 全选(1)；最后断言 8 × 20 个目标全部就位，
    // 且同名模型正确归并到同一个逻辑模型。
    let (akhub, client) = spawn_admin().await;
    let mut upstreams = Vec::new();
    for _ in 0..8 {
        let upstream = FakeUpstream::spawn().await;
        upstream.set_models(Some(json!({
            "data": (0..20).map(|i| json!({"id": format!("model-{i:02}")})).collect::<Vec<_>>()
        })));
        upstreams.push(upstream);
    }

    for (index, upstream) in upstreams.iter().enumerate() {
        let account = create_account(&akhub, &client, upstream, &format!("站点{index}")).await;
        refresh(&client, &akhub, &account).await;
        let names: Vec<String> = (0..20).map(|i| format!("model-{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let response = select(&client, &akhub, &account, &refs, false).await;
        assert_eq!(response.status(), 200);
    }

    let models = logical_models(&akhub).await;
    assert_eq!(models.len(), 20, "20 个逻辑模型，每个归并 8 个目标");
    for (name, targets) in &models {
        assert_eq!(targets.len(), 8, "{name} 应有 8 个账号的目标：{targets:?}");
    }
    let config = akhub.state.config.current();
    let group = config
        .groups
        .iter()
        .find(|g| g.group.id == akhub.group_id)
        .unwrap();
    assert_eq!(group.models.len(), 20);
    assert!(group.models.values().all(|m| m.targets.len() == 8));
}

#[tokio::test]
async fn unselecting_the_last_target_of_an_auto_model_cleans_it_up() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({"data": [{"id": "glm-4.6"}]})));
    let account = create_account(&akhub, &client, &upstream, "站点A").await;
    refresh(&client, &akhub, &account).await;
    select(&client, &akhub, &account, &["glm-4.6"], false).await;
    assert!(logical_models(&akhub).await.contains_key("glm-4.6"));

    // 自动创建的逻辑模型失去最后一个目标后被清理（§16.3）。
    let response = select(&client, &akhub, &account, &[], false).await;
    assert_eq!(response.status(), 200);
    assert!(
        !logical_models(&akhub).await.contains_key("glm-4.6"),
        "零目标的自动模型要清理"
    );
    let catalog: Vec<Value> = client
        .get(format!(
            "{}/admin/api/accounts/{account}/models",
            akhub.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(catalog[0]["selected"], false, "取消勾选要写回目录");
}

async fn find_account(akhub: &Akhub, id: &str) -> akhub::domain::Account {
    akhub
        .state
        .store
        .list_accounts()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.id == id)
        .unwrap()
}

#[tokio::test]
async fn managed_sync_hosts_everything_without_touching_the_selection_set() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({
        "data": [{"id": "m1"}, {"id": "m2"}, {"id": "m3"}]
    })));
    let account_id = create_account(&akhub, &client, &upstream, "站点A").await;

    // 先用对话框勾好选择集：只要 m1。
    refresh(&client, &akhub, &account_id).await;
    select(&client, &akhub, &account_id, &["m1"], false).await;

    // 打开自动同步并立即触发一轮托管（§16.2）。
    let response = client
        .request(
            reqwest::Method::PATCH,
            format!("{}/admin/api/accounts/{account_id}", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"auto_sync": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response = client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{account_id}/models/sync",
                akhub.base_url
            ),
        )
        .header("x-akhub-csrf", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());

    // 全部模型进入调度。
    let models = logical_models(&akhub).await;
    for name in ["m1", "m2", "m3"] {
        assert_eq!(models.get(name).map(Vec::len), Some(1), "{name} 应被托管");
    }
    // 选择集没有被托管污染：仍是只有 m1 被勾选（§16.2）。
    let catalog: Vec<Value> = client
        .get(format!(
            "{}/admin/api/accounts/{account_id}/models",
            akhub.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let flags: HashMap<String, bool> = catalog
        .iter()
        .map(|e| {
            (
                e["upstream_model"].as_str().unwrap().to_string(),
                e["selected"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        flags,
        HashMap::from([
            ("m1".into(), true),
            ("m2".into(), false),
            ("m3".into(), false)
        ])
    );

    // 同步时间已记录。
    let account = find_account(&akhub, &account_id).await;
    assert!(account.model_synced_at.is_some());

    // 上游新增 m4 后，下一轮托管自动跟上。
    upstream.set_models(Some(json!({
        "data": [{"id": "m1"}, {"id": "m2"}, {"id": "m3"}, {"id": "m4"}]
    })));
    client
        .request(
            reqwest::Method::POST,
            format!(
                "{}/admin/api/accounts/{account_id}/models/sync",
                akhub.base_url
            ),
        )
        .header("x-akhub-csrf", "1")
        .send()
        .await
        .unwrap();
    assert!(logical_models(&akhub).await.contains_key("m4"));

    // 关闭托管：调度目标收回选择集，回到只有 m1（§16.2）。
    let response = client
        .request(
            reqwest::Method::PATCH,
            format!("{}/admin/api/accounts/{account_id}", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"auto_sync": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let models = logical_models(&akhub).await;
    assert_eq!(models.get("m1").map(Vec::len), Some(1), "m1 回到选择集");
    assert!(!models.contains_key("m2"), "托管期间的全量目标要收回");
    assert!(!models.contains_key("m3"));
    assert!(!models.contains_key("m4"));

    // 托管中的账号不能用对话框勾选。
    let response = client
        .request(
            reqwest::Method::PATCH,
            format!("{}/admin/api/accounts/{account_id}", akhub.base_url),
        )
        .header("x-akhub-csrf", "1")
        .json(&json!({"auto_sync": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response = select(&client, &akhub, &account_id, &["m2"], false).await;
    assert_eq!(response.status(), 409, "托管中勾选对话框应被拒绝");
}

#[tokio::test]
async fn managed_sync_drops_orphans_and_disables_vanished_models() {
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({
        "data": [{"id": "m1"}, {"id": "m2"}, {"id": "m3"}]
    })));
    let account_id = create_account(&akhub, &client, &upstream, "站点A").await;
    let account = find_account(&akhub, &account_id).await;
    akhub::discovery::sync_managed(&akhub.state, &account)
        .await
        .unwrap();
    assert_eq!(logical_models(&akhub).await.len(), 3);

    // m3 从上游消失：托管把它移出调度；选择集里勾过的 m1 保留 missing 标记。
    upstream.set_models(Some(json!({"data": [{"id": "m1"}, {"id": "m2"}]})));
    akhub::discovery::sync_managed(&akhub.state, &account)
        .await
        .unwrap();
    let models = logical_models(&akhub).await;
    assert_eq!(models.get("m1").map(Vec::len), Some(1));
    assert!(!models.contains_key("m3"), "消失模型的目标必须停止");
    let catalog = akhub
        .state
        .store
        .list_account_models(&account_id)
        .await
        .unwrap();
    assert!(
        catalog.iter().all(|r| r.upstream_model != "m3"),
        "未勾选的消失模型移出目录"
    );

    // m3 重现后自动恢复托管。
    upstream.set_models(Some(
        json!({"data": [{"id": "m1"}, {"id": "m2"}, {"id": "m3"}]}),
    ));
    akhub::discovery::sync_managed(&akhub.state, &account)
        .await
        .unwrap();
    assert_eq!(
        logical_models(&akhub).await.get("m3").map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn an_explicit_capability_rejection_is_learned_and_routed_around() {
    // §16.7 验收：上游明确"不支持"→ 缓存限制 → 调度避开该组合；普通 400
    // 与其它状态码绝不触发学习。
    let (akhub, _client) = spawn_admin().await;
    let a = FakeUpstream::spawn().await;
    let b = FakeUpstream::spawn().await;

    // 两个账号同优先级 50，共同支撑 glm-4.6。层内评分会随机挑一个，
    // 所以学习前后都要看"请求落在谁家"而不是假设固定目标。
    let account_a = wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &a.base_url,
            Protocol::OpenAiChat,
            "glm-4.6",
            "glm-4.6",
            50,
        ),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "B",
            &b.base_url,
            Protocol::OpenAiChat,
            "glm-4.6",
            "glm-4.6",
            50,
        ),
    )
    .await;

    let image_body = json!({
        "model": "glm-4.6",
        "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]}],
    });

    // 第一发：随机落到某家并成功——先把链路跑通。
    let response = common::chat(&akhub, image_body.clone()).await;
    assert_eq!(response.status(), 200);

    // A 从此对带图片的请求明确拒绝。
    a.fallback(common::Behavior::Json(
        400,
        json!({
            "error": {"message": "this model does not support images",
                       "type": "invalid_request_error"}
        }),
    ));
    b.fallback(common::Behavior::Ok);

    // 层内抽签随机选目标：vision 请求落到 A 时得到 400（§13.3 请求级错误
    // 不切换），学习在失败路径上发生；落到 B 时得到 200。反复发直到学会。
    for _ in 0..20 {
        let response = common::chat(&akhub, image_body.clone()).await;
        assert!(
            response.status() == 200 || response.status() == 400,
            "vision 请求要么成功要么收到上游的明确拒绝：{}",
            response.status()
        );
        if akhub.state.runtime.capabilities.is_unsupported(
            &account_a.account_id,
            "glm-4.6",
            "vision",
            std::time::Instant::now(),
        ) {
            break;
        }
    }
    assert!(
        akhub.state.runtime.capabilities.is_unsupported(
            &account_a.account_id,
            "glm-4.6",
            "vision",
            std::time::Instant::now()
        ),
        "A 的明确拒绝必须被学习成 vision 限制"
    );

    // 学习之后：vision 请求一律绕开 A。普通文本请求不受影响。
    let seen_a = a.seen.lock().unwrap().len();
    let seen_b = b.seen.lock().unwrap().len();
    for _ in 0..5 {
        let response = common::chat(&akhub, image_body.clone()).await;
        assert_eq!(response.status(), 200);
    }
    assert_eq!(
        a.seen.lock().unwrap().len(),
        seen_a,
        "vision 请求不得再发往 A"
    );
    assert!(
        b.seen.lock().unwrap().len() > seen_b,
        "vision 请求应全部落到 B"
    );

    let plain = json!({"model": "glm-4.6", "messages": [{"role": "user", "content": "你好"}]});
    a.fallback(common::Behavior::Ok);
    let response = common::chat(&akhub, plain).await;
    assert_eq!(response.status(), 200, "文本请求不受 vision 限制影响");
}

#[tokio::test]
async fn empty_or_invalid_model_lists_never_wipe_the_catalog() {
    // §26.5：0 个模型不覆盖，错误 JSON 不覆盖。
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({"data": [{"id": "glm-4.6"}, {"id": "m2"}]})));
    let account = create_account(&akhub, &client, &upstream, "站点A").await;
    refresh(&client, &akhub, &account).await;
    select(&client, &akhub, &account, &["glm-4.6"], false).await;

    for bad in [
        json!({"data": []}),
        json!({"models": "不是模型列表"}),
        json!("字符串"),
    ] {
        let bad_display = format!("{bad}");
        upstream.set_models(Some(bad));
        let response = client
            .request(
                reqwest::Method::POST,
                format!(
                    "{}/admin/api/accounts/{account}/models/refresh",
                    akhub.base_url
                ),
            )
            .header("x-akhub-csrf", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 502, "无效列表必须报错：{bad_display}");
        let catalog: Vec<Value> = client
            .get(format!(
                "{}/admin/api/accounts/{account}/models",
                akhub.base_url
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(catalog.len(), 2, "原目录必须原样保留：{bad_display}");
        assert_eq!(
            logical_models(&akhub).await.get("glm-4.6").map(Vec::len),
            Some(1),
            "选择集与目标不受影响：{bad_display}"
        );
    }
}

#[tokio::test]
async fn the_selection_set_survives_a_second_fetch_with_new_models_unchecked() {
    // §26.5：选择集持久——二次拉取时已选保持选中，新增默认不勾。
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({"data": [{"id": "a1"}, {"id": "a2"}]})));
    let account = create_account(&akhub, &client, &upstream, "站点A").await;
    refresh(&client, &akhub, &account).await;
    // 勾 a1，明确排除 a2。
    select(&client, &akhub, &account, &["a1"], false).await;

    // 二次拉取：a3 新出现。
    upstream.set_models(Some(
        json!({"data": [{"id": "a1"}, {"id": "a2"}, {"id": "a3"}]}),
    ));
    let entries = refresh(&client, &akhub, &account).await;
    let by_name: HashMap<String, Value> = entries
        .iter()
        .map(|e| (e["upstream_model"].as_str().unwrap().to_string(), e.clone()))
        .collect();
    assert_eq!(by_name["a1"]["selected"], true, "已选的保持选中");
    assert_eq!(by_name["a1"]["is_new"], false);
    assert_eq!(by_name["a2"]["selected"], false, "明确排除的保持不勾");
    assert_eq!(by_name["a3"]["selected"], false, "新出现的默认不勾");
    assert_eq!(by_name["a3"]["is_new"], true);

    // 三次拉取：a3 已经不是"新出现"。
    let entries = refresh(&client, &akhub, &account).await;
    let a3 = entries
        .iter()
        .find(|e| e["upstream_model"] == "a3")
        .unwrap();
    assert_eq!(a3["is_new"], false, "见过一次就不再标记新增");
}

#[tokio::test]
async fn a_manual_logical_model_keeps_its_last_target_removed_but_stays_hidden() {
    // §26.5：手工创建的逻辑模型在最后一个目标被移除后保留记录，
    // 但从 /v1/models 消失（零目标不外显）。
    let (akhub, client) = spawn_admin().await;
    let upstream = FakeUpstream::spawn().await;
    upstream.set_models(Some(json!({"data": [{"id": "glm-4.6"}]})));
    let account = create_account(&akhub, &client, &upstream, "站点A").await;
    refresh(&client, &akhub, &account).await;
    select(&client, &akhub, &account, &["glm-4.6"], false).await;

    // 换成手工语义的逻辑模型（先删自动创建的，再按管理员视角重建）。
    let models = akhub.state.store.list_logical_models().await.unwrap();
    let auto = models.iter().find(|m| m.name == "glm-4.6").unwrap().clone();
    akhub
        .state
        .store
        .delete_logical_model(&auto.id)
        .await
        .unwrap();
    akhub
        .state
        .store
        .insert_logical_model(&akhub::domain::LogicalModel {
            id: auto.id.clone(),
            group_id: auto.group_id.clone(),
            name: auto.name.clone(),
            origin: akhub::domain::ModelOrigin::Manual,
            enabled: true,
            created_at: auto.created_at,
        })
        .await
        .unwrap();
    // 目标外键指向逻辑模型，删除会级联；按选择集语义重建目标。
    akhub.state.reload_config().await.unwrap();
    akhub::discovery::add_manual_model(
        &akhub.state,
        &find_account(&akhub, &account).await,
        "glm-4.6",
        None,
    )
    .await
    .unwrap();

    // 取消勾选：目标移除，逻辑模型保留。
    select(&client, &akhub, &account, &[], false).await;
    let models = akhub.state.store.list_logical_models().await.unwrap();
    assert!(
        models.iter().any(|m| m.name == "glm-4.6"),
        "手工模型零目标时保留记录"
    );
    let listed: Value = client
        .get(format!("{}/v1/models", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed["data"].as_array().map(Vec::len),
        Some(0),
        "零目标的手工模型从 /v1/models 消失：{listed}"
    );
}
