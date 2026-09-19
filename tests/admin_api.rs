//! 管理后台接口的端到端验收：首次设置、登录、CSRF 与资源 CRUD（§7.4、§23.2）。

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

    // cookie_store 让会话 Cookie 在后续请求中自动携带。
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

#[tokio::test]
async fn first_run_requires_setup_and_only_accepts_it_once() {
    let (base, client, _dir) = spawn().await;

    let status: Value = client
        .get(format!("{base}/admin/api/setup/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["needs_setup"], true);

    setup_admin(&base, &client).await;

    let status: Value = client
        .get(format!("{base}/admin/api/setup/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["needs_setup"], false);

    // 第二次设置必须被拒绝，否则任何人都能顶掉管理员。
    let again = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/setup"),
    )
    .json(&json!({"username": "攻击者", "password": "another password"}))
    .send()
    .await
    .unwrap();
    assert_eq!(again.status(), 409);
}

#[tokio::test]
async fn weak_passwords_are_rejected() {
    let (base, client, _dir) = spawn().await;
    let response = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/setup"),
    )
    .json(&json!({"username": "admin", "password": "short"}))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn protected_endpoints_reject_anonymous_and_wrong_passwords() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;

    // 另开一个没有会话的客户端。
    let anonymous = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let response = anonymous
        .get(format!("{base}/admin/api/groups"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    let bad_login = write(
        &anonymous,
        reqwest::Method::POST,
        format!("{base}/admin/api/auth/login"),
    )
    .json(&json!({"username": "admin", "password": "错误的密码"}))
    .send()
    .await
    .unwrap();
    assert_eq!(bad_login.status(), 401);
    let body: Value = bad_login.json().await.unwrap();
    // 不能泄漏"用户存在但密码错"。
    assert_eq!(body["error"], "用户名或密码错误");

    let good_login = write(
        &anonymous,
        reqwest::Method::POST,
        format!("{base}/admin/api/auth/login"),
    )
    .json(&json!({"username": "admin", "password": "correct horse battery"}))
    .send()
    .await
    .unwrap();
    assert_eq!(good_login.status(), 200);
    assert_eq!(
        anonymous
            .get(format!("{base}/admin/api/groups"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn writes_without_the_csrf_header_are_refused() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;

    // 没有 CSRF 头的写请求：模拟跨站表单提交。
    let response = client
        .post(format!("{base}/admin/api/groups"))
        .json(&json!({"name": "偷偷建的组", "multiplier_limit": "1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);

    // 读请求不受影响。
    assert_eq!(
        client
            .get(format!("{base}/admin/api/groups"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn logout_invalidates_the_session_immediately() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;

    write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/auth/logout"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(
        client
            .get(format!("{base}/admin/api/groups"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn the_full_configuration_path_works_end_to_end() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;

    // 1. 建分组，拿到只显示一次的完整 Key。
    let created: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "主力", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let group_id = created["group"]["id"].as_str().unwrap().to_string();
    let key = created["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("akh-"));
    assert_eq!(
        created["group"]["weights"]["multiplier"], 40,
        "默认权重应为 40/25/20/15"
    );

    // 列表接口只返回前缀，绝不返回完整 Key。
    let groups: Value = client
        .get(format!("{base}/admin/api/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!groups.to_string().contains(&key), "列表接口泄漏了完整 Key");

    // 2. 建账号。
    let account: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "账号A",
        "upstream_type": "anthropic",
        "base_url": "https://api.anthropic.com",
        "api_key": "sk-上游真Key",
        "preferred_protocol": "anthropic_messages",
        "manual_multiplier": "0.5",
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let account_id = account["id"].as_str().unwrap().to_string();
    assert_eq!(account["effective_multiplier"], "0.5");

    // 账号接口在任何形态下都不能回吐上游 Key（§23.2）。
    let accounts: Value = client
        .get(format!("{base}/admin/api/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!accounts.to_string().contains("sk-上游真Key"));

    // 3. 建逻辑模型。
    let model: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/logical-models"),
    )
    .json(&json!({"group_id": group_id, "name": "claude-sonnet-4-5"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let model_id = model["id"].as_str().unwrap().to_string();
    assert_eq!(model["listed"], false, "零目标的逻辑模型不进入 /v1/models");

    // 4. 建调度目标。
    let target = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/targets"),
    )
    .json(&json!({
        "logical_model_id": model_id,
        "account_id": account_id,
        "upstream_model": "claude-sonnet-4-5-20250929",
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(target.status(), 201);

    // 5. 配置快照已切换，模型此时可对外提供服务。
    let models: Value = client
        .get(format!("{base}/admin/api/logical-models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // 列表接口现在是分页信封（§7.4）。
    assert_eq!(models["data"][0]["dispatch_targets"], 1);
    assert_eq!(models["data"][0]["listed"], true);

    // 用刚拿到的下游 Key 真的能取到模型列表。
    let listed: Value = reqwest::Client::new()
        .get(format!("{base}/v1/models"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["data"][0]["id"], "claude-sonnet-4-5");
}

#[tokio::test]
async fn cross_group_target_binding_is_rejected() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;

    let mut ids = Vec::new();
    for name in ["组一", "组二"] {
        let created: Value = write(
            &client,
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
        ids.push(created["group"]["id"].as_str().unwrap().to_string());
    }

    let account: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": ids[0],
        "name": "账号A",
        "upstream_type": "openai_compatible",
        "base_url": "https://api.example.com",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();

    let model: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/logical-models"),
    )
    .json(&json!({"group_id": ids[1], "name": "glm-4.6"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();

    // 分组是调度硬边界，跨组绑定必须在写入时就失败（§4.1）。
    let response = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/targets"),
    )
    .json(&json!({
        "logical_model_id": model["id"],
        "account_id": account["id"],
        "upstream_model": "glm-4.6",
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn invalid_configuration_is_rejected_with_useful_messages() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;

    // 权重之和不为 100。
    let bad_weights = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({
        "name": "权重错的组",
        "multiplier_limit": "1",
        "weights": {"multiplier": 50, "reliability": 25, "first_token": 20, "throughput": 15},
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(bad_weights.status(), 400);

    // 倍率不是合法数值。
    let bad_multiplier = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "倍率错的组", "multiplier_limit": "免费"}))
    .send()
    .await
    .unwrap();
    assert_eq!(bad_multiplier.status(), 422);

    let group: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "主力", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let group_id = group["group"]["id"].as_str().unwrap().to_string();

    // SSRF：默认阻止环回地址（§23.3）。
    let ssrf = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "内网账号",
        "upstream_type": "openai_compatible",
        "base_url": "http://169.254.169.254",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(ssrf.status(), 400);

    // 重名分组返回 409 而不是 500。
    let duplicate = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "主力", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap();
    assert_eq!(duplicate.status(), 409);
}

#[tokio::test]
async fn regenerating_a_key_invalidates_the_previous_one() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;

    let created: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "主力", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let group_id = created["group"]["id"].as_str().unwrap();
    let old_key = created["key"].as_str().unwrap().to_string();

    let regenerated: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups/{group_id}/regenerate-key"),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let new_key = regenerated["key"].as_str().unwrap().to_string();
    assert_ne!(old_key, new_key);

    let downstream = reqwest::Client::new();
    let with_old = downstream
        .get(format!("{base}/v1/models"))
        .bearer_auth(&old_key)
        .send()
        .await
        .unwrap();
    assert_eq!(with_old.status(), 401, "旧 Key 必须立即失效");

    let with_new = downstream
        .get(format!("{base}/v1/models"))
        .bearer_auth(&new_key)
        .send()
        .await
        .unwrap();
    assert_eq!(with_new.status(), 200);
}

#[tokio::test]
async fn the_admin_spa_is_served_from_the_binary() {
    let (base, client, _dir) = spawn().await;

    let page = client.get(format!("{base}/admin")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(
        page.headers()["cache-control"],
        "no-cache",
        "外壳不能被缓存"
    );
    let html = page.text().await.unwrap();
    assert!(html.contains("<div id=\"root\">"), "应当返回单页应用外壳");

    // 从外壳里取出真实的资源路径，确认它也是从二进制里出来的。
    let asset = html
        .split("src=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("外壳里应当有入口脚本");
    assert!(
        asset.starts_with("/admin/assets/"),
        "资源路径应当带 /admin 前缀：{asset}"
    );

    let script = client.get(format!("{base}{asset}")).send().await.unwrap();
    assert_eq!(script.status(), 200);
    assert!(
        script.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript")
    );
    assert!(
        script.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable"),
        "带内容哈希的资源应当长期缓存"
    );
}

#[tokio::test]
async fn unknown_admin_paths_fall_back_to_the_spa_not_the_gateway() {
    let (base, client, _dir) = spawn().await;

    // 直接访问深层路径不该 404，也不该落到网关的错误体上。
    let page = client
        .get(format!("{base}/admin/anything/deep"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("<div id=\"root\">"));

    // 但 API 路径必须仍由 API 处理，不能被静态资源通配吞掉。
    let api = client
        .get(format!("{base}/admin/api/setup/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(api.status(), 200);
    assert!(api.text().await.unwrap().contains("needs_setup"));
}

#[tokio::test]
async fn phase_two_account_fields_are_validated_and_never_leak_the_token() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let group: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "主力", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let group_id = group["group"]["id"].as_str().unwrap().to_string();

    // New API 自动倍率没有访问令牌就不能保存：写进去只会让探针每 5 分钟失败一次。
    let missing_token = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "缺令牌",
        "upstream_type": "new_api",
        "base_url": "https://newapi.example.com",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
        "multiplier_mode": "new_api",
        "new_api_user_id": "42",
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(missing_token.status(), 400);

    // 限制写 0 是陷阱：看起来像不限，实际会把目标永久锁死。
    let zero_limit = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "零并发",
        "upstream_type": "openai_compatible",
        "base_url": "https://api.example.com",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
        "limits": {"max_concurrency": 0},
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(zero_limit.status(), 400);

    let created: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "自动倍率",
        "upstream_type": "new_api",
        "base_url": "https://newapi.example.com",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
        "multiplier_mode": "new_api",
        "manual_multiplier": "0.5",
        "new_api_token": "tok-绝密",
        "new_api_user_id": "42",
        "new_api_group": "vip",
        "limits": {"rpm": 600, "tpm": null, "max_concurrency": 4},
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(created["has_new_api_token"], true);
    assert_eq!(created["multiplier_mode"], "new_api");
    assert_eq!(created["limits"]["rpm"], 600);
    assert_eq!(created["limits"]["max_concurrency"], 4);
    // 首次刷新成功前用手填值顶着，并立即进入宽限期（§11.4）。
    assert_eq!(created["effective_multiplier"], "0.5");
    assert_eq!(created["multiplier_status"], "multiplier_stale");
    assert!(
        !created.to_string().contains("tok-绝密"),
        "访问令牌绝不能回吐"
    );

    // 编辑时不重填令牌也能保存：后台不提供读取凭据的接口，"没填"必须是"不变"。
    let id = created["id"].as_str().unwrap();
    let patched = write(
        &client,
        reqwest::Method::PATCH,
        format!("{base}/admin/api/accounts/{id}"),
    )
    .json(&json!({"name": "自动倍率·改名"}))
    .send()
    .await
    .unwrap();
    assert_eq!(patched.status(), 200);
    let patched: Value = patched.json().await.unwrap();
    assert_eq!(patched["has_new_api_token"], true);

    // 手动刷新按钮：自动来源会真的去探测（示例域名探不到就如实报 502），
    // 手动来源没有意义、直接 400。
    let probed = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts/{id}/refresh-multiplier"),
    )
    .send()
    .await
    .unwrap();
    assert_ne!(probed.status(), 400, "自动来源不能被当成手动倍率拒绝");
    assert_eq!(probed.status(), 502, "示例域名探测失败必须如实报错");
    let probed: Value = probed.json().await.unwrap();
    assert!(
        probed["error"]
            .as_str()
            .unwrap_or_default()
            .contains("倍率探测失败"),
        "错误信息要说明是探测失败：{probed}"
    );

    let manual: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "手动",
        "upstream_type": "openai_compatible",
        "base_url": "https://api.example.com",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(manual["multiplier_status"], "known");
    let manual_id = manual["id"].as_str().unwrap();
    let refused = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts/{manual_id}/refresh-multiplier"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(refused.status(), 400);

    // 概览带上告警：过期的自动倍率账号出现在黄色告警里。
    let overview: Value = client
        .get(format!("{base}/admin/api/overview"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(overview["probe_systemic_failure"], false);
    assert!(overview["multiplier_stale"].is_array());
}

#[tokio::test]
async fn targets_expose_runtime_status_scores_and_effective_limits() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let group: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({
        "name": "主力",
        "multiplier_limit": "1",
        "weights": {"multiplier": 70, "reliability": 10, "first_token": 10, "throughput": 10},
        "queue_capacity": 0,
        "allow_degrade": false,
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let group_id = group["group"]["id"].as_str().unwrap().to_string();
    assert_eq!(group["group"]["weights"]["multiplier"], 70);
    assert_eq!(group["group"]["queue_capacity"], 0);
    // 降级开关是分组级设置：关掉之后宁可失败也不丢任何能力（§14.8）。
    assert_eq!(group["group"]["allow_degrade"], false);

    let account: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "账号A",
        "upstream_type": "openai_compatible",
        "base_url": "https://api.example.com",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
        "manual_multiplier": "0.5",
        "limits": {"rpm": 600, "tpm": 100000, "max_concurrency": 8},
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let model: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/logical-models"),
    )
    .json(&json!({"group_id": group_id, "name": "glm-4.6"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();

    let target: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/targets"),
    )
    .json(&json!({
        "logical_model_id": model["id"],
        "account_id": account["id"],
        "upstream_model": "glm-4.6",
        "priority_override": 80,
        "limits": {"max_concurrency": 2},
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(target["priority"], 80, "覆盖值优先于账号默认值");
    assert_eq!(target["status"], "active");
    assert_eq!(target["inflight"], 0);
    // 目标覆盖并发，其余继承账号。
    assert_eq!(target["effective_limits"]["max_concurrency"], 2);
    assert_eq!(target["effective_limits"]["rpm"], 600);
    // 没有样本时性能三维是中性分，倍率维是全组最便宜 → 满分。
    assert_eq!(target["score"]["multiplier"], 1.0);
    assert_eq!(target["score"]["warm"], false);
    assert_eq!(target["score"]["samples"], 0);

    // 限制写 0 在目标上同样被拒绝。
    let id = target["id"].as_str().unwrap();
    let zero = write(
        &client,
        reqwest::Method::PATCH,
        format!("{base}/admin/api/targets/{id}"),
    )
    .json(&json!({"limits": {"rpm": 0}}))
    .send()
    .await
    .unwrap();
    assert_eq!(zero.status(), 400);
}

/// 配置版本乐观锁（§7.4）：带上对不上的版本写 → 409，带上当前版本 → 成功，
/// 每次响应都回带最新版本，不带头部时保持兼容。
#[tokio::test]
async fn writes_carry_config_version_and_conflict_is_reported() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;

    // 读一次拿到当前版本（响应头里就有）。客户端开了 cookie_store，
    // 会话 Cookie 会自动带上，不需要手动传。
    let listed = client
        .get(format!("{base}/admin/api/groups"))
        .send()
        .await
        .unwrap();
    let header = listed
        .headers()
        .get("x-akhub-config-version")
        .expect("响应必须回带配置版本")
        .to_str()
        .unwrap()
        .to_string();
    let version: u64 = header.parse().unwrap();

    let create = |expected: Option<u64>, name: &str| {
        let mut builder = write(
            &client,
            reqwest::Method::POST,
            format!("{base}/admin/api/groups"),
        );
        if let Some(value) = expected {
            builder = builder.header("x-akhub-config-version", value.to_string());
        }
        builder.json(&json!({"name": name, "multiplier_limit": "1"}))
    };

    // 带一个对不上的版本号：必须 409，且错误里点明是版本冲突。
    // 刻意用 version + 1000 而不是 version - 1：版本号可能本来就是 0/1，
    // 减一之后容易撞上当前值，那条用例就白测了。
    let stale = create(Some(version + 1000), "不该写进去")
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409, "对不上的版本必须被拒绝");
    let body: Value = stale.json().await.unwrap();
    let message = body["error"].as_str().unwrap();
    assert!(
        message.contains("config_conflict"),
        "错误里要能认出是版本冲突：{message}"
    );

    // 被拒绝的写入不能真的落库。
    let groups: Value = client
        .get(format!("{base}/admin/api/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        groups["data"].as_array().unwrap().is_empty(),
        "冲突的写入不该产生分组：{groups}"
    );

    // 带当前版本：成功，并且响应头里的版本已经前进。
    let ok = create(Some(version), "主力").send().await.unwrap();
    assert_eq!(ok.status(), 201, "当前版本应当允许写入");
    let after: u64 = ok
        .headers()
        .get("x-akhub-config-version")
        .expect("写响应也要回带版本")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        after > version,
        "写成功后版本号必须前进：{version} → {after}"
    );

    // 不带版本头：老客户端与脚本仍然可用，不强制升级。
    let no_header = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "脚本建的", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap();
    assert_eq!(no_header.status(), 201, "不带头部应当保持兼容");

    // 版本号不是数字时明确报错，而不是当成"没带"。
    let malformed = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .header("x-akhub-config-version", "不是数字")
    .json(&json!({"name": "坏的", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap();
    assert_eq!(malformed.status(), 400, "非法版本号要明确拒绝");
}

/// 版本冲突后必须重新加载才能再写：同一份陈旧版本重试仍然 409（§7.4）。
///
/// 这条钉住的是"提示重新加载"到底有没有约束力——如果冲突响应把新版本号
/// 回带并被客户端采纳，用户再点一次保存就会成功并覆盖别人的修改。
#[tokio::test]
async fn a_stale_version_keeps_failing_until_a_read_refreshes_it() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;

    let current: u64 = client
        .get(format!("{base}/admin/api/groups"))
        .send()
        .await
        .unwrap()
        .headers()
        .get("x-akhub-config-version")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let stale = current + 1000;

    let attempt = |version: u64, name: &str| {
        write(
            &client,
            reqwest::Method::POST,
            format!("{base}/admin/api/groups"),
        )
        .header("x-akhub-config-version", version.to_string())
        .json(&json!({"name": name, "multiplier_limit": "1"}))
    };

    // 连续两次用同一个陈旧版本：都必须是 409。
    for round in 0..2 {
        let response = attempt(stale, "覆盖尝试").send().await.unwrap();
        assert_eq!(response.status(), 409, "第 {round} 次重试也应当被拒绝");
    }

    // 冲突响应里仍然带着当前版本（客户端可以据此提示用户），只是客户端
    // 不该采纳它去写。
    let conflict = attempt(stale, "再看一次").send().await.unwrap();
    let advertised: u64 = conflict
        .headers()
        .get("x-akhub-config-version")
        .expect("冲突响应也要说明当前版本")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(advertised, current);

    // 重新读一次拿到最新版本，再用它写就成功——这就是"重新加载后保存"。
    let refreshed: u64 = client
        .get(format!("{base}/admin/api/groups"))
        .send()
        .await
        .unwrap()
        .headers()
        .get("x-akhub-config-version")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let ok = attempt(refreshed, "重新加载后写入").send().await.unwrap();
    assert_eq!(ok.status(), 201, "用最新版本应当写入成功");
}

/// 账号健康摘要（§6.9）：正常、无目标、停用三种情况都要有明确状态与原因。
#[tokio::test]
async fn accounts_report_a_health_summary() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let group: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "主力", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let group_id = group["group"]["id"].as_str().unwrap().to_string();
    let account: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "账号A",
        "upstream_type": "openai_compatible",
        "base_url": "https://api.example.com",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let account_id = account["id"].as_str().unwrap().to_string();

    // 还没有目标：状态正常但原因要说清"没有目标"。
    assert_eq!(account["health"]["status"], "active", "{account}");
    assert_eq!(account["health"]["target_total"], 0, "{account}");
    assert!(
        account["health"]["reason"]
            .as_str()
            .unwrap()
            .contains("没有任何调度目标"),
        "{account}"
    );

    // 加一个目标后：正常，没有原因，目标计数为 1。
    let model: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/logical-models"),
    )
    .json(&json!({"group_id": group_id, "name": "glm-4.6"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/targets"),
    )
    .json(&json!({
        "logical_model_id": model["id"],
        "account_id": account_id,
        "upstream_model": "glm-4.6",
    }))
    .send()
    .await
    .unwrap();

    let listed: Value = client
        .get(format!("{base}/admin/api/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == account_id.as_str())
        .unwrap();
    assert_eq!(row["health"]["status"], "active", "{row}");
    assert_eq!(row["health"]["target_total"], 1, "{row}");
    assert_eq!(row["health"]["targets"]["active"], 1, "{row}");
    assert!(row["health"]["reason"].is_null(), "{row}");

    // 停用账号：状态与原因都要立刻反映出来。
    let disabled: Value = write(
        &client,
        reqwest::Method::PATCH,
        format!("{base}/admin/api/accounts/{account_id}"),
    )
    .json(&json!({"enabled": false}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(disabled["health"]["status"], "disabled", "{disabled}");
    assert!(
        disabled["health"]["reason"]
            .as_str()
            .unwrap()
            .contains("已停用"),
        "{disabled}"
    );
}

/// 分组告警（§6.3）：没有目标的分组要明说 "/v1/models 会返回空列表"。
#[tokio::test]
async fn groups_report_their_own_alerts() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let group: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "主力", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let group_id = group["group"]["id"].as_str().unwrap().to_string();

    let listed: Value = client
        .get(format!("{base}/admin/api/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == group_id.as_str())
        .unwrap();
    let alerts = row["alerts"].as_array().unwrap();
    assert!(!alerts.is_empty(), "空分组必须有告警：{row}");
    assert!(
        alerts
            .iter()
            .any(|a| a["text"].as_str().unwrap().contains("/v1/models")),
        "要明说模型列表会为空：{row}"
    );
    assert!(
        alerts
            .iter()
            .all(|a| a["level"] == "warn" || a["level"] == "danger"),
        "{row}"
    );

    // 建好账号与目标之后，告警应当清空。
    let account: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "账号A",
        "upstream_type": "openai_compatible",
        "base_url": "https://api.example.com",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let model: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/logical-models"),
    )
    .json(&json!({"group_id": group_id, "name": "glm-4.6"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/targets"),
    )
    .json(&json!({
        "logical_model_id": model["id"],
        "account_id": account["id"],
        "upstream_model": "glm-4.6",
    }))
    .send()
    .await
    .unwrap();

    let listed: Value = client
        .get(format!("{base}/admin/api/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == group_id.as_str())
        .unwrap();
    assert!(
        row["alerts"].as_array().unwrap().is_empty(),
        "配好之后不该还有告警：{row}"
    );
}

/// 停用的目标要说清"为什么不动了"，而不是只给一个灰点（§6.9）。
#[tokio::test]
async fn a_disabled_target_reports_its_pause_reason() {
    let (base, client, _dir) = spawn().await;
    setup_admin(&base, &client).await;
    let group: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/groups"),
    )
    .json(&json!({"name": "主力", "multiplier_limit": "1"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let group_id = group["group"]["id"].as_str().unwrap().to_string();
    let account: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "账号A",
        "upstream_type": "openai_compatible",
        "base_url": "https://api.example.com",
        "api_key": "sk-x",
        "preferred_protocol": "openai_chat",
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let model: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/logical-models"),
    )
    .json(&json!({"group_id": group_id, "name": "glm-4.6"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let target: Value = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/targets"),
    )
    .json(&json!({
        "logical_model_id": model["id"],
        "account_id": account["id"],
        "upstream_model": "glm-4.6",
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let target_id = target["id"].as_str().unwrap().to_string();

    let listed: Value = client
        .get(format!("{base}/admin/api/targets"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == target_id.as_str())
        .unwrap();
    assert_eq!(row["status"], "active", "{row}");
    assert!(row["pause_reason"].is_null(), "{row}");

    // 通过分组开关停用整个账号的调度：目标状态与原因都要立刻反映出来。
    write(
        &client,
        reqwest::Method::PATCH,
        format!("{base}/admin/api/targets/{target_id}"),
    )
    .json(&json!({"enabled": false}))
    .send()
    .await
    .unwrap();

    let listed: Value = client
        .get(format!("{base}/admin/api/targets"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == target_id.as_str())
        .unwrap();
    assert_eq!(
        row["status"], "disabled",
        "停用的目标状态应当是 disabled：{row}"
    );
    assert!(
        row["pause_reason"].as_str().is_some_and(|r| !r.is_empty()),
        "停用必须给出原因：{row}"
    );
}
