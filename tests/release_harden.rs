//! 阶段 6 发布硬化（§25.3、§26.7、§26.8）：优雅关闭、磁盘异常与 SQLite 恢复。
//!
//! 单实例意味着进程重启是常态恢复路径：关闭时最后一笔快照必须落盘，
//! 重启后粘性与评分从持久层恢复，数据库文件损坏时明确报错而不是静默重建。

mod common;

use akhub::app::{AppState, Settings};
use common::{TargetSpec, client, spawn_akhub_with, wire_target};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn graceful_shutdown_flushes_the_last_snapshot_before_exit() {
    // §22、§25.3：注入式停止信号触发后，粘性绑定必须先落盘再退出。
    // 绑定必须指向真实存在的调度目标：后台清理会回收指向已消失目标的行。
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let upstream = common::FakeUpstream::spawn().await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            akhub::domain::Protocol::OpenAiChat,
            "m1",
            "up",
            50,
        ),
    )
    .await;
    let state = std::sync::Arc::clone(&akhub.state);

    let (key, _origin) = akhub::routing::sticky::derive(
        &state.key_digest,
        &akhub.group_id,
        "m1",
        &axum_headers_with_session(),
        &serde_json::json!({}),
    )
    .expect("会话头必须能推导粘性键");
    let key_text = key.as_str().to_string();
    state.runtime.sticky.bind(
        key,
        &akhub.group_id,
        "m1",
        &wired.target_id,
        akhub::storage::now_unix(),
    );
    state.runtime.perf.observe(
        &wired.target_id,
        akhub::routing::score::Dimension {
            protocol: akhub::domain::Protocol::OpenAiChat,
            streaming: false,
        },
        &akhub::routing::score::Sample {
            success: true,
            first_token: None,
            total: Duration::from_millis(500),
            output_tokens: Some(10),
        },
    );

    // 模拟收到停止信号：立即触发的 future。
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tx.send(()).unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // 端口留给 serve_with_shutdown 绑定。

    // 关闭完成后 serve 返回；期间 flush_snapshots 已执行。
    akhub::server::serve_with_shutdown(std::sync::Arc::clone(&state), addr, async move {
        let _ = rx.await;
    })
    .await
    .unwrap();

    // 重启：粘性绑定从盘上回来。数据目录句柄必须保活——drop 掉 Akhub
    // 会把 TempDir 整个删掉，bootstrap 就是在空目录上重建假象。
    let dir = akhub.data_dir().to_path_buf();
    std::mem::forget(akhub);
    let restarted = AppState::bootstrap(&dir, Settings::default())
        .await
        .unwrap();
    let loaded = restarted.store.load_sticky_bindings(0).await.unwrap();
    assert!(
        loaded.iter().any(|b| b.sticky_key == key_text),
        "关闭前的粘性绑定必须落盘：{loaded:?}"
    );
}

/// 回归：宽限期必须从**收到停止信号之后**才开始计时。
///
/// 早期实现用 `timeout(grace, serve)` 包住整个服务，导致进程每 180 秒自杀一次
/// （线上表现为 systemd 反复重启、管理会话丢失）。这里用 150ms 的宽限期，
/// 先验证没有信号时服务在几倍宽限期之后仍然活着，再发信号验证它会退出。
#[tokio::test]
async fn the_grace_period_only_starts_after_the_stop_signal() {
    let akhub = spawn_akhub_with(
        Settings {
            shutdown_grace: Duration::from_millis(150),
            ..Settings::default()
        },
        |_| {},
    )
    .await;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // 端口留给 serve_with_shutdown 绑定。
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let state = std::sync::Arc::clone(&akhub.state);
    let serving = tokio::spawn(async move {
        akhub::server::serve_with_shutdown(state, addr, async move {
            let _ = rx.await;
        })
        .await
    });

    let http = client();
    let mut alive = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if http
            .get(format!("http://{addr}/health/live"))
            .send()
            .await
            .map(|response| response.status().is_success())
            .unwrap_or(false)
        {
            alive = true;
            break;
        }
    }
    assert!(alive, "服务没有起来");

    // 超过宽限期数倍仍未收到信号：绝不能自行退出。
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !serving.is_finished(),
        "没有停止信号就不该退出——宽限期不能从启动时开始计时"
    );
    let response = http
        .get(format!("http://{addr}/health/live"))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "服务必须仍然可用");

    // 发信号后正常收口。
    tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("收到信号后必须退出")
        .unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn a_corrupt_database_fails_loudly_instead_of_being_silently_recreated() {
    // §26.8：主库文件损坏时启动必须失败，绝不能"当作新库"继续跑。
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("akhub.sqlite");
    std::fs::write(&db_path, b"this is not a sqlite file at all").unwrap();

    let result = AppState::bootstrap(dir.path(), Settings::default()).await;
    assert!(result.is_err(), "损坏的数据库不能静默重建");
    let message = format!("{:#}", result.err().unwrap());
    assert!(
        message.contains("数据库") || message.contains("sqlite") || message.contains("SQLite"),
        "错误信息要指向数据库问题：{message}"
    );
}

#[tokio::test]
async fn wal_sidecars_are_recovered_and_the_same_directory_reopens() {
    // 模拟进程被 SIGKILL：WAL 与 SHM 文件遗留，重新打开必须能继续。
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let dir = akhub.data_dir().to_path_buf();

    // 手写一条数据并制造 WAL 侧文件存在的事实。
    sqlx::query(
        "INSERT OR REPLACE INTO app_settings (key, value, updated_at) VALUES ('probe', '1', ?)",
    )
    .bind(akhub::storage::now_unix())
    .execute(akhub.state.store.pool())
    .await
    .unwrap();
    assert!(dir.join("akhub.sqlite").exists());

    // 同目录再次 bootstrap（等价于崩溃后重启）。
    let restarted = AppState::bootstrap(&dir, Settings::default())
        .await
        .unwrap();
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'probe'")
            .fetch_one(restarted.store.pool())
            .await
            .unwrap();
    assert_eq!(value.as_deref(), Some("1"), "崩溃前的写入必须幸存");
}

// ------------------------------------------------------------ 错误码契约

#[tokio::test]
async fn gateway_error_http_status_codes_drive_correct_client_retry() {
    // §18.3：429/5xx 让客户端重试，4xx 让客户端停止。逐码验证映射。
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let upstream = common::FakeUpstream::spawn().await;
    // pinned 保证调度直达这个目标；脚本行为由各场景控制。
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            akhub::domain::Protocol::OpenAiChat,
            "m1",
            "up",
            50,
        )
        .pinned(),
    )
    .await;
    let http = client();
    let url = format!("{}/v1/chat/completions", akhub.base_url);
    let key = &akhub.key;
    let body = json!({"model": "m1", "messages": [{"role": "user", "content": "hi"}]});

    let post = |code: u16| {
        let upstream = upstream.clone();
        let http = http.clone();
        let url = url.clone();
        let body = body.clone();
        let key = key.clone();
        async move {
            upstream.fallback(common::Behavior::Json(
                code,
                json!({"error": {"message": "x"}}),
            ));
            let response = http
                .post(&url)
                .bearer_auth(&key)
                .json(&body)
                .send()
                .await
                .unwrap();
            (code, response.status().as_u16())
        }
    };

    // 上游 429 → 网关对客户端保持 429（可重试）。
    assert_eq!(post(429).await, (429, 429));
    // 上游 500：单目标试尽 → upstream_exhausted → 503（§18.3，客户端继续重试）。
    assert_eq!(post(500).await, (500, 503));
    // 上游 502：同样是切换类失败，耗尽后按 upstream_exhausted 报 503。
    // 客户端拿到 5xx 都会重试，语义符合 §18.3 的重试契约。
    assert_eq!(post(502).await, (502, 503));
    // 上游 504 → upstream_timeout → 504。
    assert_eq!(post(504).await, (504, 504));

    // 不可重试类：模型不存在必须 404，认证失败必须 401。
    let response = http
        .post(&url)
        .bearer_auth(key)
        .json(&json!({"model": "不存在的模型", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404, "未知模型必须 404");

    let response = http
        .post(&url)
        .bearer_auth("akh-00000000000000000000000000000000")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401, "坏 Key 必须 401");
}

#[tokio::test]
async fn oversized_requests_reject_with_413_before_any_upstream_call() {
    // §17.3：请求体超限必须 413 且绝不外发。
    let settings = Settings {
        max_request_bytes: 1024,
        ..Settings::default()
    };
    let akhub = spawn_akhub_with(settings, |_| {}).await;
    let upstream = common::FakeUpstream::spawn().await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "A",
            &upstream.base_url,
            akhub::domain::Protocol::OpenAiChat,
            "m1",
            "up",
            50,
        ),
    )
    .await;

    let response = client()
        .post(format!("{}/v1/chat/completions", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({
            "model": "m1",
            "messages": [{"role": "user", "content": "字".repeat(4096)}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(upstream.requests(), 0, "超限请求绝不能外发");
}

/// 带 `x-session-id` 的请求头，供 sticky::derive 走"显式会话头"分支。
fn axum_headers_with_session() -> axum::http::HeaderMap {
    use axum::http::header::{HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-session-id",
        HeaderValue::from_static("release-harden-session"),
    );
    headers
}

#[tokio::test]
async fn a_database_written_by_a_newer_binary_refuses_to_open() {
    // §27 升级检查：新版本写出的数据库绝不能被旧二进制静默打开。
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap(dir.path(), Settings::default())
        .await
        .unwrap();
    // 把结构版本拨到未来，模拟"这份数据来自更新版本的 Akhub"。
    sqlx::query("UPDATE app_settings SET value = '999' WHERE key = 'schema_version'")
        .execute(state.store.pool())
        .await
        .unwrap();
    std::mem::forget(state);

    let result = AppState::bootstrap(dir.path(), Settings::default()).await;
    assert!(result.is_err(), "未来版本的数据库必须拒绝打开");
    let message = format!("{:#}", result.err().unwrap());
    assert!(message.contains("升级"), "错误信息要提示升级：{message}");
}

/// v1 的老库打开时必须自动迁移到 v2：补上用量/时机列与尝试明细表（§27）。
#[tokio::test]
async fn a_v1_database_is_migrated_to_v2_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap(dir.path(), Settings::default())
        .await
        .unwrap();
    // 把当前库"降级"成 v1 形状：删掉 v2 的列与表，并把版本号写回 1。
    for column in [
        "first_token_ms",
        "input_tokens",
        "output_tokens",
        "config_version",
    ] {
        // 列名来自下面的常量数组，不含用户输入。
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE request_records DROP COLUMN {column}"
        )))
        .execute(state.store.pool())
        .await
        .unwrap();
    }
    sqlx::query("DROP TABLE request_attempts")
        .execute(state.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE app_settings SET value = '1' WHERE key = 'schema_version'")
        .execute(state.store.pool())
        .await
        .unwrap();
    std::mem::forget(state);

    // 重新打开：迁移补回列与表，版本更新到 2。
    let reopened = AppState::bootstrap(dir.path(), Settings::default())
        .await
        .unwrap();
    let columns: std::collections::HashSet<String> =
        sqlx::query("PRAGMA table_info(request_records)")
            .fetch_all(reopened.store.pool())
            .await
            .unwrap()
            .iter()
            .filter_map(|row| sqlx::Row::try_get::<String, _>(row, "name").ok())
            .collect();
    for column in [
        "first_token_ms",
        "input_tokens",
        "output_tokens",
        "config_version",
    ] {
        assert!(columns.contains(column), "迁移后缺少列 {column}");
    }
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
        .fetch_one(reopened.store.pool())
        .await
        .unwrap();
    assert_eq!(attempts, 0, "迁移应建好空的尝试明细表");
    let version: String =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'schema_version'")
            .fetch_one(reopened.store.pool())
            .await
            .unwrap();
    assert_eq!(version, "2");
}

/// 第三方声明里的版本必须与 Cargo.lock 一致。
///
/// 阶段 6 包含"许可证与第三方声明审查"；声明漂移过一次（base64、getrandom、
/// sha2、tower-http 都停留在旧版本），所以用测试把它钉住。
#[test]
fn third_party_notices_match_the_lockfile() {
    let lock = std::fs::read_to_string("Cargo.lock").expect("Cargo.lock");
    let notices = std::fs::read_to_string("NOTICES.md").expect("NOTICES.md");

    // 解析 Cargo.lock 的 name/version 对。
    let mut versions = std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for line in lock.lines() {
        if let Some(name) = line.strip_prefix("name = \"") {
            current = Some(name.trim_end_matches('"').to_string());
        } else if let Some(version) = line.strip_prefix("version = \"")
            && let Some(name) = current.take()
        {
            versions.insert(name, version.trim_end_matches('"').to_string());
        }
    }

    let mut checked = 0usize;
    for line in notices.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 4 {
            continue;
        }
        let names: Vec<&str> = cells[1].split('/').map(str::trim).collect();
        let pinned: Vec<&str> = cells[2].split('/').map(str::trim).collect();
        if names.len() != pinned.len() || names.len() > 2 {
            continue;
        }
        // 只处理"每个版本都形如数字.数字"的表格行。
        if !pinned
            .iter()
            .all(|version| version.split('.').all(|part| part.parse::<u32>().is_ok()))
        {
            continue;
        }
        for (name, version) in names.iter().zip(pinned.iter()) {
            let Some(locked) = versions.get(*name) else {
                continue;
            };
            assert_eq!(
                locked, version,
                "NOTICES.md 里 {name} 声明 {version}，Cargo.lock 是 {locked}"
            );
            checked += 1;
        }
    }
    assert!(checked >= 15, "至少应该核对到主要直接依赖，实际 {checked}");
}
