//! 首次设置必须带一次性口令（§23.2）。

use std::sync::Arc;

use akhub::app::{AppState, Settings};
use serde_json::json;

#[tokio::test]
async fn setup_requires_the_one_time_token_and_deletes_it_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap(dir.path(), Settings::default())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = akhub::server::router(Arc::clone(&state));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let token_file = dir.path().join("setup-token");
    let token = std::fs::read_to_string(&token_file).expect("首次启动必须写出口令文件");

    let status: serde_json::Value = client
        .get(format!("{base}/admin/api/setup/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["data_dir"], dir.path().display().to_string());

    let body = json!({"username": "admin", "password": "correct horse battery"});
    for presented in [None, Some("猜的口令")] {
        let mut request = client
            .post(format!("{base}/admin/api/setup"))
            .header("x-akhub-csrf", "1")
            .json(&body);
        if let Some(value) = presented {
            request = request.header("x-akhub-setup-token", value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 403, "没有正确口令不能抢占管理员");
    }
    assert!(state.store.needs_setup().await.unwrap());

    let response = client
        .post(format!("{base}/admin/api/setup"))
        .header("x-akhub-csrf", "1")
        .header("x-akhub-setup-token", token.trim())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(!token_file.exists(), "设置完成后口令文件必须删除");
}
