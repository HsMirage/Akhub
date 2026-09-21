//! 阶段验收：识别上游的倍率来源（§11.2）。
//!
//! “识别”与“刷新”是两件事：刷新用已配好的来源去取倍率，识别是**先问清楚这个
//! 站到底认哪个接口**，再把结论写回账号。这个文件锁死的是识别动作的边界：
//!
//! * 只有“这个接口确实不存在”（404）才允许换下一个候选；
//! * 403 这类“路径在、凭据不对”必须如实报错，绝不能静默改写来源；
//! * 识别成功后来源落库，接着走的还是普通的自动刷新路径。

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use akhub::app::{AppState, Settings};
use common::{Behavior, FakeUpstream};
use serde_json::{Value, json};

/// 启动一台空数据目录的 Akhub，同时把共享状态带出来：识别动作会写库，
/// 测试要能直接读回那份配置。
async fn spawn_with_akhub() -> (String, reqwest::Client, common::Akhub) {
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
    // 复用 common 的 harness：它自己也会起一台服务，这里只要它的 state 与
    // 数据目录，所以把已经起好的那台服务地址让出去。
    let akhub = common::spawn_akhub_on(state, dir).await;
    (format!("http://{addr}"), client, akhub)
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

async fn create_group(base: &str, client: &reqwest::Client) -> String {
    let created: Value = write(
        client,
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
    created["group"]["id"].as_str().unwrap().to_string()
}

/// 建一个账号（默认手动倍率），返回 id。
async fn create_account(base: &str, client: &reqwest::Client, body: Value) -> Value {
    let response = write(
        client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&body)
    .send()
    .await
    .unwrap();
    let status = response.status();
    let payload: Value = response.json().await.unwrap();
    assert!(status.is_success(), "建号失败（{status}）：{payload}");
    payload
}

/// 把 New API 的访问令牌与用户 ID 写进账号（识别顺序依赖"凭据是否齐全"）。
async fn give_new_api_credentials(akhub: &common::Akhub, account_id: &str) {
    let mut account = akhub
        .state
        .store
        .list_accounts()
        .await
        .unwrap()
        .into_iter()
        .find(|account| account.id == account_id)
        .unwrap();
    account.new_api_user_id = Some("42".into());
    let secrets = akhub::storage::store::AccountSecrets {
        api_key: None,
        new_api_token: Some(akhub.state.cipher.seal(b"tok-detect").unwrap()),
    };
    akhub
        .state
        .store
        .update_account(&account, &secrets)
        .await
        .unwrap();
    akhub.state.reload_config().await.unwrap();
}

fn account_body(group_id: &str, base_url: &str) -> Value {
    json!({
        "group_id": group_id,
        "name": "待识别",
        "base_url": base_url,
        "api_key": "sk-detect",
        "preferred_protocol": "openai_chat",
        "manual_multiplier": "1",
        "allow_private_network": true,
    })
}

async fn detect(
    base: &str,
    client: &reqwest::Client,
    account_id: &str,
) -> (reqwest::StatusCode, Value) {
    let response = write(
        client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts/{account_id}/detect-multiplier-source"),
    )
    .send()
    .await
    .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

/// 命中 Sub2API：只要一把 API Key，识别成功即把来源写回账号。
#[tokio::test]
async fn a_sub2api_site_is_recognized_and_written_back() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_billing(Some(json!({
        "object": "billing",
        "version": 1,
        "scope": "key",
        "effective_multiplier": "0.5",
        "observed_at": 1_700_000_000
    })));

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let account = create_account(&base, &client, account_body(&group_id, &upstream.base_url)).await;
    let id = account["id"].as_str().unwrap().to_string();
    assert_eq!(account["multiplier_mode"], "manual");

    let (status, payload) = detect(&base, &client, &id).await;
    assert_eq!(status, 200, "{payload}");
    assert_eq!(payload["multiplier_mode"], "sub2api");
    assert_eq!(payload["detected"], true);
    assert!(payload.get("probe_error").is_none(), "{payload}");

    // 结论已经落库：重新读账号就能看到，而不是只停在这次响应里。
    let list: Value = client
        .get(format!("{base}/admin/api/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let accounts = list["data"].as_array().unwrap();
    let stored = accounts
        .iter()
        .find(|account| account["id"] == id.as_str())
        .unwrap();
    assert_eq!(stored["multiplier_mode"], "sub2api");
    // 识别成功后立刻探测了一次：倍率是上游给的那一档。
    assert_eq!(stored["effective_multiplier"], "0.5");
}

/// Sub2API 返回 404 时换 New API 候选，命中后写回。
#[tokio::test]
async fn a_new_api_site_is_found_after_sub2api_answers_404() {
    let upstream = FakeUpstream::spawn().await;
    upstream.absent_billing();
    upstream.set_groups(Some(json!({
        "success": true,
        "data": {"vip": {"ratio": 0.2}, "default": {"ratio": 1.0}}
    })));

    let (base, client, akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let account = create_account(&base, &client, account_body(&group_id, &upstream.base_url)).await;
    let id = account["id"].as_str().unwrap().to_string();
    give_new_api_credentials(&akhub, &id).await;

    let (status, payload) = detect(&base, &client, &id).await;
    assert_eq!(status, 200, "{payload}");
    assert_eq!(payload["multiplier_mode"], "new_api");
    // 顺序与次数都要对得上：先 Sub2API（被 404 拒了），再 New API；识别成功后
    // 紧接着的那次刷新会再打一次分组接口，所以 New API 至少出现两次。
    let seen = upstream.seen.lock().unwrap();
    let paths: Vec<&str> = seen.iter().map(|seen| seen.path.as_str()).collect();
    assert_eq!(
        paths.first().copied(),
        Some("/v1/sub2api/billing"),
        "Sub2API 必须排在 New API 前面：它只要一把 Key"
    );
    assert_eq!(
        paths
            .iter()
            .filter(|path| **path == "/v1/sub2api/billing")
            .count(),
        1,
        "404 之后不该反复重试同一个候选：{paths:?}"
    );
    assert!(
        paths
            .iter()
            .filter(|path| **path == "/api/user/self/groups")
            .count()
            >= 2,
        "识别与随后的首次刷新各要探一次 New API：{paths:?}"
    );
}

/// 403 不是“这个站没有这个接口”，而是“路径在、凭据不对”：必须报错且不改来源。
#[tokio::test]
async fn an_authenticated_failure_never_rewrites_the_source() {
    let upstream = FakeUpstream::spawn().await;
    upstream.lock_billing();
    upstream.set_groups(Some(json!({
        "success": true,
        "data": {"vip": {"ratio": 0.2}}
    })));

    let (base, client, akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let account = create_account(&base, &client, account_body(&group_id, &upstream.base_url)).await;
    let id = account["id"].as_str().unwrap().to_string();
    give_new_api_credentials(&akhub, &id).await;

    let (status, payload) = detect(&base, &client, &id).await;
    assert_eq!(status, 502, "{payload}");
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains("403"),
        "报错必须点明 HTTP 403：{payload}"
    );
    // 关键断言：New API 那边明明是通的，但 Sub2API 的 403 已经说明“说不准”，
    // 不能顺手把它识别成 New API。
    assert_eq!(
        upstream
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.path == "/api/user/self/groups")
            .count(),
        0,
        "鉴权失败之后不应再打下一个候选"
    );

    let list: Value = client
        .get(format!("{base}/admin/api/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let stored = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(stored["multiplier_mode"], "manual", "来源不能被改写");
}

/// 两个候选都不认：如实报错，并说明另一个候选为什么没被探测。
#[tokio::test]
async fn a_site_that_knows_neither_interface_reports_both() {
    let upstream = FakeUpstream::spawn().await;
    upstream.absent_billing();
    // groups 保持 None：没配凭据的候选根本不该被打到。

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let account = create_account(&base, &client, account_body(&group_id, &upstream.base_url)).await;
    let id = account["id"].as_str().unwrap().to_string();

    let (status, payload) = detect(&base, &client, &id).await;
    assert_eq!(status, 502, "{payload}");
    let error = payload["error"].as_str().unwrap_or_default();
    assert!(error.contains("/v1/sub2api/billing"), "{error}");
    // 另一个候选没被探测是因为没配凭据——错误里要说清这一点，否则管理员会
    // 以为"这个站连 New API 也没有"，而事实是我们根本没问。
    assert!(error.contains("识别未能得出结论"), "{error}");
    assert!(error.contains("/api/user/self/groups"), "{error}");
    assert_eq!(
        upstream
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.path == "/api/user/self/groups")
            .count(),
        0,
        "没有 New API 凭据就不该发请求"
    );
}

/// 没有任何凭据的账号：识别要在**发请求之前**拒绝，而不是拿空 Key 去打一次。
#[tokio::test]
async fn an_account_without_keys_cannot_be_detected() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_billing(Some(json!({
        "object": "billing",
        "version": 1,
        "scope": "key",
        "effective_multiplier": "0.5",
        "observed_at": 1_700_000_000
    })));
    upstream.fallback(Behavior::Ok);

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let mut body = account_body(&group_id, &upstream.base_url);
    body["api_key"] = json!(null);
    body["keys"] = json!([]);
    let account = create_account(&base, &client, body).await;
    let id = account["id"].as_str().unwrap().to_string();

    let (status, payload) = detect(&base, &client, &id).await;
    assert_eq!(status, 400, "{payload}");
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains("API Key"),
        "{payload}"
    );
    assert_eq!(upstream.requests(), 0, "没有凭据就不该发探测请求");
}
