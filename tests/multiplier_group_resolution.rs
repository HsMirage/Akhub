//! 阶段验收：New API 这把 Key 的**真实档位**（§11.2）。
//!
//! 现场缺陷（用户报告）：ccmax 分组的账号填了访问令牌与用户 ID，刷新出的倍率是
//! 1X，而实际是 0.85x。根因不是"取不到倍率"，而是**取错了分组**：
//!
//! * New API 的 relay 计费按 **令牌分组** 走：`sk-` 令牌自己绑了分组时，它覆盖
//!   用户的默认分组（`middleware/auth.go` 的 TokenAuth）。
//! * 而 `/api/user/self/groups` 返回的倍率矩阵是按 **用户分组** 算的。
//!
//! 旧实现拿到整张矩阵后直接取 `max()`，于是"令牌绑在 ccmax（0.85）"的账号会被
//! 算成该用户可用分组里的最高档（1.0）。这个文件锁死修正后的解析链与它的降级行为：
//!
//! 1. 账号上显式填的分组永远优先，取不到就报错，绝不静默换一档；
//! 2. 没填就查令牌自己的分组；
//! 3. 令牌没绑分组（正常状态）才用用户默认分组；
//! 4. 两步都问不出来，才退回"最高档"保守估算——宁可估高，不能估低。

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use akhub::app::{AppState, Settings};
use common::FakeUpstream;
use serde_json::{Value, json};

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
    let akhub = common::spawn_akhub_on(state, dir).await;
    (format!("http://{addr}"), client, akhub)
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

/// 现场形状：ccmax=0.85 与 default=1.0 两个分组，最高档是 1.0。
///
/// 这正是缺陷的温床——"取最高档"会拿到 1.0，而令牌实际绑在 ccmax。
fn ccmax_groups() -> Value {
    json!({
        "success": true,
        "message": "",
        "data": {
            "ccmax": {"ratio": 0.85, "desc": "无限制可蒸馏"},
            "default": {"ratio": 1.0, "desc": ""}
        }
    })
}

/// 建一个 New API 自动倍率的账号，凭据齐备。
async fn create_new_api_account(
    base: &str,
    client: &reqwest::Client,
    upstream: &FakeUpstream,
    group_id: &str,
) -> String {
    let created: Value = write(
        client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts"),
    )
    .json(&json!({
        "group_id": group_id,
        "name": "冰峰-ccmax",
        "base_url": upstream.base_url,
        "api_key": "sk-ccmax-key",
        "preferred_protocol": "openai_chat",
        "multiplier_mode": "new_api",
        "manual_multiplier": "1",
        "new_api_token": "tok-绝密",
        "new_api_user_id": "42",
        "allow_private_network": true,
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    created["id"].as_str().unwrap().to_string()
}

async fn refresh(base: &str, client: &reqwest::Client, id: &str) -> (reqwest::StatusCode, Value) {
    let response = write(
        client,
        reqwest::Method::POST,
        format!("{base}/admin/api/accounts/{id}/refresh-multiplier"),
    )
    .send()
    .await
    .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

/// **核心回归**：令牌绑在 ccmax（0.85）时，倍率必须是 0.85，而不是可用分组里的
/// 最高档 1.0。
///
/// 这条断言就是用户报的那个 bug：旧实现取 `max()`，必然回 1。
#[tokio::test]
async fn a_token_bound_to_a_group_uses_that_groups_ratio_not_the_highest() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(ccmax_groups()));
    // 这把 Key 在 New API 上自己绑定了 ccmax。
    upstream.set_token_search(Some(json!({
        "success": true,
        "message": "",
        "data": {"items": [{"id": 7, "name": "ccmax", "key": "sk-c****************key", "group": "ccmax"}]}
    })));

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let id = create_new_api_account(&base, &client, &upstream, &group_id).await;

    let (status, payload) = refresh(&base, &client, &id).await;
    assert_eq!(status, 200, "{payload}");
    assert_eq!(
        payload["effective_multiplier"], "0.85",
        "令牌绑在 ccmax 就必须用 ccmax 的 0.85，而不是可用分组最高档：{payload}"
    );
}

/// 令牌没绑分组是**正常状态**（New API 里 `group` 默认空串），此时 relay 用用户
/// 默认分组，倍率必须跟着它走。
#[tokio::test]
async fn an_unbound_token_falls_back_to_the_users_default_group() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(ccmax_groups()));
    upstream.set_token_search(Some(json!({
        "success": true,
        "message": "",
        "data": {"items": [{"id": 7, "name": "未绑分组", "key": "sk-c****************key", "group": ""}]}
    })));
    upstream.set_user_self(Some(json!({
        "success": true,
        "message": "",
        "data": {"id": 42, "username": "u", "group": "ccmax"}
    })));

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let id = create_new_api_account(&base, &client, &upstream, &group_id).await;

    let (status, payload) = refresh(&base, &client, &id).await;
    assert_eq!(status, 200, "{payload}");
    assert_eq!(payload["effective_multiplier"], "0.85", "{payload}");
}

/// 显式填了分组就只认它：即使令牌绑的是别的分组，也不许"帮忙"改档。
#[tokio::test]
async fn an_explicit_group_wins_over_what_the_token_says() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(ccmax_groups()));
    // 令牌说自己绑的是 default(1.0)，但管理员在账号上填了 ccmax。
    upstream.set_token_search(Some(json!({
        "success": true,
        "data": {"items": [{"id": 7, "group": "default"}]}
    })));

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let id = create_new_api_account(&base, &client, &upstream, &group_id).await;
    write(
        &client,
        reqwest::Method::PATCH,
        format!("{base}/admin/api/accounts/{id}"),
    )
    .json(&json!({"new_api_group": "ccmax"}))
    .send()
    .await
    .unwrap();

    let (status, payload) = refresh(&base, &client, &id).await;
    assert_eq!(status, 200, "{payload}");
    assert_eq!(payload["effective_multiplier"], "0.85", "{payload}");
    // 显式分组下根本不该去问令牌归属：问了也没有意义。
    let seen = upstream.seen.lock().unwrap();
    assert!(
        !seen.iter().any(|seen| seen.path == "/api/token/search"),
        "显式填了分组就不该再查令牌归属"
    );
}

/// 解析不出来时必须**保守估高**（最高档），绝不能猜一个偏低的数——猜低是亏钱方向。
#[tokio::test]
async fn an_unresolvable_group_stays_conservative() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(ccmax_groups()));
    // 令牌接口回失败、用户接口 404（老版本 New API 的常见形态）。
    upstream.set_token_search(Some(json!({"success": false, "message": "令牌搜索被限流"})));
    // user_self 未设置 -> 404。

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let id = create_new_api_account(&base, &client, &upstream, &group_id).await;

    let (status, payload) = refresh(&base, &client, &id).await;
    assert_eq!(status, 200, "问不出分组不该让整次刷新失败：{payload}");
    assert_eq!(
        payload["effective_multiplier"], "1",
        "解析不出档位就按最高档保守估算：{payload}"
    );
}

/// 分组归属要能在账号列表里看到，管理员才说得清"这个数凭什么可信"。
#[tokio::test]
async fn the_account_reports_which_group_the_ratio_came_from() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(ccmax_groups()));
    upstream.set_token_search(Some(json!({
        "success": true,
        "data": {"items": [{"id": 7, "group": "ccmax"}]}
    })));

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let id = create_new_api_account(&base, &client, &upstream, &group_id).await;
    refresh(&base, &client, &id).await;

    let list: Value = client
        .get(format!("{base}/admin/api/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let account = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["id"] == id.as_str())
        .expect("账号应当在列表里");
    assert_eq!(account["multiplier_group"], "ccmax", "{account}");
    assert_eq!(account["multiplier_group_source"], "token", "{account}");
    assert_eq!(
        account["effective_multiplier"], "0.85",
        "列表上的数必须与刷新结果一致：{account}"
    );
}

/// 保存前就能预览：新建账号时不必"先存再点开"才能看到分组与倍率。
///
/// 这是用户报告的体验问题——第一次填 New API 时表单拿不到分组，保存后才对。
#[tokio::test]
async fn groups_can_be_previewed_before_the_account_exists() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(ccmax_groups()));
    upstream.set_token_search(Some(json!({
        "success": true,
        "data": {"items": [{"id": 7, "group": "ccmax"}]}
    })));

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;

    let response = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/new-api/preview-groups"),
    )
    .json(&json!({
        "base_url": upstream.base_url,
        "access_token": "tok-绝密",
        "user_id": "42",
        "api_key": "sk-ccmax-key",
        "allow_private_network": true,
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    let payload: Value = response.json().await.unwrap();

    let groups = payload["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{payload}");
    // 按倍率升序：最便宜的排前面。
    assert_eq!(groups[0]["name"], "ccmax");
    assert_eq!(groups[0]["ratio"], "0.85");
    // 关键：预览阶段就把"这把 Key 落在哪一档"解析出来了。
    assert_eq!(payload["resolved_group"], "ccmax", "{payload}");
    assert_eq!(payload["resolved_ratio"], "0.85", "{payload}");
    assert_eq!(payload["resolved_source"], "token", "{payload}");
    // 凭据只用于这一次请求，绝不回吐。
    assert!(
        !payload.to_string().contains("tok-绝密"),
        "预览响应绝不能回吐访问令牌：{payload}"
    );
}

/// 预览接口对缺凭据要给可读的 400，而不是拿空令牌去打一次注定 401 的请求。
#[tokio::test]
async fn previewing_without_credentials_is_a_readable_error() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(ccmax_groups()));

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;

    let response = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/new-api/preview-groups"),
    )
    .json(&json!({
        "base_url": upstream.base_url,
        "user_id": "42",
        "allow_private_network": true,
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 400);
    let payload: Value = response.json().await.unwrap();
    let message = payload["error"].as_str().unwrap_or_default();
    assert!(message.contains("访问令牌"), "{payload}");
    assert_eq!(upstream.requests(), 0, "缺凭据必须在发请求之前就拦下");
}

/// **现场复现**：用 2026-10-12 从 newaicxy.xyz（用户报告的那台上游）实测抓到的
/// 真实分组表跑一遍，锁死"冰峰-ccmax"这个账号必须得到 0.85。
///
/// 这份数据的特点是：所有分组倍率都 < 1，唯一能产出 1 的途径就是**取错了档**
/// 或上游对未知分组回落 1。所以这条断言同时守住了两个方向。
#[tokio::test]
async fn the_live_ccmax_group_table_yields_0_85() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(json!({
        "success": true,
        "message": "",
        "data": {
            "ccmax": {"ratio": 0.85, "desc": "无限制可蒸馏"},
            "heavy-grok": {"ratio": 0.2, "desc": "无限制"},
            "kiro个人号按次": {"ratio": 0.28, "desc": "默认90缓"},
            "kiro按次": {"ratio": 0.03, "desc": "华强北按次"},
            "kiro正价": {"ratio": 0.3, "desc": ""},
            "kiro稳定高缓": {"ratio": 0.13, "desc": "华强北"},
            "kiro高缓": {"ratio": 0.06, "desc": "华强北"},
            "pro 特价组": {"ratio": 0.13, "desc": "gpt-pro号池，不降智特价"},
            "pro满血智商": {"ratio": 0.2, "desc": "gpt-pro，100%不降智组"}
        }
    })));
    // 这把 Key 绑在 ccmax。
    upstream.set_token_search(Some(json!({
        "success": true,
        "message": "",
        "data": {"items": [{"id": 7, "name": "冰峰-ccmax", "key": "sk-c********ccmax", "group": "ccmax"}]}
    })));

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    let group_id = create_group(&base, &client).await;
    let id = create_new_api_account(&base, &client, &upstream, &group_id).await;

    let (status, payload) = refresh(&base, &client, &id).await;
    assert_eq!(status, 200, "{payload}");
    assert_eq!(
        payload["effective_multiplier"], "0.85",
        "冰峰-ccmax 必须是 0.85x：{payload}"
    );
}

/// 站点级凭据能顶替账号级令牌（§6.4）：预览接口也要认这条路。
#[tokio::test]
async fn previewing_accepts_site_level_credentials() {
    let upstream = FakeUpstream::spawn().await;
    upstream.set_groups(Some(ccmax_groups()));

    let (base, client, _akhub) = spawn_with_akhub().await;
    setup_admin(&base, &client).await;
    // 先按站点配一次凭据。
    write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/new-api-sites"),
    )
    .json(&json!({
        "base_url": upstream.base_url,
        "user_id": "42",
        "access_token": "tok-site"
    }))
    .send()
    .await
    .unwrap();

    // 预览时不再传令牌，应当自动用站点凭据。
    let response = write(
        &client,
        reqwest::Method::POST,
        format!("{base}/admin/api/new-api/preview-groups"),
    )
    .json(&json!({
        "base_url": upstream.base_url,
        "allow_private_network": true,
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["groups"].as_array().unwrap().len(), 2, "{payload}");
}
