//! 一键升级接口的端到端验收（§更新）。
//!
//! 盯住三件事：走不通的部署要给出**可照做的**替代路径；接口必须立刻回执
//! （容器路径要几分钟，不能把请求挂在那里）；进度要留在服务端，刷新页面还能看到。

mod common;

use std::sync::Arc;

use akhub::app::{AppState, Settings};
use akhub::update::Deploy;
use common::FakeGithub;
use serde_json::Value;

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

fn write(client: &reqwest::Client, url: String) -> reqwest::RequestBuilder {
    client.post(url).header("x-akhub-csrf", "1")
}

async fn setup_admin(base: &str, client: &reqwest::Client) {
    let response = write(client, format!("{base}/admin/api/setup"))
        .json(&serde_json::json!({"username": "admin", "password": "correct horse battery"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

async fn status_of(base: &str, client: &reqwest::Client) -> Value {
    client
        .get(format!("{base}/admin/api/system/update"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// 状态里必须报出"能不能一键升级"，面板据此决定是给按钮还是给命令。
#[tokio::test]
async fn status_reports_whether_one_click_is_possible() {
    let (base, client, _dir, _state) = spawn().await;
    setup_admin(&base, &client).await;

    let status = status_of(&base, &client).await;
    assert!(
        status.get("can_one_click").is_some(),
        "状态里必须有一键升级标志：{status}"
    );
    assert!(
        status.get("progress").is_some(),
        "状态里必须有进度字段：{status}"
    );
    // 测试进程在 target/ 下，通常是源码构建；无论哪种形态，没有新版本时
    // 都不该出现一键入口。
    assert_eq!(
        status["can_one_click"], false,
        "没有新版本时不该出现一键升级入口：{status}"
    );
    assert_eq!(status["progress"]["phase"], "idle");
}

/// 走不通时必须说清怎么替代，而不是只回一句不行。
///
/// 复审后"走不通"只剩源码构建与 Windows：容器改成"就地替换自己"（照搬同机
/// 部署的 sub2api），不再依赖宿主机上的额外组件，所以这里改验源码构建。
#[tokio::test]
async fn one_click_is_refused_with_a_usable_alternative() {
    let (base, client, _dir, state) = spawn().await;
    setup_admin(&base, &client).await;

    state.runtime.update.set_deploy_for_tests(Deploy::Source);

    let status = status_of(&base, &client).await;
    assert_eq!(status["deploy"], "source");
    assert_eq!(
        status["can_one_click"], false,
        "源码构建不能一键升级：{status}"
    );

    // 接口本身仍然可调：它会立刻回执，失败原因写进 progress，而不是挂在请求上。
    let response = write(&client, format!("{base}/admin/api/system/upgrade"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "接口要立刻回执，不做长等待");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["started"], true);

    // 等后台任务把失败写进进度。
    let mut progress = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        progress = status_of(&base, &client).await["progress"].clone();
        if progress["phase"] == "failed" {
            break;
        }
    }
    assert_eq!(
        progress["phase"], "failed",
        "失败要落进进度里，重新打开面板还能看到：{progress}"
    );
    let error = progress["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("一键升级"),
        "失败原因要告诉用户怎么把路走通：{error}"
    );
}

/// 容器里二进制可替换时，一键升级必须**可用**——这正是本次修复的主诉。
///
/// 判据是入口脚本设的 AKHUB_ALLOW_RESTART：它只在"二进制已复制到可写卷"时出现
/// （见 deploy/docker-entrypoint.sh）。子进程里设置它，再断言面板会给按钮。
#[tokio::test]
async fn a_container_with_a_writable_binary_can_one_click() {
    if !common::cli_updates_supported() {
        eprintln!("跳过：当前平台没有命令行发行资产");
        return;
    }
    let github = FakeGithub::spawn("v9.9.9", b"not-a-real-binary".to_vec()).await;
    let (base, client, _dir, state) = spawn().await;
    state.runtime.update.set_api_base(github.base_url.clone());
    state.runtime.update.set_deploy_for_tests(Deploy::Docker);
    setup_admin(&base, &client).await;

    // 没有这个变量时容器不该出现一键入口（二进制在只读镜像层，替换不了）。
    let status = status_of(&base, &client).await;
    assert_eq!(status["has_update"], true, "{status}");
    assert_eq!(
        status["can_one_click"], false,
        "容器里的二进制不可替换时不能一键升级：{status}"
    );

    // 入口脚本声明"二进制在可写卷里"之后，一键入口必须出现。
    unsafe { std::env::set_var("AKHUB_ALLOW_RESTART", "1") };
    let status = status_of(&base, &client).await;
    assert_eq!(
        status["can_one_click"], true,
        "容器里二进制可替换时必须能一键升级：{status}"
    );
    unsafe { std::env::remove_var("AKHUB_ALLOW_RESTART") };
}

/// 有新版本、且部署支持时，一键入口必须出现。
#[tokio::test]
async fn one_click_appears_when_an_update_is_available_and_supported() {
    if !common::cli_updates_supported() {
        eprintln!("跳过：当前平台没有命令行发行资产（macOS / Windows 只发桌面端安装包）");
        return;
    }
    let github = FakeGithub::spawn("v9.9.9", b"not-a-real-binary".to_vec()).await;
    let (base, client, _dir, state) = spawn().await;
    state.runtime.update.set_api_base(github.base_url.clone());
    // 扮演原生二进制部署：这条路上"一键"是有意义的。
    state.runtime.update.set_deploy_for_tests(Deploy::Binary);
    setup_admin(&base, &client).await;

    let status = status_of(&base, &client).await;
    assert_eq!(status["has_update"], true, "{status}");
    assert_eq!(
        status["can_one_click"], true,
        "二进制部署有新版本时必须能一键升级：{status}"
    );
}

/// 一键升级接口要求管理员会话。
#[tokio::test]
async fn the_upgrade_endpoint_requires_an_admin_session() {
    let (base, _client, _dir, _state) = spawn().await;
    let anonymous = reqwest::Client::new();
    let response = anonymous
        .post(format!("{base}/admin/api/system/upgrade"))
        .header("x-akhub-csrf", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401, "一键升级必须要求登录");
}
