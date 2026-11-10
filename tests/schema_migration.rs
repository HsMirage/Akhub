//! v25 → v26 迁移（§20.1）：用 v25 的完整结构（`tests/fixtures/schema_v25.sql`，
//! 取自 v1.1.41 的 schema.sql）建一个真实形状的库，升级后核对数据。

use akhub::storage::{SCHEMA_VERSION, open};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};

const V25: &str = include_str!("fixtures/schema_v25.sql");

async fn seed(dir: &std::path::Path, version: i64, seed_sql: &str) {
    let options = SqliteConnectOptions::new()
        .filename(dir.join("akhub.sqlite"))
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    sqlx::raw_sql(V25).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO app_settings (key, value, updated_at) VALUES ('schema_version', ?, 0)",
    )
    .bind(version.to_string())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(seed_sql))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

const DATA: &str = "
INSERT INTO groups (id, name, key_prefix, key_digest_hex, multiplier_limit, weight_multiplier,
    weight_reliability, weight_first_token, weight_throughput, weight_cache_hit, queue_capacity,
    allow_degrade, created_at)
VALUES ('g1', '主力', 'akh', 'd1', 1000000, 55, 15, 5, 10, 15, 4294967295, 1, 1),
       ('g2', '备用', 'akh', 'd2', 1000000, 55, 15, 5, 10, 15, 7, 1, 2);
INSERT INTO upstream_accounts (id, group_id, name, upstream_type, base_url, preferred_protocol,
    adaptive_protocol, default_priority, calibration, multiplier_mode, manual_multiplier,
    allow_private_network, enabled, created_at)
VALUES ('a1', 'g1', '账号', 'openai', 'https://x', 'openai_chat', 1, 0, 1000000, 'manual', 1000000,
        0, 1, 1);
INSERT INTO upstream_secrets (account_id, api_key, new_api_token, updated_at, keys_migrated)
VALUES ('a1', x'AA', x'BB', 1, 1);
INSERT INTO upstream_account_keys (id, account_id, sealed_key, credential_digest, created_at)
VALUES ('k1', 'a1', x'CC', '', 1);
INSERT INTO logical_models (id, group_id, name, origin, enabled, created_at)
VALUES ('m1', 'g1', 'cat', 'auto', 1, 1),
       ('m2', 'g1', 'manual-no-row', 'manual', 1, 2),
       ('m3', 'g1', 'empty', 'manual', 1, 3);
INSERT INTO dispatch_targets (id, logical_model_id, account_id, upstream_model, hide_original,
    priority_override, enabled, created_at)
VALUES ('t1', 'm1', 'a1', 'up-cat', 0, 5, 1, 1),
       ('t2', 'm2', 'a1', 'up-manual', 0, NULL, 1, 2);
INSERT INTO account_models (account_id, upstream_model, public_name, hide_original, selected,
    missing, discovered_at)
VALUES ('a1', 'up-cat', 'cat', 1, 1, 0, 1),
       ('a1', 'up-off', 'up-off', 0, 0, 0, 1);
INSERT INTO account_aliases (account_id, upstream_model, public_name) VALUES ('a1', 'x', 'y');
";

#[tokio::test]
async fn a_v25_database_upgrades_without_losing_configuration() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 25, DATA).await;
    let pool = open(dir.path()).await.unwrap();

    let version: String =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'schema_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, SCHEMA_VERSION.to_string());

    // 删掉的列与表确实没了。
    let has_column = |table: &'static str, column: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})")))
                .fetch_all(&pool)
                .await
                .unwrap()
                .iter()
                .any(|row| row.get::<String, _>("name") == column)
        }
    };
    assert!(!has_column("upstream_accounts", "upstream_type").await);
    assert!(!has_column("upstream_secrets", "api_key").await);
    assert!(!has_column("upstream_secrets", "keys_migrated").await);
    assert!(!has_column("dispatch_targets", "hide_original").await);
    assert!(!has_column("dispatch_targets", "priority_override").await);
    assert!(!has_column("account_models", "hide_original").await);
    assert!(!has_column("logical_models", "origin").await);
    assert!(has_column("account_models", "excluded").await);
    let gone: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE name IN ('account_aliases', 'performance_buckets', 'idx_buckets_time')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(gone, 0);

    // 子表数据没有被重建账号表级联带走。
    let count = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(count("SELECT COUNT(*) FROM upstream_account_keys").await, 1);
    assert_eq!(
        count("SELECT COUNT(*) FROM upstream_secrets WHERE new_api_token IS NOT NULL").await,
        1
    );
    assert_eq!(count("SELECT COUNT(*) FROM dispatch_targets").await, 2);
    // 手工目标补进了目录（否则下一次调和就会被撤下），对外名是它的逻辑模型名。
    let public: String = sqlx::query_scalar(
        "SELECT public_name FROM account_models WHERE upstream_model = 'up-manual' AND selected = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(public, "manual-no-row");
    // 零目标逻辑模型清理，有目标的保留。
    assert_eq!(count("SELECT COUNT(*) FROM logical_models").await, 2);
    // 老库不知道谁被亲手停用过，一律算"没碰过"。
    assert_eq!(count("SELECT SUM(excluded) FROM account_models").await, 0);
    // 队列容量是管理员的设置，迁移一律不动：不限仍是不限，显式值原样保留。
    let capacities: Vec<i64> =
        sqlx::query_scalar("SELECT queue_capacity FROM groups ORDER BY created_at")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(capacities, vec![4294967295, 7]);

    // 外键动作已是 SET NULL：删分组，账号回到未分配，Key 池还在。
    sqlx::query("DELETE FROM groups WHERE id = 'g1'")
        .execute(&pool)
        .await
        .unwrap();
    let group: Option<String> =
        sqlx::query_scalar("SELECT group_id FROM upstream_accounts WHERE id = 'a1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(group, None);
    assert_eq!(count("SELECT COUNT(*) FROM upstream_account_keys").await, 1);
    assert_eq!(count("SELECT COUNT(*) FROM dispatch_targets").await, 0);
    pool.close().await;

    // 再开一次是空操作。
    open(dir.path()).await.unwrap().close().await;
}

#[tokio::test]
async fn databases_older_than_v25_are_refused_with_upgrade_advice() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 24, "").await;
    let error = open(dir.path()).await.unwrap_err().to_string();
    assert!(error.contains("先升级到 v1.1.41 再升级"), "{error}");
}

#[tokio::test]
async fn a_newer_database_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), SCHEMA_VERSION + 1, "").await;
    assert!(open(dir.path()).await.is_err());
}

#[tokio::test]
async fn a_fresh_database_matches_the_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let pool = open(dir.path()).await.unwrap();
    let version: String =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'schema_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, SCHEMA_VERSION.to_string());
    pool.close().await;
    open(dir.path()).await.unwrap().close().await;
}
