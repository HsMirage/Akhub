//! 版本检查接口的端到端验收（§更新）。
//!
//! 盯住四件事：拿到的确实是 Release 里的最新版本、缓存真的在省请求、
//! 查不到时明确报错而不是假装「已是最新」、非二进制部署拒绝自更新并给出替代命令。

mod common;

use std::sync::Arc;

use akhub::app::{AppState, Settings};
use akhub::update::Deploy;
use common::FakeGithub;
use serde_json::Value;

/// 起一台空数据目录的 Akhub，返回（地址、带 Cookie 的客户端、数据目录、状态）。
async fn spawn() -> (String, reqwest::Client, tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap(dir.path(), Settings::default())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = akhub::server::router(Arc::clone(&state));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    (format!("http://{addr}"), client, dir, state)
}

/// 带 CSRF 头的写请求。
fn write(client: &reqwest::Client, url: String) -> reqwest::RequestBuilder {
    client.post(url).header("x-akhub-csrf", "1")
}

async fn setup_admin(base: &str, client: &reqwest::Client) -> String {
    let response = write(client, format!("{base}/admin/api/setup"))
        .json(&serde_json::json!({"username": "admin", "password": "correct horse battery"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    base.to_string()
}

async fn status_of(base: &str, client: &reqwest::Client, query: &str) -> Value {
    client
        .get(format!("{base}/admin/api/system/update{query}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_newer_release_is_reported_and_the_answer_is_cached() {
    let github = FakeGithub::spawn("v9.9.9", b"not-a-real-binary".to_vec()).await;
    let (base, client, _dir, state) = spawn().await;
    state.runtime.update.set_api_base(github.base_url.clone());
    setup_admin(&base, &client).await;

    let status = status_of(&base, &client, "").await;
    assert_eq!(status["current"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status["latest"], "9.9.9", "必须读出版本号：{status}");
    assert_eq!(status["has_update"], true);
    assert_eq!(status["cached"], false);
    assert_eq!(status["error"], Value::Null);
    assert_eq!(status["enabled"], true);
    assert_eq!(
        status["release_url"],
        "https://example.invalid/releases/v9.9.9"
    );

    // 部署形态取决于测试进程所在路径：默认的 target/ 下判成源码构建，
    // CARGO_TARGET_DIR 指到别处时可能判成原生二进制。两种形态都必须给出可复制的
    // 升级命令；至于"点了会不会真的替换测试二进制"，由下面那条带守卫的用例负责
    // 拦住——它一旦发现进程被判成二进制部署就直接跳过。
    let deploy = status["deploy"].as_str().unwrap_or_default().to_string();
    let command = status["update_command"].as_str().unwrap_or_default();
    match deploy.as_str() {
        "source" => {
            assert_eq!(
                status["can_self_update"], false,
                "源码构建不该出现自更新入口：{status}"
            );
            assert!(command.contains("git pull"), "源码构建要给出升级命令：{status}");
        }
        _ => assert!(
            command.contains("sudo akhub --update"),
            "二进制部署要给出升级命令：{status}"
        ),
    }

    // 第二次必须是缓存命中：GitHub 匿名限流只有 60 次/小时，不能任人连点。
    let again = status_of(&base, &client, "").await;
    assert_eq!(again["cached"], true);
    assert_eq!(again["latest"], "9.9.9");
    assert_eq!(github.hits(), 1, "缓存命中时不该再请求 /releases/latest");

    // 手动重新检查会绕过缓存，但受 15 秒最小间隔约束——立刻再点仍然是缓存。
    let forced = status_of(&base, &client, "?refresh=1").await;
    assert_eq!(forced["latest"], "9.9.9");
}

#[tokio::test]
async fn an_unreachable_release_api_is_reported_as_an_error() {
    let (base, client, _dir, state) = spawn().await;
    // 127.0.0.1:1 上不会有东西应答。
    state
        .runtime
        .update
        .set_api_base("http://127.0.0.1:1".to_string());
    setup_admin(&base, &client).await;

    let status = status_of(&base, &client, "").await;
    assert_eq!(status["has_update"], false);
    assert_eq!(status["latest"], Value::Null);
    assert!(
        status["error"].as_str().is_some_and(|e| !e.is_empty()),
        "查不到就要说查不到，不能假装已是最新：{status}"
    );
}

#[tokio::test]
async fn self_update_is_refused_outside_binary_installs() {
    let (base, client, _dir, _state) = spawn().await;
    setup_admin(&base, &client).await;

    // 双保险：万一这个测试进程被摆在 target/ 之外（例如被拷走运行），它会
    // 被判成二进制部署，那时绝不能真的去替换正在运行的测试二进制。
    if akhub::update::deploy() == Deploy::Binary {
        eprintln!("跳过：当前进程会被判成原生二进制部署，不能在测试里触发真实自更新");
        return;
    }

    let response = write(&client, format!("{base}/admin/api/system/update"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    let message = body["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("源码构建") || message.contains("容器部署") || message.contains("Windows"),
        "拒绝自更新时必须说清原因与替代路径：{message}"
    );
}

#[tokio::test]
async fn restart_is_refused_when_nothing_would_start_us_again() {
    let (base, client, _dir, _state) = spawn().await;
    setup_admin(&base, &client).await;

    // 被 systemd 拉起时（INVOCATION_ID 存在）这个接口会真的触发关闭，
    // 那种环境下不做断言。
    if akhub::update::can_restart() {
        eprintln!("跳过：当前测试进程由监督进程拉起，重启接口会真的关闭服务");
        return;
    }

    let response = write(&client, format!("{base}/admin/api/system/restart"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("手工重启"),
        "拒绝重启时要说明怎么手工重启：{body}"
    );
}

#[tokio::test]
async fn the_update_endpoints_require_an_admin_session() {
    let (base, client, _dir, _state) = spawn().await;
    let anonymous = reqwest::Client::new();
    for (method, path) in [
        ("GET", "/admin/api/system/update"),
        ("POST", "/admin/api/system/update"),
        ("POST", "/admin/api/system/restart"),
    ] {
        let request = match method {
            "GET" => anonymous.get(format!("{base}{path}")),
            _ => anonymous
                .post(format!("{base}{path}"))
                .header("x-akhub-csrf", "1"),
        };
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 401, "{method} {path} 必须要求登录");
    }
    let _ = client;
}
