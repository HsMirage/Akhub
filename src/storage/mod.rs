//! SQLite 持久层：连接池、结构初始化与仓储访问。

pub mod store;

use std::path::Path;
use std::str::FromStr as _;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

pub use store::Store;

const SCHEMA: &str = include_str!("schema.sql");

/// 当前二进制认识的数据库结构版本（§27 阶段 6 的升级检查）。
///
/// 结构变更时递增并编写迁移；用**更新的** Akhub 写出的数据库不能被**更旧的**
/// 二进制打开——宁可拒绝启动，也不要在未知结构上写坏数据。
///
/// v2：请求记录补 `first_token_ms` / `input_tokens` / `output_tokens` /
/// `config_version`，并新增每次尝试明细表 `request_attempts`（§6.6、§6.8）。
/// v3：分组补"队列最长等待"`max_wait_secs`（§6.3）。
/// v4：分组补"允许托管后台"`allow_managed_background`，并新增网关托管后台
/// 任务表 `background_tasks`（计划 §29.1）。
/// v5：新增分钟级目标性能聚合表 `performance_buckets`，支撑
/// `GET /api/metrics`（§7.4、§20.1、§22）。
/// v6：请求记录补粘性等待/新鲜度、输出速度、倍率来源、额度状态、
/// 候选过滤原因与选中层（§6.6、§24.1）。
/// v7：请求记录补 Token 细分：缓存读/写与思考 Token（§11.6）。
const SCHEMA_VERSION: i64 = 7;

/// 打开（必要时创建）数据目录中的 SQLite 数据库并初始化结构。
pub async fn open(data_dir: &Path) -> Result<SqlitePool> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("创建数据目录失败：{}", data_dir.display()))?;
    // 目录里除了数据库还有 WAL/SHM 与主密钥，整目录只给属主访问（§23.2）。
    restrict_dir_to_owner(data_dir);
    let db_path = data_dir.join("akhub.sqlite");

    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .with_context(|| format!("打开数据库失败：{}", db_path.display()))?;

    apply_schema(&pool).await?;
    check_schema_version(&pool).await?;
    // 库里有加密后的上游凭据与请求元数据，文件本身只给属主读写（§23.2）。
    restrict_to_owner(&db_path);
    Ok(pool)
}

/// 把数据库文件权限收紧到 0600（非 Unix 平台静默跳过）。
fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(%error, path = %path.display(), "收紧数据库文件权限失败");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 把数据目录权限收紧到 0700（非 Unix 平台静默跳过）。
///
/// 这样即使 WAL/SHM 由 SQLite 按默认 umask 创建，也不会被同机其他用户读到。
fn restrict_dir_to_owner(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(error) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(%error, path = %dir.display(), "收紧数据目录权限失败");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// 升级检查（§27）：数据库的结构版本不得高于当前二进制。
async fn check_schema_version(pool: &SqlitePool) -> Result<()> {
    // 老库可能还没有版本行：按 0 处理，交给下面的迁移补齐。
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'schema_version'")
            .fetch_optional(pool)
            .await?;
    let stored_version: i64 = stored.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0);
    if stored_version > SCHEMA_VERSION {
        anyhow::bail!(
            "数据库结构版本 {stored_version} 比当前二进制认识的 {SCHEMA_VERSION} 新：\
             请先升级 Akhub，不要用旧版本打开新数据库"
        );
    }
    if stored_version < SCHEMA_VERSION {
        migrate(pool, stored_version).await?;
    }
    sqlx::query(
        "INSERT INTO app_settings (key, value, updated_at) VALUES ('schema_version', ?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(SCHEMA_VERSION.to_string())
    .bind(now_unix())
    .execute(pool)
    .await
    .context("写入结构版本失败")?;
    Ok(())
}

/// 逐版本迁移。只在版本落后时执行，可重复运行（按列名判断是否已存在）。
async fn migrate(pool: &SqlitePool, from: i64) -> Result<()> {
    if from < 2 {
        // v2：请求记录补用量与时机列。新库由 schema.sql 直接建好，这里只补老库。
        let existing = table_columns(pool, "request_records").await?;
        for (column, ddl) in [
            (
                "first_token_ms",
                "ALTER TABLE request_records ADD COLUMN first_token_ms INTEGER",
            ),
            (
                "input_tokens",
                "ALTER TABLE request_records ADD COLUMN input_tokens INTEGER",
            ),
            (
                "output_tokens",
                "ALTER TABLE request_records ADD COLUMN output_tokens INTEGER",
            ),
            (
                "config_version",
                "ALTER TABLE request_records ADD COLUMN config_version INTEGER",
            ),
        ] {
            if !existing.contains(column) {
                sqlx::query(ddl)
                    .execute(pool)
                    .await
                    .with_context(|| format!("迁移 request_records.{column} 失败"))?;
            }
        }
    }
    if from < 4 {
        // v4：分组的托管后台开关（老库补列；任务表由 schema.sql 的
        // CREATE TABLE IF NOT EXISTS 建好，这里不用重复建）。
        let existing = table_columns(pool, "groups").await?;
        if !existing.contains("allow_managed_background") {
            sqlx::query(
                "ALTER TABLE groups ADD COLUMN allow_managed_background INTEGER NOT NULL DEFAULT 0",
            )
            .execute(pool)
            .await
            .context("迁移 groups.allow_managed_background 失败")?;
        }
    }
    if from < 3 {
        // v3：分组补"队列最长等待"（§6.3）。默认 60 秒，0 表示跟随请求总超时。
        let existing = table_columns(pool, "groups").await?;
        if !existing.contains("max_wait_secs") {
            sqlx::query("ALTER TABLE groups ADD COLUMN max_wait_secs INTEGER NOT NULL DEFAULT 60")
                .execute(pool)
                .await
                .context("迁移 groups.max_wait_secs 失败")?;
        }
    }
    if from < 6 {
        // v6：请求记录补诊断列（§6.6、§24.1）。新库由 schema.sql 直接建好。
        let existing = table_columns(pool, "request_records").await?;
        for (column, ddl) in [
            (
                "sticky_wait_ms",
                "ALTER TABLE request_records ADD COLUMN sticky_wait_ms INTEGER",
            ),
            (
                "sticky_freshness",
                "ALTER TABLE request_records ADD COLUMN sticky_freshness REAL",
            ),
            (
                "output_tps",
                "ALTER TABLE request_records ADD COLUMN output_tps REAL",
            ),
            (
                "multiplier_source",
                "ALTER TABLE request_records ADD COLUMN multiplier_source TEXT",
            ),
            (
                "quota_status",
                "ALTER TABLE request_records ADD COLUMN quota_status TEXT",
            ),
            (
                "filter_summary",
                "ALTER TABLE request_records ADD COLUMN filter_summary TEXT",
            ),
            (
                "selected_layer",
                "ALTER TABLE request_records ADD COLUMN selected_layer INTEGER",
            ),
        ] {
            if !existing.contains(column) {
                sqlx::query(ddl)
                    .execute(pool)
                    .await
                    .with_context(|| format!("迁移 request_records.{column} 失败"))?;
            }
        }
    }
    if from < 7 {
        // v7：Token 细分（§11.6）。上游不上报的项留 NULL，绝不估算。
        let existing = table_columns(pool, "request_records").await?;
        for (column, ddl) in [
            (
                "cache_read_tokens",
                "ALTER TABLE request_records ADD COLUMN cache_read_tokens INTEGER",
            ),
            (
                "cache_write_tokens",
                "ALTER TABLE request_records ADD COLUMN cache_write_tokens INTEGER",
            ),
            (
                "reasoning_tokens",
                "ALTER TABLE request_records ADD COLUMN reasoning_tokens INTEGER",
            ),
        ] {
            if !existing.contains(column) {
                sqlx::query(ddl)
                    .execute(pool)
                    .await
                    .with_context(|| format!("迁移 request_records.{column} 失败"))?;
            }
        }
    }
    if from < 5 {
        // v5：分钟级性能聚合表。老库需要补建；新库已由 schema.sql 建好，
        // 这里用 CREATE TABLE IF NOT EXISTS 保持幂等（§20.1）。
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS performance_buckets (
                bucket_start      INTEGER NOT NULL,
                target_id         TEXT NOT NULL,
                protocol          TEXT NOT NULL,
                streaming         INTEGER NOT NULL,
                requests          INTEGER NOT NULL,
                success           INTEGER NOT NULL,
                total_ms_sum      INTEGER NOT NULL,
                first_token_sum   INTEGER NOT NULL,
                first_token_count INTEGER NOT NULL,
                output_tokens_sum INTEGER NOT NULL,
                rate_limited      INTEGER NOT NULL DEFAULT 0,
                server_errors     INTEGER NOT NULL DEFAULT 0,
                protocol_errors   INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (bucket_start, target_id, protocol, streaming)
            )",
        )
        .execute(pool)
        .await
        .context("迁移 performance_buckets 建表失败")?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_buckets_time ON performance_buckets(bucket_start DESC)",
        )
        .execute(pool)
        .await
        .context("迁移 performance_buckets 建索引失败")?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_buckets_target ON performance_buckets(target_id, bucket_start DESC)",
        )
        .execute(pool)
        .await
        .context("迁移 performance_buckets 建目标索引失败")?;
    }
    Ok(())
}

/// 读取某张表的列名集合。表名来自代码内常量，不是用户输入。
async fn table_columns(
    pool: &SqlitePool,
    table: &str,
) -> Result<std::collections::HashSet<String>> {
    Ok(
        sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})")))
            .fetch_all(pool)
            .await
            .with_context(|| format!("读取 {table} 结构失败"))?
            .iter()
            .filter_map(|row| sqlx::Row::try_get::<String, _>(row, "name").ok())
            .collect(),
    )
}

/// 打开一个仅存在于内存中的数据库，供测试使用。
pub async fn open_in_memory() -> Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true))
        .await?;
    apply_schema(&pool).await?;
    Ok(pool)
}

async fn apply_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(SCHEMA)
        .execute(pool)
        .await
        .context("初始化数据库结构失败")?;
    Ok(())
}

/// 当前 Unix 秒。
pub fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
