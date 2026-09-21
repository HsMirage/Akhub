//! 阶段验收：上游账号迁移分组（§4.2.2）。
//!
//! 分组是调度的硬边界，跨组目标在配置装配时会被直接丢弃（§4.1）。所以
//! "改分组"不能只改账号上那一列外键：账号到了新分组，旧的逻辑模型还在旧
//! 分组，账号会一个模型都不剩。这些用例全部通过真实 HTTP 走后台接口，断言
//! 的是**装配出来的配置快照**（逻辑模型列表里的 `dispatch_targets` 列），
//! 不是数据库里的行——只有快照才对下游可见。

use std::net::SocketAddr;
use std::sync::Arc;

use akhub::app::{AppState, Settings};
use serde_json::{Value, json};

/// 启动一台空数据目录的 Akhub。
async fn spawn() -> (String, reqwest::Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap(dir.path(), Settings::default())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let router = akhub::server::router(Arc::clone(&state));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    (format!("http://{addr}"), client, dir)
}

/// 带上 CSRF 头的写请求。
fn write(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
) -> reqwest::RequestBuilder {
    client.request(method, url).header("x-akhub-csrf", "1")
}

async fn setup_admin(base: &str, client: &reqwest::Client) {
    let response = write(
        client,
        reqwest::Method::POST,
        format!("{base}/admin/api/setup"),
    )
    .json(&json!({"username": "admin", "password": "correct horse battery"}))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
}

/// 建一个分组，返回 (id, 下游 Key)。
async fn create_group(base: &str, client: &reqwest::Client, name: &str) -> (String, String) {
    let created: Value = write(
        client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": name, "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    (
        created["group"]["id"].as_str().unwrap().to_string(),
        created["key"].as_str().unwrap().to_string(),
    )
}

/// 建一个账号，返回 id。
async fn create_account(
    base: &str,
    client: &reqwest::Client,
    group_id: &str,
    name: &str,
) -> String {
    let account: Value = write(
        client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": name,
        "upstream_type": "openai_compatible",
        "base_url": "https://upstream.example.com",
        "api_key": "sk-test",
        "preferred_protocol": "openai_chat",
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    account["id"].as_str().unwrap().to_string()
}

/// 手动加一行上游模型（等价于在"模型管理"里勾选），立即生成调度目标。
async fn add_model(
    base: &str,
    client: &reqwest::Client,
    account_id: &str,
    upstream_model: &str,
    public_name: Option<&str>,
) {
    let response = write(
        client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts/{account_id}/models"),
    )
    .json(&json!({"upstream_model": upstream_model, "public_name": public_name}))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 204);
}

/// 改账号（后台的 PATCH 接口）。
async fn patch_account(
    base: &str,
    client: &reqwest::Client,
    account_id: &str,
    body: Value,
) -> reqwest::Response {
    write(
        client,
        reqwest::Method::PATCH,
        format!("{base}/admin/api/accounts/{account_id}"),
    )
    .json(&body)
    .send()
    .await
    .unwrap()
}

/// 逻辑模型的只读视图；`dispatch_targets` 来自配置快照。
async fn logical_models(base: &str, client: &reqwest::Client) -> Vec<Value> {
    let page: Value = client
        .get(format!("{base}/admin/api/logical-models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    page["data"].as_array().unwrap().clone()
}

/// 某个分组的模型名落在哪里：分组 id → [(模型名, 目标数)]。
fn models_of(models: &[Value], group_id: &str) -> Vec<(String, u64)> {
    models
        .iter()
        .filter(|model| model["group_id"] == group_id)
        .map(|model| {
            (
                model["name"].as_str().unwrap().to_string(),
                model["dispatch_targets"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// 用下游 Key 拉一次 `/v1/models`，返回排好序的模型名。
///
/// 排序是为了断言稳定：快照里的模型是哈希表，迭代顺序不保证。设置了下游模型名
/// 的行会同时暴露上游原名（§16.4），所以这里的期望值通常是两个名字。
async fn downstream_models(base: &str, key: &str) -> Vec<String> {
    let listed: Value = reqwest::Client::new()
        .get(format!("{base}/v1/models"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut names: Vec<String> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn moving_an_account_carries_its_models_into_the_new_group() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (source, source_key) = create_group(&base, &client, "主力").await;
    let (target, target_key) = create_group(&base, &client, "备用").await;

    let account = create_account(&base, &client, &source, "账号A").await;
    add_model(&base, &client, &account, "glm-4.6", None).await;
    add_model(&base, &client, &account, "glm-4.6-air", Some("glm-4.6")).await;

    // 两台上游模型归并到同一个对外名，落在旧分组里。
    let models = logical_models(&base, &client).await;
    assert_eq!(
        models_of(&models, &source),
        vec![("glm-4.6".to_string(), 2)]
    );
    assert!(models_of(&models, &target).is_empty());
    assert_eq!(
        downstream_models(&base, &target_key).await,
        Vec::<String>::new()
    );

    // 改分组。
    let response = patch_account(&base, &client, &account, json!({"group_id": target})).await;
    assert_eq!(response.status(), 200);
    let updated: Value = response.json().await.unwrap();
    assert_eq!(updated["group_id"], target);
    assert_eq!(updated["name"], "账号A");

    // 两个目标一起搬进新分组，旧的自动模型随最后一个目标消失。
    let models = logical_models(&base, &client).await;
    assert_eq!(
        models_of(&models, &target),
        vec![("glm-4.6".to_string(), 2)]
    );
    assert!(
        models_of(&models, &source).is_empty(),
        "旧分组里空出来的自动模型应被清理"
    );

    // 下游口径：新分组两个名字都能取到（下游模型名 + 上游原名，§16.4），
    // 旧分组一个都不剩。
    assert_eq!(
        downstream_models(&base, &target_key).await,
        vec!["glm-4.6", "glm-4.6-air"]
    );
    assert_eq!(
        downstream_models(&base, &source_key).await,
        Vec::<String>::new()
    );

    // 账号自己的模型目录跟着走，模型管理对话框里仍然是那两行。
    let rows: Value = client
        .get(format!("{base}/admin/api/accounts/{account}/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2, "模型目录跟着账号走，仍然是那两行");
    assert!(rows.iter().all(|row| row["selected"] == true));
}

#[tokio::test]
async fn moving_onto_an_existing_model_name_merges_targets() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (source, _) = create_group(&base, &client, "主力").await;
    let (target, target_key) = create_group(&base, &client, "备用").await;

    // 新分组里已经有一台提供同名模型的账号。
    let resident = create_account(&base, &client, &target, "账号B").await;
    add_model(&base, &client, &resident, "glm-4.6", None).await;

    let moving = create_account(&base, &client, &source, "账号A").await;
    add_model(&base, &client, &moving, "glm-4.6-bf16", Some("glm-4.6")).await;

    let response = patch_account(&base, &client, &moving, json!({"group_id": target})).await;
    assert_eq!(response.status(), 200);

    // 同一个对外名合到一个逻辑模型下成为两个候选目标，不是两条同名模型。
    let models = logical_models(&base, &client).await;
    assert_eq!(
        models_of(&models, &target),
        vec![("glm-4.6".to_string(), 2)]
    );
    assert!(models_of(&models, &source).is_empty());
    assert_eq!(
        downstream_models(&base, &target_key).await,
        vec!["glm-4.6", "glm-4.6-bf16"]
    );
}

#[tokio::test]
async fn a_name_clash_in_the_target_group_is_refused_with_a_useful_message() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (source, _) = create_group(&base, &client, "主力").await;
    let (target, _) = create_group(&base, &client, "备用").await;

    let moving = create_account(&base, &client, &source, "账号A").await;
    create_account(&base, &client, &target, "账号A").await;

    // 同名撞车：409，并说清是搬家撞的，而不是数据库的"名称已存在"。
    let response = patch_account(&base, &client, &moving, json!({"group_id": target})).await;
    assert_eq!(response.status(), 409);
    let body: Value = response.json().await.unwrap();
    let message = body.to_string();
    assert!(message.contains("账号A"), "报错应带上账号名：{message}");

    // 目标分组不存在：404，账号原地不动。
    let response = patch_account(
        &base,
        &client,
        &moving,
        json!({"group_id": "g-does-not-exist"}),
    )
    .await;
    assert_eq!(response.status(), 404);

    // 一个请求里改名 + 搬家：按改完的名字判冲突，因此应当成功。
    let response = patch_account(
        &base,
        &client,
        &moving,
        json!({"group_id": target, "name": "账号A-新"}),
    )
    .await;
    assert_eq!(response.status(), 200);
    let updated: Value = response.json().await.unwrap();
    assert_eq!(updated["group_id"], target);
    assert_eq!(updated["name"], "账号A-新");
}

#[tokio::test]
async fn legacy_targets_without_a_catalog_row_move_by_public_name_too() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (source, _) = create_group(&base, &client, "主力").await;
    let (target, target_key) = create_group(&base, &client, "备用").await;

    let account = create_account(&base, &client, &source, "账号A").await;
    // 老路径：手工建逻辑模型 + 手工建目标，账号目录里没有任何行。
    let model: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/logical-models"),
    )
    .json(&json!({"group_id": source, "name": "claude-sonnet-4-5"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let model_id = model["id"].as_str().unwrap().to_string();
    let created = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/targets"),
    )
    .json(&json!({
        "logical_model_id": model_id,
        "account_id": account,
        "upstream_model": "claude-sonnet-4-5-20250929",
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(created.status(), 201);

    let response = patch_account(&base, &client, &account, json!({"group_id": target})).await;
    assert_eq!(response.status(), 200);

    let models = logical_models(&base, &client).await;
    assert_eq!(
        models_of(&models, &target),
        vec![("claude-sonnet-4-5".to_string(), 1)]
    );
    // 手工模型不随目标消失：留在旧分组的管理视图里，但没有目标、不对外列出。
    assert_eq!(
        models_of(&models, &source),
        vec![("claude-sonnet-4-5".to_string(), 0)]
    );
    assert_eq!(
        downstream_models(&base, &target_key).await,
        vec!["claude-sonnet-4-5", "claude-sonnet-4-5-20250929"]
    );
}
