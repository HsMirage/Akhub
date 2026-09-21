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
        None,
        akhub::storage::now_unix(),
        4096,
    );
    state.runtime.perf.observe(
        &wired.target_id,
        akhub::routing::score::Dimension {
            protocol: akhub::domain::Protocol::OpenAiChat,
            streaming: false,
        },
        &akhub::routing::score::Sample {
            success: true,
            counts: true,
            first_token: None,
            total: Duration::from_millis(500),
            output_tokens: Some(10),
        },
        akhub::storage::now_unix(),
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
async fn an_old_database_is_migrated_to_the_current_schema_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap(dir.path(), Settings::default())
        .await
        .unwrap();
    // 把当前库"降级"成 v1 形状：删掉 v2/v3/v5/v6 的列与表，版本号写回 1。
    for column in [
        "first_token_ms",
        "input_tokens",
        "output_tokens",
        "config_version",
        "sticky_wait_ms",
        "sticky_freshness",
        "output_tps",
        "multiplier_source",
        "quota_status",
        "filter_summary",
        "selected_layer",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
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
    sqlx::query("ALTER TABLE groups DROP COLUMN max_wait_secs")
        .execute(state.store.pool())
        .await
        .unwrap();
    sqlx::query("ALTER TABLE groups DROP COLUMN allow_managed_background")
        .execute(state.store.pool())
        .await
        .unwrap();
    sqlx::query("ALTER TABLE account_models DROP COLUMN hide_original")
        .execute(state.store.pool())
        .await
        .unwrap();
    sqlx::query("ALTER TABLE dispatch_targets DROP COLUMN hide_original")
        .execute(state.store.pool())
        .await
        .unwrap();
    sqlx::query("ALTER TABLE upstream_accounts DROP COLUMN hide_original")
        .execute(state.store.pool())
        .await
        .unwrap();
    sqlx::query("DROP TABLE background_tasks")
        .execute(state.store.pool())
        .await
        .unwrap();
    sqlx::query("DROP TABLE performance_buckets")
        .execute(state.store.pool())
        .await
        .unwrap();
    // 老库还有数据：一个账号、一把停在 \`upstream_secrets\` 里的凭据。
    // v1 时代没有 Key 池表，迁移必须把它展开成"一把 Key 的池"（§4.2.1）。
    sqlx::query(
        "INSERT INTO groups (id, name, key_prefix, key_digest_hex, multiplier_limit,
            weight_multiplier, weight_reliability, weight_first_token, weight_throughput,
            queue_capacity, allow_degrade, created_at)
         VALUES ('g-old', '老分组', 'akh-old', 'digest-old', 1000000, 40, 25, 20, 15, 10, 1, 1)",
    )
    .execute(state.store.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO upstream_accounts (id, group_id, name, upstream_type, base_url,
            preferred_protocol, adaptive_protocol, default_priority, calibration,
            multiplier_mode, manual_multiplier, allow_private_network, enabled, created_at)
         VALUES ('a-old', 'g-old', '老账号', 'openai', 'https://old.example.com',
            'openai_chat', 1, 0, 1000000, 'manual', 1000000, 0, 1, 1)",
    )
    .execute(state.store.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO upstream_secrets (account_id, api_key, new_api_token, updated_at)
         VALUES ('a-old', X'DEADBEEF', NULL, 1)",
    )
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
        "sticky_wait_ms",
        "sticky_freshness",
        "output_tps",
        "multiplier_source",
        "quota_status",
        "filter_summary",
        "selected_layer",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
    ] {
        assert!(columns.contains(column), "迁移后缺少列 {column}");
    }
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
        .fetch_one(reopened.store.pool())
        .await
        .unwrap();
    assert_eq!(attempts, 0, "迁移应建好空的尝试明细表");
    let group_columns: std::collections::HashSet<String> = sqlx::query("PRAGMA table_info(groups)")
        .fetch_all(reopened.store.pool())
        .await
        .unwrap()
        .iter()
        .filter_map(|row| sqlx::Row::try_get::<String, _>(row, "name").ok())
        .collect();
    assert!(
        group_columns.contains("max_wait_secs"),
        "迁移后 groups 缺少 max_wait_secs"
    );
    assert!(
        group_columns.contains("allow_managed_background"),
        "迁移后 groups 缺少 allow_managed_background"
    );
    let background_tables: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'background_tasks'")
            .fetch_one(reopened.store.pool())
            .await
            .unwrap();
    assert_eq!(background_tables, 1, "迁移后应存在 background_tasks 表");
    // v5：分钟级性能聚合表（§20.1、§22）。
    let bucket_tables: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'performance_buckets'")
            .fetch_one(reopened.store.pool())
            .await
            .unwrap();
    assert_eq!(bucket_tables, 1, "迁移后应存在 performance_buckets 表");
    // v8/v9：目录行、目标与账号都补上 hide_original。
    for table in ["account_models", "dispatch_targets", "upstream_accounts"] {
        let columns: std::collections::HashSet<String> =
            sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})")))
                .fetch_all(reopened.store.pool())
                .await
                .unwrap()
                .iter()
                .filter_map(|row| sqlx::Row::try_get::<String, _>(row, "name").ok())
                .collect();
        assert!(
            columns.contains("hide_original"),
            "迁移后 {table} 缺少 hide_original"
        );
    }
    // v10：账号内 Key 池（§4.2.1）。老库的单把凭据必须被展开成"一把 Key
    // 的池"，否则升级后每个账号都会变成"没有可用的 Key"。
    let key_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'upstream_account_keys'",
    )
    .fetch_one(reopened.store.pool())
    .await
    .unwrap();
    assert_eq!(key_tables, 1, "迁移后应存在 upstream_account_keys 表");
    let key_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_account_keys")
        .fetch_one(reopened.store.pool())
        .await
        .unwrap();
    let secrets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_secrets")
        .fetch_one(reopened.store.pool())
        .await
        .unwrap();
    let nonempty: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM upstream_secrets WHERE api_key IS NOT NULL")
            .fetch_one(reopened.store.pool())
            .await
            .unwrap();
    assert_eq!(
        key_rows, 1,
        "老库的单把凭据必须展开成一把 Key（secrets={secrets} nonempty={nonempty}）"
    );
    let opened_accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_accounts")
        .fetch_one(reopened.store.pool())
        .await
        .unwrap();
    assert_eq!(opened_accounts, 1, "老库的账号必须保留");
    // 粘性绑定也补了"绑的是哪把 Key"。
    let sticky_columns: std::collections::HashSet<String> =
        sqlx::query("PRAGMA table_info(sticky_bindings)")
            .fetch_all(reopened.store.pool())
            .await
            .unwrap()
            .iter()
            .filter_map(|row| sqlx::Row::try_get::<String, _>(row, "name").ok())
            .collect();
    assert!(
        sticky_columns.contains("credential_digest"),
        "迁移后 sticky_bindings 缺少 credential_digest"
    );
    // v13/v14：粘性绑定补"最近一次迁移时刻"与"绑定时请求体大小"（§10.1 修订）。
    assert!(
        sticky_columns.contains("migrated_at"),
        "迁移后 sticky_bindings 缺少 migrated_at：{sticky_columns:?}"
    );
    assert!(
        sticky_columns.contains("context_bytes"),
        "迁移后 sticky_bindings 缺少 context_bytes：{sticky_columns:?}"
    );
    // v11/v12：上游类型先归一、再整个停用（§4.2）。这一列现在是历史遗留，
    // 旧值不该让账号加载失败，新写入也不该再碰它。
    let stored: String =
        sqlx::query_scalar("SELECT upstream_type FROM upstream_accounts WHERE id = 'a-old'")
            .fetch_one(reopened.store.pool())
            .await
            .unwrap();
    assert_eq!(
        stored, "openai",
        "v11 迁移必须把 openai_compatible 归一为 openai，v12 之后不再改写这一列"
    );
    // 账号本身必须能读出来：类型字段已经不在领域模型里了。
    let loaded = reopened.store.list_accounts().await.unwrap();
    assert_eq!(loaded.len(), 1, "老库的账号必须能正常加载");
    assert_eq!(loaded[0].name, "老账号");
    // 凭据快照能读到这把 Key：账号不会因为升级而失去凭据。
    let account_id: String = sqlx::query_scalar("SELECT id FROM upstream_accounts LIMIT 1")
        .fetch_one(reopened.store.pool())
        .await
        .unwrap();
    assert_eq!(account_id, "a-old");
    // 这条老凭据是随便几个字节（v1 库没法伪造主密钥密封的信封），所以凭据
    // 快照会跳过它——**但绝不能因此让网关起不来**。上面 \`bootstrap\` 能成功
    // 返回本身就是在断言这一点：一把坏 Key 只影响那个账号，不影响整台网关。
    let _ = account_id;

    let version: String =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'schema_version'")
            .fetch_one(reopened.store.pool())
            .await
            .unwrap();
    // 当前版本；升级检查靠这个数字决定要不要跑迁移（§27）。
    assert_eq!(version, "14");
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

// ------------------------------------------------------------ 发布与部署契约
//
// 这些测试盯的不是运行时行为，而是「发版产物与文档是否还说同一件事」。
// 发版流程的典型失败模式正是文档写着一套、脚本做着另一套，而且只有当用户
// 照着文档敲下去才会暴露——所以把它钉在 CI 里。

/// 内嵌的管理后台版本号、/health/version 与 Cargo.toml 必须是同一个字符串。
#[tokio::test]
async fn the_reported_version_matches_cargo_manifest() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let response = client()
        .get(format!("{}/health/version", akhub.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "版本端点必须无需凭据即可访问");
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(
        body["version"],
        env!("CARGO_PKG_VERSION"),
        "版本端点与 Cargo.toml 必须一致；对不上就说明升级后跑的还是旧二进制"
    );
}

/// 版本端点不能泄露账号、模型或倍率信息。
#[tokio::test]
async fn the_version_endpoint_leaks_nothing_but_the_version() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let body: serde_json::Value = client()
        .get(format!("{}/health/version", akhub.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let object = body.as_object().expect("必须是 JSON 对象");
    assert_eq!(
        object.len(),
        3,
        "版本端点只能有 object/status/version 三个字段，实际：{object:?}"
    );
}

/// CI 与本地发版必须产出同一组平台名，否则安装脚本会去下载不存在的资产。
///
/// 平台名同时出现在三个地方：release.yml 的 matrix.name、scripts/release.sh 的
/// PLATFORMS、以及 install.sh / install.ps1 里拼资产名的那几行。任何一处漂移，
/// 用户看到的就是 404。
#[test]
fn release_platforms_agree_across_workflow_script_and_installers() {
    let workflow = std::fs::read_to_string(".github/workflows/release.yml").expect("release.yml");
    let release = std::fs::read_to_string("scripts/release.sh").expect("release.sh");
    let install_sh = std::fs::read_to_string("install.sh").expect("install.sh");
    let install_ps1 = std::fs::read_to_string("install.ps1").expect("install.ps1");

    for platform in [
        "linux-x86_64",
        "linux-aarch64",
        "linux-x86_64-musl",
        "macos-aarch64",
        "macos-x86_64",
        "windows-x86_64",
    ] {
        assert!(
            workflow.contains(&format!("name: {platform}")),
            "release.yml 的 matrix 缺少平台 {platform}"
        );
        assert!(
            release.contains(&format!("\"{platform}:")),
            "scripts/release.sh 的 PLATFORMS 缺少 {platform}"
        );
    }

    // 安装脚本要能按本机架构拼出资产名。这里不查具体三元组，只确认那段
    // 拼装逻辑还在——它一旦被改成硬编码，多架构就废了。
    for needle in ["linux-x86_64-musl", "linux-$", "macos-$"] {
        assert!(
            install_sh.contains(needle),
            "install.sh 里找不到平台拼装片段 {needle}"
        );
    }
    assert!(
        install_ps1.contains("windows-x86_64"),
        "install.ps1 必须指定 windows-x86_64 资产"
    );
}

/// 安装脚本在**真实平台上**拼出的资产名，必须与发版矩阵声明的平台名逐一对上。
///
/// 上面那个测试只检查"拼装片段还在不在"，抓不到映射结果不匹配——真实发生过：
/// install.sh 把 `arm64` 归一化成 `aarch64`，而发版矩阵当时写的是 `macos-arm64`，
/// 于是 macOS 用户下载的 URL 必然 404，而字符串检查全绿。
///
/// 这里换个做法：伪造 uname 让 install.sh 以为自己在别的平台上跑，实际执行它，
/// 再从输出里把平台名抠出来比对。测的是行为，不是文本。
#[cfg(unix)]
#[test]
fn installer_resolves_to_platform_names_that_actually_exist() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    let workflow = std::fs::read_to_string(".github/workflows/release.yml").expect("release.yml");

    // 发版矩阵真正会产出的平台名。
    let declared: Vec<String> = workflow
        .lines()
        // 只认 matrix.include 里的条目：它们的缩进是 10 个空格，
        // step 的 name 缩进是 6 个，别把步骤名混进平台集合。
        .filter_map(|line| line.strip_prefix("          - name: "))
        .map(|name| name.trim().to_string())
        .collect();
    assert!(
        declared.len() >= 6,
        "release.yml 里应当解析出至少 6 个平台名，实际 {declared:?}"
    );

    // 伪造的 uname：只回答 -s 与 -m，值取自环境变量。
    let dir = tempfile::tempdir().expect("临时目录");
    let fake_uname = dir.path().join("uname");
    std::fs::write(
        &fake_uname,
        "#!/bin/sh\ncase \"$1\" in\n  -s) echo \"$FAKE_UNAME_S\" ;;\n  -m) echo \"$FAKE_UNAME_M\" ;;\n  *) echo unknown ;;\nesac\n",
    )
    .expect("写假 uname");
    std::fs::set_permissions(&fake_uname, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // 覆盖安装脚本里每一个平台分支。
    let cases = [
        ("Linux", "x86_64", "linux-x86_64-musl"),
        ("Linux", "aarch64", "linux-aarch64"),
        ("Darwin", "arm64", "macos-aarch64"),
        ("Darwin", "x86_64", "macos-x86_64"),
    ];

    // 用 Cargo.toml 的版本构造 tag，避免测试里钉死一个会过期的版本号。
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));

    for (os, machine, expected) in cases {
        let output = Command::new("sh")
            .args(["install.sh", "--version", &tag, "--dry-run"])
            .env("PATH", &path)
            .env("FAKE_UNAME_S", os)
            .env("FAKE_UNAME_M", machine)
            .output()
            .expect("执行 install.sh");
        assert!(
            output.status.success(),
            "install.sh 在 {os}/{machine} 上失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find(|line| line.contains("将要下载"))
            .unwrap_or_else(|| panic!("{os}/{machine} 没有输出下载地址：{stdout}"));

        // 形如 .../v1.1.2/akhub-v1.1.2-<platform>.tar.gz
        let asset = line.rsplit('/').next().expect("资产名").trim();
        let prefix = format!("akhub-{tag}-");
        let platform = asset
            .strip_prefix(prefix.as_str())
            .and_then(|rest| rest.strip_suffix(".tar.gz"))
            .unwrap_or_else(|| panic!("资产名不符合约定：{asset}"));

        assert_eq!(
            platform, expected,
            "{os}/{machine} 解析出的平台名与预期不符"
        );
        assert!(
            declared.iter().any(|name| name == platform),
            "install.sh 在 {os}/{machine} 上拼出 {platform}，但 release.yml 从不产出这个名字；\
             用户会拿到 404。矩阵里是：{declared:?}"
        );
    }

    // Windows 的脚本没法在这里执行，退而求其次：把它硬编码的平台名抠出来比对。
    let install_ps1 = std::fs::read_to_string("install.ps1").expect("install.ps1");
    let ps1_platform = install_ps1
        .lines()
        .find_map(|line| line.trim().strip_prefix("$platform = '"))
        .and_then(|rest| rest.strip_suffix('\''))
        .expect("install.ps1 里应有平台名赋值");
    assert!(
        declared.iter().any(|name| name == ps1_platform),
        "install.ps1 指定的平台名 {ps1_platform}，release.yml 从不产出"
    );
}

/// Windows 必须同时给出裸 exe 与捆绑 zip，且两者都在校验和里。
///
/// Unix 那边必须打包：可执行权限靠文件模式的 +x 位，浏览器下载会丢掉它，
/// 裸传 ELF 用户拿到的是「权限不足」。Windows 没有这个问题——能不能跑只看
/// 扩展名——所以「必须打包」的理由在这里不成立，剩下的只是压缩收益
/// （17 MB -> 6 MB）和顺带捎上文档。让用户为这两点被迫多走
/// 「解压 -> 进一层目录 -> 运行」并不划算，所以两个都给。
#[test]
fn windows_ships_both_a_bare_exe_and_a_bundle() {
    let package = std::fs::read_to_string("scripts/package.sh").expect("package.sh");
    let workflow = std::fs::read_to_string(".github/workflows/release.yml").expect("release.yml");
    let install_ps1 = std::fs::read_to_string("install.ps1").expect("install.ps1");

    // 打包侧要产出裸 exe。
    assert!(
        package.contains(r#"cp "$STAGE/akhub.exe" "dist/$ASSET.exe""#),
        "package.sh 必须为 Windows 额外产出一个裸 exe"
    );
    assert!(
        package.contains(r#"ARTIFACT="$ASSET.zip $ASSET.exe""#),
        "package.sh 的 Windows 分支必须同时声明 zip 与 exe 两个产物"
    );

    // 流水线要把 exe 一起上传，并且**算进校验和**——否则用户没法校验
    // 那个他直接下载的文件，而校验和的意义就在于覆盖每一个可下载的产物。
    assert!(
        workflow.matches("dist/*.exe").count() >= 2,
        "release.yml 必须在上传与发布两处都包含 dist/*.exe"
    );
    let checksum_line = workflow
        .lines()
        .find(|line| line.contains("sha256sum ./*.tar.gz"))
        .expect("release.yml 里应有校验和生成命令");
    assert!(
        checksum_line.contains("./*.exe"),
        "校验和必须覆盖裸 exe，实际命令：{checksum_line}"
    );

    // 安装脚本仍然按 zip 装（包里才有 install.ps1 与文档），这一点不能被打乱。
    assert!(
        install_ps1.contains(r#"$asset = "akhub-$tag-$platform.zip""#),
        "install.ps1 应当继续下载 zip 包，而不是裸 exe"
    );
}

/// 镜像路径必须全小写。
///
/// GitHub 仓库是 HsMirage/Akhub，直接拿去拼 ghcr.io/<owner>/<repo> 会得到
/// 含大写的路径，而 Docker 会拒绝这样的 repository 名——用户复制文档里的
/// docker run 命令只会看到一个和文档内容毫不相干的报错。
#[test]
fn container_image_paths_are_lowercase() {
    let install_sh = std::fs::read_to_string("install.sh").expect("install.sh");
    assert!(
        install_sh.contains("tr '[:upper:]' '[:lower:]'"),
        "install.sh 必须把仓库名转成小写再拼镜像路径"
    );

    for name in ["docker-compose.yml", "deploy/README.md", "README.md"] {
        let content =
            std::fs::read_to_string(name).unwrap_or_else(|e| panic!("读不到 {name}：{e}"));
        for (index, _) in content.match_indices("ghcr.io/") {
            let after = &content[index + "ghcr.io/".len()..];
            let end = after
                .find(|c: char| c.is_whitespace() || c == '`' || c == '"' || c == '\\')
                .unwrap_or(after.len());
            let path = &after[..end];
            assert_eq!(
                path,
                path.to_lowercase(),
                "{name} 里的镜像路径 {path} 含大写字母，Docker 会拒绝"
            );
        }
    }
}

/// 发布资产的命名规则必须在打包脚本与安装脚本之间保持一致。
#[test]
fn artifact_naming_is_consistent_between_packager_and_installer() {
    let package = std::fs::read_to_string("scripts/package.sh").expect("package.sh");
    let install_sh = std::fs::read_to_string("install.sh").expect("install.sh");
    let install_ps1 = std::fs::read_to_string("install.ps1").expect("install.ps1");

    // 打包侧：akhub-<tag>-<platform>.tar.gz / .zip
    assert!(
        package.contains("ASSET=\"akhub-$TAG-$PLATFORM\""),
        "package.sh 的资产命名变了，安装脚本会找不到文件"
    );
    assert!(
        install_sh.contains("asset=\"akhub-${tag}-${platform}.tar.gz\""),
        "install.sh 的资产名模板与 package.sh 不一致"
    );
    assert!(
        install_ps1.contains("$asset = \"akhub-$tag-$platform.zip\""),
        "install.ps1 的资产名模板与 package.sh 不一致"
    );

    // 发行包必须同时带上两个安装脚本：Windows 文档让用户去运行
    // install.ps1，包里没有它就会指向一个不存在的文件。
    assert!(
        package.contains("deploy install.sh install.ps1"),
        "package.sh 必须把 install.sh 与 install.ps1 都放进发行包"
    );

    // 归档内层目录名 == 资产名去掉扩展名，安装脚本按这个规则定位可执行文件。
    assert!(
        package.contains("STAGE=\"dist/$ASSET\""),
        "package.sh 的归档内层目录必须等于资产名"
    );
    assert!(
        install_sh.contains("inner=\"${tmp}/akhub-${tag}-${platform}\""),
        "install.sh 的内层目录推断与 package.sh 不一致"
    );
}

/// 停止宽限期的默认值必须在四处说得一致（§25.3）。
///
/// 这个数字只要有一处偏小，后果都是在途的长流式请求被强杀：代码里的默认值、
/// systemd 的 TimeoutStopSec、Docker 的 stop_grace_period、镜像的 STOPSIGNAL。
#[test]
fn the_shutdown_grace_default_is_consistent_everywhere() {
    let main = std::fs::read_to_string("src/main.rs").expect("main.rs");
    assert!(
        main.contains("env_duration(\"AKHUB_SHUTDOWN_GRACE_SECS\", 180)"),
        "代码里的关闭宽限期默认值被改动，下面三处都要跟着改"
    );

    let service = std::fs::read_to_string("deploy/akhub.service").expect("akhub.service");
    assert!(
        service.contains("TimeoutStopSec=200"),
        "systemd 的 TimeoutStopSec 必须大于 180 秒的宽限期"
    );

    let compose = std::fs::read_to_string("docker-compose.yml").expect("docker-compose.yml");
    assert!(
        compose.contains("stop_grace_period: 200s"),
        "compose 的 stop_grace_period 必须大于 180 秒的宽限期"
    );

    let dockerfile = std::fs::read_to_string("Dockerfile").expect("Dockerfile");
    assert!(
        dockerfile.contains("STOPSIGNAL SIGTERM"),
        "镜像必须显式声明 SIGTERM，否则优雅关闭不会触发"
    );
}

/// 部署文档里引用的仓库文件必须真的存在。
///
/// 文档链接失效是发版重构最常见的副作用：改了脚本名、忘了改文档，
/// 而这条路径只有用户会走到。
#[test]
fn deployment_docs_only_reference_files_that_exist() {
    let tick = '\u{60}';
    let mut checked = 0usize;

    for name in [
        "deploy/README.md",
        "deploy/README.windows.md",
        "README.md",
        "install.sh",
        "install.ps1",
    ] {
        let content =
            std::fs::read_to_string(name).unwrap_or_else(|e| panic!("读不到 {name}：{e}"));

        let mut rest = content.as_str();
        while let Some(start) = rest.find(tick) {
            let after = &rest[start + 1..];
            let Some(end) = after.find(tick) else { break };
            let candidate = &after[..end];
            rest = &after[end + 1..];

            if candidate.contains(char::is_whitespace) || candidate.contains(',') {
                continue;
            }
            // 反斜杠写法（Windows 文档）统一成正斜杠再判断。
            let path = candidate.replace('\\', "/");
            let path = path.trim_start_matches("./");

            // 只关心明确像「仓库内相对路径」的引用。
            let looks_like_path = path.ends_with(".sh")
                || path.ends_with(".ps1")
                || path.ends_with(".yml")
                || path.ends_with(".service")
                || path.ends_with(".conf")
                || path.ends_with("Dockerfile")
                || path == "docker-compose.yml"
                || path == "Caddyfile"
                || path == "Caddyfile.windows"
                || path == "deploy/README.windows.md";
            if !looks_like_path {
                continue;
            }
            // 绝对路径、盘符路径与环境变量展开出来的路径不在仓库里。
            if path.starts_with('/')
                || path.starts_with("C:/")
                || path.contains('%')
                || path.contains('$')
            {
                continue;
            }
            // 发行包内 / Release 附件里的文件名不要求在仓库里存在。
            if path.contains("akhub-v")
                || path.starts_with("dist/")
                || path.ends_with("checksums.txt")
                || path.ends_with("akhub.exe")
            {
                continue;
            }
            assert!(
                std::path::Path::new(path).exists(),
                "{name} 引用了不存在的文件：{candidate}"
            );
            checked += 1;
        }
    }

    assert!(checked >= 8, "应当核对到若干仓库内文件引用，实际 {checked}");
}
