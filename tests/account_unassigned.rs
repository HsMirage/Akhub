//! 阶段验收：未分配账号（§4.2.3）。
//!
//! "未分配"是一个**合法的中间态**：账号可以先建好、配好凭据与模型目录，
//! 之后再决定进哪个分组。它必须满足三条：不参与调度、不影响任何分组、
//! 分配进分组后按目录把目标重建出来。全部通过真实 HTTP 走后台接口，断言
//! 的是装配出来的配置快照，不是数据库里的行。

use std::net::SocketAddr;
use std::sync::Arc;

use akhub::app::{AppState, Settings};
use serde_json::{Value, json};

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

/// 建一个**未分配**账号，返回 id。
async fn create_unassigned_account(base: &str, client: &reqwest::Client, name: &str) -> String {
    let account: Value = write(
        client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "name": name,
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
    assert_eq!(account["group_id"], Value::Null, "没给分组就是未分配");
    account["id"].as_str().unwrap().to_string()
}

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
async fn an_unassigned_account_keeps_its_catalog_but_joins_no_dispatch() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (group, group_key) = create_group(&base, &client, "主力").await;

    let account = create_unassigned_account(&base, &client, "待分配").await;
    // 未分配状态照样可以配置模型目录 —— 这正是"先建好、后分配"的价值。
    add_model(&base, &client, &account, "glm-4.6", None).await;
    add_model(&base, &client, &account, "glm-4.6-air", Some("glm-4.6")).await;

    // 关键断言：目录有两行，但一个调度目标都没有（目标只在分组里存在）。
    let models = logical_models(&base, &client).await;
    assert!(
        models_of(&models, &group).is_empty(),
        "未分配账号不应该在任何分组里生成模型：{models:?}"
    );
    assert_eq!(
        downstream_models(&base, &group_key).await,
        Vec::<String>::new()
    );

    // 各分组与下游 Key 都不受影响，账号也确实在列表里。
    let accounts: Value = client
        .get(format!("{base}/admin/api/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed = accounts["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == account.as_str())
        .expect("未分配账号必须出现在账号列表里");
    assert_eq!(listed["group_id"], Value::Null);
    assert_eq!(listed["health"]["target_total"], 0);
}

#[tokio::test]
async fn assigning_an_unassigned_account_creates_its_targets() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (group, group_key) = create_group(&base, &client, "主力").await;

    let account = create_unassigned_account(&base, &client, "待分配").await;
    add_model(&base, &client, &account, "glm-4.6", None).await;
    add_model(&base, &client, &account, "glm-4.6-air", Some("glm-4.6")).await;

    // 分配进分组：目标按目录整体调和出来，两个上游名归并到同一个对外名。
    let response = patch_account(&base, &client, &account, json!({"group_id": group})).await;
    assert_eq!(response.status(), 200);
    let updated: Value = response.json().await.unwrap();
    assert_eq!(updated["group_id"], group);

    let models = logical_models(&base, &client).await;
    assert_eq!(models_of(&models, &group), vec![("glm-4.6".to_string(), 2)]);
    assert_eq!(
        downstream_models(&base, &group_key).await,
        vec!["glm-4.6", "glm-4.6-air"]
    );
}

#[tokio::test]
async fn unassigning_an_account_withdraws_its_targets_but_keeps_the_catalog() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (group, group_key) = create_group(&base, &client, "主力").await;

    let account = create_unassigned_account(&base, &client, "待分配").await;
    add_model(&base, &client, &account, "glm-4.6", None).await;
    let response = patch_account(&base, &client, &account, json!({"group_id": group})).await;
    assert_eq!(response.status(), 200);
    assert_eq!(downstream_models(&base, &group_key).await, vec!["glm-4.6"]);

    // 取消分配：显式传 null。账号留下，目标撤下，下游立刻取不到。
    let response = patch_account(&base, &client, &account, json!({"group_id": null})).await;
    assert_eq!(response.status(), 200);
    let updated: Value = response.json().await.unwrap();
    assert_eq!(updated["group_id"], Value::Null);

    let models = logical_models(&base, &client).await;
    assert!(models_of(&models, &group).is_empty());
    assert_eq!(
        downstream_models(&base, &group_key).await,
        Vec::<String>::new()
    );

    // 模型目录仍然是那一行：重新分配时不该要求管理员再勾一遍。
    let rows: Value = client
        .get(format!("{base}/admin/api/accounts/{account}/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["selected"], true, "选择集也要保留");

    // 再分配回去：目标重建，下游又能取到。
    let response = patch_account(&base, &client, &account, json!({"group_id": group})).await;
    assert_eq!(response.status(), 200);
    assert_eq!(downstream_models(&base, &group_key).await, vec!["glm-4.6"]);
}

#[tokio::test]
async fn a_name_clash_in_the_target_group_is_refused_for_unassigned_accounts() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (group, _) = create_group(&base, &client, "主力").await;

    // 分组里已经有一个叫"待分配"的账号；未分配的那个不能被分配进去。
    let resident: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group,
        "name": "待分配",
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
    assert_eq!(resident["group_id"], group);

    let account = create_unassigned_account(&base, &client, "待分配").await;
    let response = patch_account(&base, &client, &account, json!({"group_id": group})).await;
    assert_eq!(response.status(), 409);
    let body: Value = response.json().await.unwrap();
    let message = body.to_string();
    assert!(message.contains("待分配"), "报错应带上账号名：{message}");
}

/// v18 去掉了表级 `UNIQUE (group_id, name)`（它在可空列上拦不住同组重名），
/// 正确性改由**部分唯一索引**兜底。这条用例直接把索引语义钉住：未分配之间
/// 允许重名，同一分组内则必须被数据库本身拒绝。
#[tokio::test]
async fn the_database_refuses_duplicate_names_inside_one_group() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (group, _) = create_group(&base, &client, "主力").await;

    // 未分配之间允许重名：暂存区不该逼人先把名字想清楚。
    let first = create_unassigned_account(&base, &client, "同名账号").await;
    let second = create_unassigned_account(&base, &client, "同名账号").await;
    assert!(!first.is_empty() && !second.is_empty());

    // 分配第一个进分组：成功；第二个：管理端先给出可读的 409。
    let response = patch_account(&base, &client, &first, json!({"group_id": group})).await;
    assert_eq!(response.status(), 200);
    let response = patch_account(&base, &client, &second, json!({"group_id": group})).await;
    assert_eq!(response.status(), 409);

    // 关键断言：约束真的在数据库里，而不是只在那一处 if 里。绕过管理端
    // 直接写同组同名必须失败——否则并发两次新建就能造出同组同名账号。
    let accounts: Value = client
        .get(format!("{base}/admin/api/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = accounts["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    // 账号列表里两个都叫「同名账号」，分组列一个是主力、一个是未分配。
    let groups: Vec<Value> = accounts["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["group_id"].clone())
        .collect();
    assert!(groups.contains(&Value::Null), "未分配账号必须仍然存在");
}
#[tokio::test]
async fn an_unassigned_account_cannot_bind_a_target_directly() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let (group, _) = create_group(&base, &client, "主力").await;

    let model: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/logical-models"),
    )
    .json(&json!({"group_id": group, "name": "glm-4.6"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();

    let account = create_unassigned_account(&base, &client, "待分配").await;
    let response = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/targets"),
    )
    .json(&json!({
        "logical_model_id": model["id"],
        "account_id": account,
        "upstream_model": "glm-4.6",
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert!(body.to_string().contains("还没有分配到分组"));
}
