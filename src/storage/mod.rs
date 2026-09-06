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
const SCHEMA_VERSION: i64 = 1;

/// 打开（必要时创建）数据目录中的 SQLite 数据库并初始化结构。
pub async fn open(data_dir: &Path) -> Result<SqlitePool> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("创建数据目录失败：{}", data_dir.display()))?;
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
    Ok(pool)
}

/// 升级检查（§27）：数据库的结构版本不得高于当前二进制。
async fn check_schema_version(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO app_settings (key, value, updated_at) VALUES ('schema_version', ?, ?)",
    )
    .bind(SCHEMA_VERSION.to_string())
    .bind(now_unix())
    .execute(pool)
    .await
    .context("写入结构版本失败")?;

    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'schema_version'")
            .fetch_one(pool)
            .await?;
    let stored_version: i64 = stored
        .and_then(|v| v.parse().ok())
        .unwrap_or(SCHEMA_VERSION);
    if stored_version > SCHEMA_VERSION {
        anyhow::bail!(
            "数据库结构版本 {stored_version} 比当前二进制认识的 {SCHEMA_VERSION} 新：\
             请先升级 Akhub，不要用旧版本打开新数据库"
        );
    }
    Ok(())
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
