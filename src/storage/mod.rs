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
/// v8：模型别名的唯一真相移入 `account_models.public_name`，并新增
/// `account_models.hide_original` 与 `dispatch_targets.hide_original`；
/// 同一模型的不同上游名可以按对外名归并，并按需隐藏原始名。
/// v9：把"隐藏原始模型"提升为账号级 `upstream_accounts.hide_original`；
/// 打开后只暴露设置了下游模型名的模型。
/// v10：账号内 Key 池。新增 `upstream_account_keys`，请求记录、尝试明细与
/// Responses 状态链补 Key 定位列；老库的单把 Key 由
/// [`migrate_account_keys`] 展开成一把 Key 的池（§4.2.1）。
/// v11：上游类型合并成"官方 OpenAI / 官方 Anthropic"两家：`openai_compatible`
/// 归一为 `openai`，`new_api` / `sub2api` 按首选协议落到对应的官方类型。
/// v12：上游类型整个删掉（§4.2）。它不参与任何路由或倍率决策，留着只会
/// 让人以为必须选对；`upstream_accounts.upstream_type` 列保留但不再读写。
const SCHEMA_VERSION: i64 = 13;

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

    // 连接池是 C 库句柄，创建本身不碰数据库文件（真正的打开发生在第一次查询：
    // 那时 [`apply_schema`] 会建表并读结构版本）。因此这里在**运行时之前**就
    // 建立起整套连接，真正的数据库打开是异步的，不会阻塞 tokio 的工作线程。
    //
    // 这一点是刻意的：配置写操作（勾选模型、改名、调和调度目标）都是短事务，
    // 单个连接就够用，也不会互相撞 SQLite 写锁；而多连接会让"每次写操作
    // 都新开一次数据库文件"这类操作付出成倍的打开/关闭成本。
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_lazy_with(options);

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
    if from < 11 {
        // v11：上游类型归一化（见 SCHEMA_VERSION 的说明）。用事务包起来，
        // 并且**先改数据、后写版本**：中途失败整体回滚，下次启动从头再来，
        // 不会留下"版本已是 11、库里还是旧值"的分叉（§27）。
        let mut tx = pool.begin().await.context("开始 v11 迁移事务失败")?;
        sqlx::query(
            "UPDATE upstream_accounts SET upstream_type = 'openai'
              WHERE upstream_type = 'openai_compatible'",
        )
        .execute(&mut *tx)
        .await
        .context("归一化新 OpenAI 兼容账号失败")?;
        for (legacy, protocol) in [
            ("new_api", "anthropic_messages"),
            ("sub2api", "anthropic_messages"),
        ] {
            sqlx::query(
                "UPDATE upstream_accounts SET upstream_type = 'anthropic'
                  WHERE upstream_type = ? AND preferred_protocol = ?",
            )
            .bind(legacy)
            .bind(protocol)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("归一转发站账号（{legacy}，Anthropic 协议）失败"))?;
            sqlx::query(
                "UPDATE upstream_accounts SET upstream_type = 'openai'
                  WHERE upstream_type = ?",
            )
            .bind(legacy)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("归一转发站账号（{legacy}）失败"))?;
        }
        tx.commit().await.context("提交 v11 迁移失败")?;
    }
    if from < 8 {
        // v8：别名与"隐藏原始模型"的落点改为账号模型目录；旧别名表的数据
        // 在迁移时合并进来，之后运行时不再依赖 account_aliases。
        let model_columns = table_columns(pool, "account_models").await?;
        if !model_columns.contains("hide_original") {
            sqlx::query(
                "ALTER TABLE account_models ADD COLUMN hide_original INTEGER NOT NULL DEFAULT 0",
            )
            .execute(pool)
            .await
            .context("迁移 account_models.hide_original 失败")?;
        }
        let target_columns = table_columns(pool, "dispatch_targets").await?;
        if !target_columns.contains("hide_original") {
            sqlx::query(
                "ALTER TABLE dispatch_targets ADD COLUMN hide_original INTEGER NOT NULL DEFAULT 0",
            )
            .execute(pool)
            .await
            .context("迁移 dispatch_targets.hide_original 失败")?;
        }
        // 人工优先级的旧默认值是 50；新版默认 0，让同组账号默认同层、由
        // 评分决定分配。把历史默认值归零，显式设置过其它值的账号不动。
        sqlx::query(
            "UPDATE upstream_accounts SET default_priority = 0 WHERE default_priority = 50",
        )
        .execute(pool)
        .await
        .context("迁移 upstream_accounts.default_priority 失败")?;
        // 老版弹窗只写别名表，目录快照里还是上游真名。把已保存的别名
        // 回填到目录行，这样升级后不会丢历史别名。
        sqlx::query(
            "UPDATE account_models
                SET public_name = (
                    SELECT aa.public_name FROM account_aliases aa
                    WHERE aa.account_id = account_models.account_id
                      AND aa.upstream_model = account_models.upstream_model
                )
              WHERE EXISTS (
                    SELECT 1 FROM account_aliases aa
                    WHERE aa.account_id = account_models.account_id
                      AND aa.upstream_model = account_models.upstream_model
              )",
        )
        .execute(pool)
        .await
        .context("迁移 account_aliases 到 account_models 失败")?;
    }
    if from < 9 {
        // v9：隐藏原始模型从"每行/每目标"提升为账号级开关。旧库如果任一
        // 目录行或目标勾过隐藏，账号级开关也置 1，保持升级后的可见性不扩大。
        let account_columns = table_columns(pool, "upstream_accounts").await?;
        if !account_columns.contains("hide_original") {
            sqlx::query(
                "ALTER TABLE upstream_accounts ADD COLUMN hide_original INTEGER NOT NULL DEFAULT 0",
            )
            .execute(pool)
            .await
            .context("迁移 upstream_accounts.hide_original 失败")?;
        }
        sqlx::query(
            "UPDATE upstream_accounts
                SET hide_original = 1
              WHERE EXISTS (
                    SELECT 1 FROM account_models am
                    WHERE am.account_id = upstream_accounts.id AND am.hide_original != 0
              ) OR EXISTS (
                    SELECT 1 FROM dispatch_targets dt
                    WHERE dt.account_id = upstream_accounts.id AND dt.hide_original != 0
              )",
        )
        .execute(pool)
        .await
        .context("迁移账号级 hide_original 失败")?;
    }
    if from < 10 {
        // v10：Key 池的定位列。表与索引由 schema.sql 的 CREATE TABLE IF NOT
        // EXISTS 建好，这里只补老库缺的列。
        let secret_columns = table_columns(pool, "upstream_secrets").await?;
        if !secret_columns.contains("keys_migrated") {
            sqlx::query(
                "ALTER TABLE upstream_secrets ADD COLUMN keys_migrated INTEGER NOT NULL DEFAULT 0",
            )
            .execute(pool)
            .await
            .context("迁移 upstream_secrets.keys_migrated 失败")?;
        }
        let record_columns = table_columns(pool, "request_records").await?;
        for (column, ddl) in [
            (
                "key_id",
                "ALTER TABLE request_records ADD COLUMN key_id TEXT",
            ),
            (
                "key_label",
                "ALTER TABLE request_records ADD COLUMN key_label TEXT",
            ),
        ] {
            if !record_columns.contains(column) {
                sqlx::query(ddl)
                    .execute(pool)
                    .await
                    .with_context(|| format!("迁移 request_records.{column} 失败"))?;
            }
        }
        let attempt_columns = table_columns(pool, "request_attempts").await?;
        for (column, ddl) in [
            (
                "key_id",
                "ALTER TABLE request_attempts ADD COLUMN key_id TEXT",
            ),
            (
                "key_label",
                "ALTER TABLE request_attempts ADD COLUMN key_label TEXT",
            ),
        ] {
            if !attempt_columns.contains(column) {
                sqlx::query(ddl)
                    .execute(pool)
                    .await
                    .with_context(|| format!("迁移 request_attempts.{column} 失败"))?;
            }
        }
        let state_columns = table_columns(pool, "response_states").await?;
        if !state_columns.contains("key_id") {
            sqlx::query("ALTER TABLE response_states ADD COLUMN key_id TEXT")
                .execute(pool)
                .await
                .context("迁移 response_states.key_id 失败")?;
        }
        // 粘性绑定补"绑的是哪把 Key"。旧快照为 NULL，下一次使用即补齐：
        // 老会话的缓存本来就已经冷了，重新绑一次没有额外代价。
        let sticky_columns = table_columns(pool, "sticky_bindings").await?;
        if !sticky_columns.contains("credential_digest") {
            sqlx::query("ALTER TABLE sticky_bindings ADD COLUMN credential_digest TEXT")
                .execute(pool)
                .await
                .context("迁移 sticky_bindings.credential_digest 失败")?;
        }
    }
    if from < 13 {
        // v13：粘性绑定补"最近一次真正换目标的时刻"，供迁移冷却使用（§10.1 修订）。
        // 旧快照为 NULL 表示"还没迁移过"，不改变任何语义。
        let sticky_columns = table_columns(pool, "sticky_bindings").await?;
        if !sticky_columns.contains("migrated_at") {
            sqlx::query("ALTER TABLE sticky_bindings ADD COLUMN migrated_at INTEGER")
                .execute(pool)
                .await
                .context("迁移 sticky_bindings.migrated_at 失败")?;
        }
    }
    if from < 10 {
        // 老库的单把凭据展开成"一把 Key 的池"（§4.2.1）。放在迁移里而不是
        // bootstrap 里：内存库、测试与任何直接 open 的路径都走同一条路，
        // 不会有"某些调用方建出来的库里 Key 池是空的"这种状态。
        backfill_account_keys(pool).await?;
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

/// 把还停在"单把凭据"形状的账号展开成 Key 池（§4.2.1）。
///
/// **幂等**，可以反复调用：只处理 \`upstream_account_keys\` 里一行都没有、
/// 而 \`upstream_secrets.api_key\` 有值的账号。密文直接用原样搬过去——seal 的
/// 信封本身就是主密钥加密的，不需要也不该在这个没有主密钥的层里解开。
///
/// 摘要暂时写空串：它只用于把动态状态归类，而这一类账号此刻还没有任何动态
/// 状态。真正的摘要在凭据快照装载时按明文现算（[\`crate::credential\`]），
/// 那才是唯一的真相来源。
async fn backfill_account_keys(pool: &SqlitePool) -> Result<()> {
    // 判据只有"这个账号一把 Key 都没有"。\`keys_migrated\` 列保留是为了兼容
    // 旧库结构，不参与判断：任何把凭据写进 \`upstream_secrets\` 却没建 Key 行的
    // 路径（老版本、老备份）都会被这里补上。
    let pending = sqlx::query(
        "SELECT account_id, api_key FROM upstream_secrets
          WHERE api_key IS NOT NULL
            AND NOT EXISTS (
                  SELECT 1 FROM upstream_account_keys k
                   WHERE k.account_id = upstream_secrets.account_id
            )",
    )
    .fetch_all(pool)
    .await
    .context("读取待展开的账号凭据失败")?;
    if pending.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for row in &pending {
        let account_id: String = sqlx::Row::try_get(row, "account_id")?;
        let sealed: Vec<u8> = sqlx::Row::try_get(row, "api_key")?;
        sqlx::query(
            "INSERT INTO upstream_account_keys
                (id, account_id, label, sealed_key, credential_digest,
                 limit_rpm, limit_tpm, limit_concurrency, enabled, created_at)
             VALUES (?, ?, '', ?, '', NULL, NULL, NULL, 1, ?)",
        )
        .bind(format!("key_{}", ulid::Ulid::generate()))
        .bind(&account_id)
        .bind(&sealed)
        .bind(now_unix())
        .execute(&mut *tx)
        .await
        .context("展开账号 Key 池失败")?;
    }
    tx.commit().await?;
    tracing::info!(
        count = pending.len(),
        "已把老库的单把凭据展开成账号内 Key 池"
    );
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
        .connect_lazy_with(SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true));
    apply_schema(&pool).await?;
    backfill_account_keys(&pool).await?;
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
