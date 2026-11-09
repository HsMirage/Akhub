//! SQLite 持久层：连接池、结构初始化与仓储访问。

pub mod config;
pub mod store;

use std::path::Path;
use std::str::FromStr as _;

use anyhow::{Context, Result};
use sqlx::Acquire as _;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

pub use config::ConfigTx;
pub use store::Store;

const SCHEMA: &str = include_str!("schema.sql");

/// 当前二进制认识的数据库结构版本（§20.1、§27）。
///
/// `schema.sql` 就是这个版本的完整结构：新库直接按它建，已有的库只接受
/// **上一个版本**（v25，对应 v1.1.41），由 [`migrate_v25_to_v26`] 一步升上来。
/// 更老的库拒绝打开——先用 v1.1.41 把它升到 v25。用**更新的** Akhub 写出的
/// 数据库同样拒绝：宁可起不来，也不要在不认识的结构上写坏数据。
///
/// v26：账号随分组删除改为回到"未分配"（外键 SET NULL）；删掉凭据镜像
/// `upstream_secrets.api_key`、上游类型、目标的隐藏原名与优先级覆盖、目录行的
/// 隐藏原名、逻辑模型来源、旧别名表与分钟级性能聚合表；目录行补"管理员停用过"
/// `excluded`。
pub const SCHEMA_VERSION: i64 = 26;

/// 能直接升级到 [`SCHEMA_VERSION`] 的最老版本。
const OLDEST_UPGRADABLE: i64 = 25;

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
        .foreign_keys(true)
        // WAL 在检查点之后不会自动缩小；不设上限时一次大批量写入留下的
        // 几百 MB WAL 会一直占着磁盘（§22）。
        .pragma("journal_size_limit", (64 * 1024 * 1024).to_string())
        // 新建的库直接用增量回收，清理任务定期 `incremental_vacuum` 归还空闲页。
        // 对已有的 NONE 库这条 PRAGMA 不生效（需要全量 VACUUM），见
        // `Store::incremental_vacuum`。
        .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental);

    // 连接池是 C 库句柄，创建本身不碰数据库文件（真正的打开发生在第一次查询：
    // 那时 [`prepare_schema`] 会读结构版本并建表）。
    //
    // 配置写操作走 `BEGIN IMMEDIATE` 的短事务，写仍然会被 SQLite 串行化，冲突
    // 由 `busy_timeout` 兜底。但"一个连接就够"同时意味着**单点**：一条连接被
    // 卡住以后，请求记录、Responses 状态、粘性落盘与 /health/ready 会一起挂住
    // ——2026-10-08 线上就是这样卡了 31 小时。所以这里留出几条备用连接。
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        // 拿不到连接就快速失败：默认 30 秒会让每个调用点长时间挂住，
        // 日志里也看不出是谁占着连接。
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect_lazy_with(options);

    prepare_schema(&pool).await?;
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

/// 读结构版本、必要时迁移，再确保全部表与索引存在。
///
/// 顺序有讲究：先迁移、后执行 `schema.sql`。`schema.sql` 描述的是**新**结构，
/// 对一个还停在旧结构的库先执行它，任何引用新列的索引都会失败。
async fn prepare_schema(pool: &SqlitePool) -> Result<()> {
    match stored_schema_version(pool).await? {
        // 全新的库：建表与写版本放在同一个事务里，不会留下"有表没版本"的半截库。
        None => {
            let mut tx = pool.begin().await?;
            sqlx::raw_sql(SCHEMA)
                .execute(&mut *tx)
                .await
                .context("初始化数据库结构失败")?;
            write_schema_version(&mut tx).await?;
            tx.commit().await?;
            return Ok(());
        }
        Some(version) if version > SCHEMA_VERSION => anyhow::bail!(
            "数据库结构版本 {version} 比当前二进制认识的 {SCHEMA_VERSION} 新：\
             请先升级 Akhub，不要用旧版本打开新数据库"
        ),
        Some(version) if version < OLDEST_UPGRADABLE => anyhow::bail!(
            "数据库结构版本 {version} 太旧，本版本只能从 v{OLDEST_UPGRADABLE} 升级：\
             请先升级到 v1.1.41 再升级"
        ),
        Some(version) if version < SCHEMA_VERSION => migrate_v25_to_v26(pool).await?,
        Some(_) => {}
    }
    apply_schema(pool).await
}

/// 库里记录的结构版本；`None` 表示这是一个还没有任何表的新库。
async fn stored_schema_version(pool: &SqlitePool) -> Result<Option<i64>> {
    let has_settings: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'app_settings')",
    )
    .fetch_one(pool)
    .await
    .context("读取数据库结构失败")?;
    if !has_settings {
        return Ok(None);
    }
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'schema_version'")
            .fetch_optional(pool)
            .await?;
    // 有表却没有版本行，只可能是很早期的库：按 0 处理，交给"太旧"分支。
    Ok(Some(
        stored.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0),
    ))
}

async fn write_schema_version(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    sqlx::query(
        "INSERT INTO app_settings (key, value, updated_at) VALUES ('schema_version', ?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(SCHEMA_VERSION.to_string())
    .bind(now_unix())
    .execute(conn)
    .await
    .context("写入结构版本失败")?;
    Ok(())
}

/// v25 → v26 的全部语句，按顺序执行（说明见 [`migrate_v25_to_v26`]）。
const V26_STATEMENTS: &[(&str, &str)] = &[
    (
        "CREATE TABLE upstream_accounts_v26 (
            id                    TEXT PRIMARY KEY,
            group_id              TEXT REFERENCES groups(id) ON DELETE SET NULL,
            name                  TEXT NOT NULL,
            base_url              TEXT NOT NULL,
            preferred_protocol    TEXT NOT NULL,
            adaptive_protocol     INTEGER NOT NULL,
            default_priority      INTEGER NOT NULL,
            calibration           INTEGER NOT NULL,
            multiplier_mode       TEXT NOT NULL,
            manual_multiplier     INTEGER NOT NULL,
            new_api_user_id       TEXT,
            new_api_group         TEXT,
            limit_rpm             INTEGER,
            limit_tpm             INTEGER,
            limit_concurrency     INTEGER,
            allow_private_network INTEGER NOT NULL,
            enabled               INTEGER NOT NULL,
            auto_sync             INTEGER NOT NULL DEFAULT 0,
            hide_original         INTEGER NOT NULL DEFAULT 0,
            model_synced_at       INTEGER,
            created_at            INTEGER NOT NULL
        )",
        "建新账号表",
    ),
    (
        "INSERT INTO upstream_accounts_v26
            (id, group_id, name, base_url, preferred_protocol, adaptive_protocol,
             default_priority, calibration, multiplier_mode, manual_multiplier,
             new_api_user_id, new_api_group, limit_rpm, limit_tpm, limit_concurrency,
             allow_private_network, enabled, auto_sync, hide_original,
             model_synced_at, created_at)
         SELECT id, group_id, name, base_url, preferred_protocol, adaptive_protocol,
                default_priority, calibration, multiplier_mode, manual_multiplier,
                new_api_user_id, new_api_group, limit_rpm, limit_tpm, limit_concurrency,
                allow_private_network, enabled, auto_sync, hide_original,
                model_synced_at, created_at
           FROM upstream_accounts",
        "拷贝账号",
    ),
    ("DROP TABLE upstream_accounts", "删除旧账号表"),
    (
        "ALTER TABLE upstream_accounts_v26 RENAME TO upstream_accounts",
        "账号表换名",
    ),
    (
        "CREATE INDEX IF NOT EXISTS idx_accounts_group ON upstream_accounts(group_id)",
        "重建账号分组索引",
    ),
    (
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_group_name
            ON upstream_accounts(group_id, name) WHERE group_id IS NOT NULL",
        "重建账号重名索引",
    ),
    // Key 池早已是唯一真相；镜像列在池被清空时也不会跟着清，留着只会让
    // "拉取模型"之类的旁路读到一把已经删掉的 Key。
    (
        "ALTER TABLE upstream_secrets DROP COLUMN api_key",
        "删除凭据镜像",
    ),
    (
        "ALTER TABLE upstream_secrets DROP COLUMN keys_migrated",
        "删除 Key 池迁移标记",
    ),
    (
        "ALTER TABLE dispatch_targets DROP COLUMN hide_original",
        "删除目标隐藏原名",
    ),
    (
        "ALTER TABLE dispatch_targets DROP COLUMN priority_override",
        "删除目标优先级覆盖",
    ),
    (
        "ALTER TABLE account_models DROP COLUMN hide_original",
        "删除目录行隐藏原名",
    ),
    // 老库无从知道哪些未启用的行是管理员亲手停用的，一律当作"没碰过"：
    // 宁可少显示一个"你停用过"，也不要给从没见过的模型挂上它。
    (
        "ALTER TABLE account_models ADD COLUMN excluded INTEGER NOT NULL DEFAULT 0",
        "补目录行停用标记",
    ),
    // 目标从此只由目录调和生成（§16.3）：手工建的目标如果没有对应目录行，下一次
    // 调和就会被撤下。先把它们补进目录，升级不能悄悄丢调度目标。
    (
        "INSERT OR IGNORE INTO account_models
            (account_id, upstream_model, public_name, selected, excluded, missing, discovered_at)
         SELECT t.account_id, t.upstream_model, m.name, 1, 0, 0, t.created_at
           FROM dispatch_targets t
           JOIN logical_models m ON m.id = t.logical_model_id
          ORDER BY t.created_at",
        "把手工目标补进目录",
    ),
    // 同一账号 + 上游模型挂在多个逻辑模型上只可能来自旧的手工接口；目录每行只
    // 对应一个目标。保留目录行指向的那个，其余删除。
    (
        "DELETE FROM dispatch_targets
          WHERE EXISTS (
                SELECT 1 FROM logical_models m, account_models am
                 WHERE m.id = dispatch_targets.logical_model_id
                   AND am.account_id = dispatch_targets.account_id
                   AND am.upstream_model = dispatch_targets.upstream_model
                   AND am.public_name <> m.name
          )
            AND EXISTS (
                SELECT 1 FROM dispatch_targets t2
                  JOIN logical_models m2 ON m2.id = t2.logical_model_id
                  JOIN account_models am2 ON am2.account_id = t2.account_id
                                         AND am2.upstream_model = t2.upstream_model
                 WHERE t2.account_id = dispatch_targets.account_id
                   AND t2.upstream_model = dispatch_targets.upstream_model
                   AND am2.public_name = m2.name
          )",
        "清理重复目标",
    ),
    // 手工逻辑模型的入口已经删除：没有目标的逻辑模型从此不可达，一并清掉。
    (
        "DELETE FROM logical_models
          WHERE id NOT IN (SELECT logical_model_id FROM dispatch_targets)",
        "清理零目标逻辑模型",
    ),
    (
        "ALTER TABLE logical_models DROP COLUMN origin",
        "删除逻辑模型来源",
    ),
    ("DROP TABLE IF EXISTS account_aliases", "删除旧别名表"),
    ("DROP TABLE IF EXISTS performance_buckets", "删除性能聚合表"),
];

/// v25 → v26，整段在一个事务里完成，失败时库保持 v25 原样。
///
/// 删列直接用 `ALTER TABLE ... DROP COLUMN`；只有 `upstream_accounts` 要改外键
/// 动作（CASCADE → SET NULL），SQLite 改不了约束，只能重建表：建新表 → 拷数据
/// → 删旧表 → 新表改回原名。两件事必须小心：
///
/// 1. 外键打开时 `DROP TABLE` 会先做一次隐式 DELETE，级联删光 Key 池、目录与
///    目标——所以外键要先关掉，而 `PRAGMA foreign_keys` 在事务内静默无效，
///    只能在事务外、对**同一条连接**设置；
/// 2. 关掉外键之后任何一步失败都不能用 `?` 直接返回，否则连接会带着"外键已关"
///    的状态回到池里。整段收进内层 async 块，先恢复外键再决定成败。
///
/// `legacy_alter_table` 默认关闭，所以"新表改回原名"时子表里的引用按名字仍然
/// 指向它，不会被改写成临时表名。
async fn migrate_v25_to_v26(pool: &SqlitePool) -> Result<()> {
    let mut conn = pool.acquire().await.context("v26 迁移取连接失败")?;
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await
        .context("v26 迁移前关闭外键失败")?;
    let outcome: Result<()> = async {
        let mut tx = conn.begin().await.context("开始 v26 迁移事务失败")?;
        for (statement, what) in V26_STATEMENTS {
            sqlx::query(sqlx::AssertSqlSafe(*statement))
                .execute(&mut *tx)
                .await
                .with_context(|| format!("v26 迁移{what}失败"))?;
        }
        let dangling = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut *tx)
            .await
            .context("v26 迁移检查外键失败")?;
        if !dangling.is_empty() {
            anyhow::bail!(
                "v26 迁移后有 {} 处引用不完整，已回滚；请把数据库交给维护者排查",
                dangling.len()
            );
        }
        write_schema_version(&mut tx).await?;
        tx.commit().await.context("提交 v26 迁移失败")?;
        Ok(())
    }
    .await;
    let restored = sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *conn)
        .await
        .context("v26 迁移恢复外键失败");
    drop(conn);
    outcome?;
    restored?;
    tracing::info!("数据库结构已从 v25 升级到 v26");
    Ok(())
}

/// 打开一个仅存在于内存中的数据库，供测试使用。
pub async fn open_in_memory() -> Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_lazy_with(SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true));
    prepare_schema(&pool).await?;
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
