//! 配置表的读写：分组、账号、Key 池、模型目录、逻辑模型、调度目标与站点凭据。
//!
//! 写操作只有一个入口：[`ConfigTx`]，一次 `BEGIN IMMEDIATE` 事务（§21）。管理端
//! 的每一次保存都在同一个事务里完成"改行 → 调和目标 → 装配候选配置 → 提交"，
//! 中途失败整体回滚，不会留下"账号搬了、目标没搬"的中间态。读操作同一份 SQL
//! 同时服务连接池（[`Store`] 上的 `list_*`）与事务（[`ConfigTx`] 上的同名方法）。

use anyhow::{Context, Result};
use sqlx::{Row, Sqlite, SqliteConnection, SqliteExecutor, SqlitePool, Transaction};
use time::OffsetDateTime;

use super::now_unix;
use super::store::{CalibrationRecord, Store};
use crate::domain::{
    Account, DispatchTarget, Group, Limits, LogicalModel, Multiplier, MultiplierMode, Protocol,
    SchedulingWeights,
};

/// 配置修订号在 `app_settings` 里的键（§7.4 的乐观锁）。
const REVISION_KEY: &str = "config_revision";

/// 一把已落库的 Key 的元数据与密文（§4.2.1）。**不含明文**。
#[derive(Debug, Clone)]
pub struct AccountKeyRow {
    pub id: String,
    pub label: String,
    pub sealed_key: Vec<u8>,
    /// Key 级限额覆盖，逐项盖住账号默认值。
    pub limits: Limits,
    pub enabled: bool,
}

/// 准备写入 Key 池的一把 Key。密封与摘要都由调用方完成。
#[derive(Debug, Clone)]
pub struct AccountKeyWrite {
    pub id: String,
    pub label: String,
    pub sealed_key: Vec<u8>,
    /// 明文凭据的稳定摘要（hex）。空串表示"沿用旧行"，真正的摘要在凭据快照
    /// 装载时按明文现算（见 `crate::credential`）。
    pub credential_digest: String,
    pub limits: Limits,
    pub enabled: bool,
}

/// 账号模型目录里的一条记录：上游真名、对外名与选择状态（§16.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountModelRow {
    /// 上游真名（规范化后，保留大小写）。
    pub upstream_model: String,
    /// 对外名；未设置别名时等于上游真名。
    pub public_name: String,
    pub selected: bool,
    /// 管理员亲手把这一行从启用改成停用过（§16.2）。重新启用时清零。
    pub excluded: bool,
    /// 上游列表里已经消失但仍在选择集内（§16.5）。
    pub missing: bool,
    pub discovered_at: i64,
}

impl AccountModelRow {
    /// 管理员从没表达过态度的行：没启用、也没亲手停用过。只有这样的行才会
    /// 被"同组已有同名模型 → 默认勾选"改写（§16.2）。
    pub fn untouched(&self) -> bool {
        !self.selected && !self.excluded
    }
}

/// 校准助手的用量汇总（§6.8）。
#[derive(Debug, Default, Clone, Copy)]
pub struct CalibrationUsage {
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    by_tokens: i128,
    by_requests: i128,
}

impl CalibrationUsage {
    /// Token 加权均倍率：站点按 Token 扣费，大请求理应占更大权重。上游完全没
    /// 上报用量时退回按请求数加权；没有样本时为 `None`。
    pub fn weighted_multiplier(&self) -> Option<Multiplier> {
        let tokens = i128::from(self.input_tokens + self.output_tokens);
        let (sum, weight) = if tokens > 0 {
            (self.by_tokens, tokens)
        } else {
            (self.by_requests, i128::from(self.requests))
        };
        (weight > 0).then(|| Multiplier::from_raw(((sum + weight / 2) / weight) as i64))
    }
}

fn to_time(raw: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(raw).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// 站点级凭据的匹配键：去掉首尾空白与结尾斜杠，大小写不敏感。
fn normalize_site(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_ascii_lowercase()
}

// ------------------------------------------------------------------ 共享读

async fn read_groups<'e>(e: impl SqliteExecutor<'e>) -> Result<Vec<Group>> {
    let rows = sqlx::query("SELECT * FROM groups ORDER BY created_at")
        .fetch_all(e)
        .await?;
    rows.iter().map(row_to_group).collect()
}

async fn read_accounts<'e>(e: impl SqliteExecutor<'e>) -> Result<Vec<Account>> {
    let rows = sqlx::query("SELECT * FROM upstream_accounts ORDER BY created_at")
        .fetch_all(e)
        .await?;
    rows.iter().map(row_to_account).collect()
}

async fn read_account<'e>(e: impl SqliteExecutor<'e>, id: &str) -> Result<Option<Account>> {
    sqlx::query("SELECT * FROM upstream_accounts WHERE id = ?")
        .bind(id)
        .fetch_optional(e)
        .await?
        .as_ref()
        .map(row_to_account)
        .transpose()
}

/// 排序必须稳定：Key 池的抽签顺序、界面展示顺序与快照重建后的顺序都要一致，
/// 否则"同一份配置"会因为行序不同而给出不同的分配（§4.2.1）。
async fn read_account_keys<'e>(
    e: impl SqliteExecutor<'e>,
    account_id: &str,
) -> Result<Vec<AccountKeyRow>> {
    let rows = sqlx::query(
        "SELECT id, label, sealed_key, limit_rpm, limit_tpm, limit_concurrency, enabled
           FROM upstream_account_keys
          WHERE account_id = ?
          ORDER BY created_at, id",
    )
    .bind(account_id)
    .fetch_all(e)
    .await
    .context("读取账号 Key 池失败")?;
    rows.iter()
        .map(|row| {
            Ok(AccountKeyRow {
                id: row.try_get("id")?,
                label: row.try_get("label")?,
                sealed_key: row.try_get("sealed_key")?,
                limits: row_to_limits(row)?,
                enabled: row.try_get::<i64, _>("enabled")? != 0,
            })
        })
        .collect()
}

async fn read_account_models<'e>(
    e: impl SqliteExecutor<'e>,
    account_id: &str,
) -> Result<Vec<AccountModelRow>> {
    let rows =
        sqlx::query("SELECT * FROM account_models WHERE account_id = ? ORDER BY public_name")
            .bind(account_id)
            .fetch_all(e)
            .await?;
    rows.iter()
        .map(|row| {
            Ok(AccountModelRow {
                upstream_model: row.try_get("upstream_model")?,
                public_name: row.try_get("public_name")?,
                selected: row.try_get("selected")?,
                excluded: row.try_get("excluded")?,
                missing: row.try_get("missing")?,
                discovered_at: row.try_get("discovered_at")?,
            })
        })
        .collect()
}

async fn read_logical_models<'e>(e: impl SqliteExecutor<'e>) -> Result<Vec<LogicalModel>> {
    let rows = sqlx::query("SELECT * FROM logical_models ORDER BY created_at")
        .fetch_all(e)
        .await?;
    rows.iter().map(row_to_logical_model).collect()
}

async fn read_targets<'e>(e: impl SqliteExecutor<'e>) -> Result<Vec<DispatchTarget>> {
    let rows = sqlx::query("SELECT * FROM dispatch_targets ORDER BY created_at")
        .fetch_all(e)
        .await?;
    rows.iter().map(row_to_target).collect()
}

async fn read_new_api_site<'e>(
    e: impl SqliteExecutor<'e>,
    base_url: &str,
) -> Result<Option<(String, Vec<u8>)>> {
    let row = sqlx::query("SELECT user_id, sealed_token FROM new_api_sites WHERE base_url = ?")
        .bind(normalize_site(base_url))
        .fetch_optional(e)
        .await?;
    row.map(|row| Ok((row.try_get("user_id")?, row.try_get("sealed_token")?)))
        .transpose()
}

async fn read_revision<'e>(e: impl SqliteExecutor<'e>) -> Result<u64> {
    let stored: Option<String> = sqlx::query_scalar("SELECT value FROM app_settings WHERE key = ?")
        .bind(REVISION_KEY)
        .fetch_optional(e)
        .await?;
    // 从 1 起步：0 在客户端是"还没读到过版本"的意思（web/src/lib/api.ts）。
    Ok(stored.and_then(|v| v.parse().ok()).unwrap_or(1))
}

// ------------------------------------------------------------- 连接池上的读

impl Store {
    pub async fn list_groups(&self) -> Result<Vec<Group>> {
        read_groups(self.pool()).await
    }

    pub async fn list_accounts(&self) -> Result<Vec<Account>> {
        read_accounts(self.pool()).await
    }

    pub async fn account(&self, id: &str) -> Result<Option<Account>> {
        read_account(self.pool(), id).await
    }

    /// 一个账号的 Key 池，按创建时间稳定排序。
    pub async fn list_account_key_rows(&self, account_id: &str) -> Result<Vec<AccountKeyRow>> {
        read_account_keys(self.pool(), account_id).await
    }

    /// 读取 New API 倍率探针的访问令牌信封。
    pub async fn account_sealed_new_api_token(&self, account_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(
            sqlx::query_scalar("SELECT new_api_token FROM upstream_secrets WHERE account_id = ?")
                .bind(account_id)
                .fetch_optional(self.pool())
                .await?
                .flatten(),
        )
    }

    pub async fn list_account_models(&self, account_id: &str) -> Result<Vec<AccountModelRow>> {
        read_account_models(self.pool(), account_id).await
    }

    /// 分组内所有账号目录里可选的模型（§6.5）。返回（对外名, 账号名），
    /// 已标记缺失的模型不出现。
    pub async fn group_available_models(&self, group_id: &str) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT am.public_name AS public_name, a.name AS account_name
             FROM account_models am
             JOIN upstream_accounts a ON a.id = am.account_id
             WHERE a.group_id = ? AND am.missing = 0
             ORDER BY am.public_name, a.name",
        )
        .bind(group_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("public_name")?, row.try_get("account_name")?)))
            .collect()
    }

    pub async fn list_logical_models(&self) -> Result<Vec<LogicalModel>> {
        read_logical_models(self.pool()).await
    }

    pub async fn list_targets(&self) -> Result<Vec<DispatchTarget>> {
        read_targets(self.pool()).await
    }

    /// 读取某个 Base URL 的站点级 New API 凭据（§6.4）。返回 (用户 ID, 密封令牌)。
    pub async fn new_api_site(&self, base_url: &str) -> Result<Option<(String, Vec<u8>)>> {
        read_new_api_site(self.pool(), base_url).await
    }

    /// 站点级凭据列表（不回吐令牌本身）。
    pub async fn list_new_api_sites(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT base_url, user_id FROM new_api_sites ORDER BY base_url")
            .fetch_all(self.pool())
            .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("base_url")?, row.try_get("user_id")?)))
            .collect()
    }

    /// 记录账号上次完成模型同步的时间。与调度无关，不走配置事务。
    pub async fn touch_model_sync(&self, account_id: &str, at: i64) -> Result<()> {
        sqlx::query("UPDATE upstream_accounts SET model_synced_at = ? WHERE id = ?")
            .bind(at)
            .bind(account_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// 校准助手的样本（§6.8）：这个账号自己在这个模型上的成功请求，按当时的
    /// 有效倍率分桶。
    pub async fn calibration_usage(
        &self,
        account_id: &str,
        logical_model: &str,
        since: i64,
    ) -> Result<CalibrationUsage> {
        let rows = sqlx::query(
            "SELECT effective_multiplier, COUNT(*) AS requests,
                    COALESCE(SUM(input_tokens), 0) AS input_tokens,
                    COALESCE(SUM(output_tokens), 0) AS output_tokens
               FROM request_records
              WHERE account_id = ? AND logical_model = ? AND started_at >= ?
                AND http_status >= 200 AND http_status < 300 AND error_code IS NULL
                AND effective_multiplier IS NOT NULL
              GROUP BY effective_multiplier",
        )
        .bind(account_id)
        .bind(logical_model)
        .bind(since)
        .fetch_all(self.pool())
        .await
        .context("读取校准样本失败")?;
        let mut usage = CalibrationUsage::default();
        for row in &rows {
            let requests: i64 = row.try_get("requests")?;
            let input: i64 = row.try_get("input_tokens")?;
            let output: i64 = row.try_get("output_tokens")?;
            let multiplier: i64 = row.try_get("effective_multiplier")?;
            usage.requests += requests;
            usage.input_tokens += input;
            usage.output_tokens += output;
            usage.by_tokens += i128::from(multiplier) * i128::from(input + output);
            usage.by_requests += i128::from(multiplier) * i128::from(requests);
        }
        Ok(usage)
    }

    /// 开一个配置写事务。生产路径请走 `AppState::write_config`：它负责乐观锁、
    /// 修订号、候选配置校验与快照切换；这里直接暴露是为了测试夹具。
    pub async fn begin_config(&self) -> Result<ConfigTx> {
        ConfigTx::begin(self.pool()).await
    }

    // 下面几个是测试夹具的便捷写法：各自一个事务，不推进修订号、不重载配置。

    pub async fn insert_group(&self, group: &Group) -> Result<()> {
        let mut tx = self.begin_config().await?;
        tx.insert_group(group).await?;
        tx.commit().await
    }

    pub async fn delete_group(&self, id: &str) -> Result<bool> {
        let mut tx = self.begin_config().await?;
        let deleted = tx.delete_group(id).await?;
        tx.commit().await?;
        Ok(deleted)
    }

    pub async fn insert_account(&self, account: &Account) -> Result<()> {
        let mut tx = self.begin_config().await?;
        tx.insert_account(account, None).await?;
        tx.commit().await
    }

    pub async fn update_account(
        &self,
        account: &Account,
        new_api_token: Option<&[u8]>,
    ) -> Result<()> {
        let mut tx = self.begin_config().await?;
        tx.update_account(account, new_api_token).await?;
        tx.commit().await
    }

    pub async fn replace_account_keys(
        &self,
        account_id: &str,
        keys: &[AccountKeyWrite],
    ) -> Result<()> {
        let mut tx = self.begin_config().await?;
        tx.replace_account_keys(account_id, keys).await?;
        tx.commit().await
    }

    pub async fn upsert_account_model(
        &self,
        account_id: &str,
        row: &AccountModelRow,
    ) -> Result<()> {
        let mut tx = self.begin_config().await?;
        tx.upsert_account_models(account_id, std::slice::from_ref(row))
            .await?;
        tx.commit().await
    }

    pub async fn insert_logical_model(&self, model: &LogicalModel) -> Result<()> {
        let mut tx = self.begin_config().await?;
        tx.apply_config_delta(std::slice::from_ref(model), &[], &[], &[])
            .await?;
        tx.commit().await
    }

    pub async fn insert_target(&self, target: &DispatchTarget) -> Result<()> {
        let mut tx = self.begin_config().await?;
        tx.apply_config_delta(&[], std::slice::from_ref(target), &[], &[])
            .await?;
        tx.commit().await
    }
}

// ------------------------------------------------------------------ 写事务

/// 一次配置写事务（§21）。
///
/// 以 `BEGIN IMMEDIATE` 开始：一开事务就拿到写锁，"先读后写"的两个并发写者
/// 不会在提交时才发现撞车。丢弃而不提交即回滚。
///
/// **事务里只能用这里的方法读写**：在事务进行中再去用连接池读，读到的是提交
/// 前的旧状态；测试用的内存库只有一条连接，那样做会直接卡死。
pub struct ConfigTx {
    tx: Transaction<'static, Sqlite>,
}

impl ConfigTx {
    pub(crate) async fn begin(pool: &SqlitePool) -> Result<Self> {
        let tx = pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("开始配置写事务失败")?;
        Ok(Self { tx })
    }

    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await.context("提交配置写事务失败")
    }

    fn conn(&mut self) -> &mut SqliteConnection {
        &mut self.tx
    }

    // -------------------------------------------------------------- 修订号

    /// 当前配置修订号。每一次管理端写入都会推进它（§7.4）。
    pub async fn revision(&mut self) -> Result<u64> {
        read_revision(self.conn()).await
    }

    pub async fn set_revision(&mut self, revision: u64) -> Result<()> {
        self.set_app_setting(REVISION_KEY, &revision.to_string())
            .await
    }

    pub async fn set_app_setting(&mut self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now_unix())
        .execute(self.conn())
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------------ 读

    pub async fn list_groups(&mut self) -> Result<Vec<Group>> {
        read_groups(self.conn()).await
    }

    pub async fn list_accounts(&mut self) -> Result<Vec<Account>> {
        read_accounts(self.conn()).await
    }

    pub async fn account(&mut self, id: &str) -> Result<Option<Account>> {
        read_account(self.conn(), id).await
    }

    pub async fn list_account_key_rows(&mut self, account_id: &str) -> Result<Vec<AccountKeyRow>> {
        read_account_keys(self.conn(), account_id).await
    }

    pub async fn list_account_models(&mut self, account_id: &str) -> Result<Vec<AccountModelRow>> {
        read_account_models(self.conn(), account_id).await
    }

    pub async fn list_logical_models(&mut self) -> Result<Vec<LogicalModel>> {
        read_logical_models(self.conn()).await
    }

    pub async fn list_targets(&mut self) -> Result<Vec<DispatchTarget>> {
        read_targets(self.conn()).await
    }

    pub async fn new_api_site(&mut self, base_url: &str) -> Result<Option<(String, Vec<u8>)>> {
        read_new_api_site(self.conn(), base_url).await
    }

    pub async fn has_new_api_token(&mut self, account_id: &str) -> Result<bool> {
        let token: Option<Option<Vec<u8>>> =
            sqlx::query_scalar("SELECT new_api_token FROM upstream_secrets WHERE account_id = ?")
                .bind(account_id)
                .fetch_optional(self.conn())
                .await?;
        Ok(token.flatten().is_some())
    }

    // ---------------------------------------------------------------- 分组

    pub async fn insert_group(&mut self, group: &Group) -> Result<()> {
        sqlx::query(
            "INSERT INTO groups (id, name, key_prefix, key_digest_hex, multiplier_limit,
                weight_multiplier, weight_reliability, weight_first_token, weight_throughput,
                weight_cache_hit, queue_capacity, max_wait_secs, allow_degrade,
                allow_managed_background, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&group.id)
        .bind(&group.name)
        .bind(&group.key_prefix)
        .bind(&group.key_digest_hex)
        .bind(group.multiplier_limit.raw())
        .bind(group.weights.multiplier)
        .bind(group.weights.reliability)
        .bind(group.weights.first_token)
        .bind(group.weights.throughput)
        .bind(group.weights.cache_hit)
        .bind(group.queue_capacity)
        .bind(group.max_wait_secs)
        .bind(group.allow_degrade)
        .bind(group.allow_managed_background)
        .bind(group.created_at.unix_timestamp())
        .execute(self.conn())
        .await
        .context("写入分组失败")?;
        Ok(())
    }

    pub async fn update_group(&mut self, group: &Group) -> Result<()> {
        sqlx::query(
            "UPDATE groups SET name = ?, key_prefix = ?, key_digest_hex = ?, multiplier_limit = ?,
                weight_multiplier = ?, weight_reliability = ?, weight_first_token = ?,
                weight_throughput = ?, weight_cache_hit = ?, queue_capacity = ?,
                max_wait_secs = ?, allow_degrade = ?, allow_managed_background = ? WHERE id = ?",
        )
        .bind(&group.name)
        .bind(&group.key_prefix)
        .bind(&group.key_digest_hex)
        .bind(group.multiplier_limit.raw())
        .bind(group.weights.multiplier)
        .bind(group.weights.reliability)
        .bind(group.weights.first_token)
        .bind(group.weights.throughput)
        .bind(group.weights.cache_hit)
        .bind(group.queue_capacity)
        .bind(group.max_wait_secs)
        .bind(group.allow_degrade)
        .bind(group.allow_managed_background)
        .bind(&group.id)
        .execute(self.conn())
        .await
        .context("更新分组失败")?;
        Ok(())
    }

    /// 删除分组。账号经外键 SET NULL 回到未分配，逻辑模型与目标随分组级联删除。
    pub async fn delete_group(&mut self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM groups WHERE id = ?")
            .bind(id)
            .execute(self.conn())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    // ---------------------------------------------------------------- 账号

    /// 写入账号行与账号级凭据行。Key 池由 [`Self::replace_account_keys`] 另写。
    ///
    /// **同组内账号名唯一**由部分唯一索引 `idx_accounts_group_name` 兜底（见
    /// schema.sql）；管理端写入前的检查只为给出可读的报错。
    pub async fn insert_account(
        &mut self,
        account: &Account,
        new_api_token: Option<&[u8]>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO upstream_accounts (id, group_id, name, base_url,
                preferred_protocol, adaptive_protocol, default_priority, calibration,
                multiplier_mode, manual_multiplier, new_api_user_id, new_api_group,
                limit_rpm, limit_tpm, limit_concurrency, allow_private_network, enabled,
                auto_sync, hide_original, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&account.id)
        .bind(account.group_id.as_deref())
        .bind(&account.name)
        .bind(&account.base_url)
        .bind(account.preferred_protocol.as_str())
        .bind(account.adaptive_protocol)
        .bind(account.default_priority)
        .bind(account.calibration.raw())
        .bind(account.multiplier_mode.as_str())
        .bind(account.manual_multiplier.raw())
        .bind(&account.new_api_user_id)
        .bind(&account.new_api_group)
        .bind(account.limits.rpm)
        .bind(account.limits.tpm)
        .bind(account.limits.max_concurrency)
        .bind(account.allow_private_network)
        .bind(account.enabled)
        .bind(account.auto_sync)
        .bind(account.hide_original)
        .bind(account.created_at.unix_timestamp())
        .execute(self.conn())
        .await
        .context("写入账号失败")?;
        sqlx::query(
            "INSERT INTO upstream_secrets (account_id, new_api_token, updated_at) VALUES (?, ?, ?)",
        )
        .bind(&account.id)
        .bind(new_api_token)
        .bind(now_unix())
        .execute(self.conn())
        .await
        .context("写入账号凭据失败")?;
        Ok(())
    }

    /// 更新账号行；`new_api_token` 为 `None` 表示保持原令牌不变。
    pub async fn update_account(
        &mut self,
        account: &Account,
        new_api_token: Option<&[u8]>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE upstream_accounts SET group_id = ?, name = ?, base_url = ?,
                preferred_protocol = ?, adaptive_protocol = ?, default_priority = ?,
                calibration = ?, multiplier_mode = ?, manual_multiplier = ?, new_api_user_id = ?,
                new_api_group = ?, limit_rpm = ?, limit_tpm = ?, limit_concurrency = ?,
                allow_private_network = ?, enabled = ?, auto_sync = ?, hide_original = ?
             WHERE id = ?",
        )
        .bind(account.group_id.as_deref())
        .bind(&account.name)
        .bind(&account.base_url)
        .bind(account.preferred_protocol.as_str())
        .bind(account.adaptive_protocol)
        .bind(account.default_priority)
        .bind(account.calibration.raw())
        .bind(account.multiplier_mode.as_str())
        .bind(account.manual_multiplier.raw())
        .bind(&account.new_api_user_id)
        .bind(&account.new_api_group)
        .bind(account.limits.rpm)
        .bind(account.limits.tpm)
        .bind(account.limits.max_concurrency)
        .bind(account.allow_private_network)
        .bind(account.enabled)
        .bind(account.auto_sync)
        .bind(account.hide_original)
        .bind(&account.id)
        .execute(self.conn())
        .await
        .context("更新账号失败")?;
        if let Some(token) = new_api_token {
            sqlx::query(
                "UPDATE upstream_secrets SET new_api_token = ?, updated_at = ? WHERE account_id = ?",
            )
            .bind(token)
            .bind(now_unix())
            .bind(&account.id)
            .execute(self.conn())
            .await
            .context("更新账号凭据失败")?;
        }
        Ok(())
    }

    /// 清掉没有任何目标的逻辑模型（§16.3）。
    pub async fn delete_orphan_logical_models(&mut self) -> Result<()> {
        sqlx::query(
            "DELETE FROM logical_models
              WHERE id NOT IN (SELECT logical_model_id FROM dispatch_targets)",
        )
        .execute(self.conn())
        .await?;
        Ok(())
    }

    pub async fn delete_account(&mut self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM upstream_accounts WHERE id = ?")
            .bind(id)
            .execute(self.conn())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    /// 把一个账号的 Key 池写成给定列表（§4.2.1）。
    ///
    /// 按 ID 对齐：已有的行原地更新（创建时间不变，池内顺序因此不会因为改了
    /// 标签而重排），新 ID 插入，列表里没有的删除。列表为空就是清空 Key 池。
    pub async fn replace_account_keys(
        &mut self,
        account_id: &str,
        keys: &[AccountKeyWrite],
    ) -> Result<()> {
        let existing: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM upstream_account_keys WHERE account_id = ? ORDER BY created_at, id",
        )
        .bind(account_id)
        .fetch_all(self.conn())
        .await
        .context("读取账号 Key 池失败")?;
        for id in existing
            .iter()
            .filter(|id| !keys.iter().any(|key| key.id == **id))
        {
            sqlx::query("DELETE FROM upstream_account_keys WHERE id = ? AND account_id = ?")
                .bind(id)
                .bind(account_id)
                .execute(self.conn())
                .await
                .context("删除账号 Key 失败")?;
        }
        for (index, key) in keys.iter().enumerate() {
            if existing.contains(&key.id) {
                sqlx::query(
                    "UPDATE upstream_account_keys
                        SET label = ?, sealed_key = ?,
                            credential_digest = CASE WHEN ? = '' THEN credential_digest ELSE ? END,
                            limit_rpm = ?, limit_tpm = ?, limit_concurrency = ?, enabled = ?
                      WHERE id = ? AND account_id = ?",
                )
                .bind(&key.label)
                .bind(&key.sealed_key)
                .bind(&key.credential_digest)
                .bind(&key.credential_digest)
                .bind(key.limits.rpm)
                .bind(key.limits.tpm)
                .bind(key.limits.max_concurrency)
                .bind(key.enabled)
                .bind(&key.id)
                .bind(account_id)
                .execute(self.conn())
                .await
                .context("更新账号 Key 失败")?;
            } else {
                insert_account_key(self.conn(), account_id, key, index).await?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ 模型目录

    /// 整体替换一个账号的模型目录（§16.1）。
    pub async fn replace_account_models(
        &mut self,
        account_id: &str,
        rows: &[AccountModelRow],
    ) -> Result<()> {
        sqlx::query("DELETE FROM account_models WHERE account_id = ?")
            .bind(account_id)
            .execute(self.conn())
            .await?;
        self.upsert_account_models(account_id, rows).await
    }

    /// 改写（或新增）多行目录记录。
    pub async fn upsert_account_models(
        &mut self,
        account_id: &str,
        rows: &[AccountModelRow],
    ) -> Result<()> {
        for row in rows {
            sqlx::query(
                "INSERT INTO account_models (account_id, upstream_model, public_name,
                    selected, excluded, missing, discovered_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(account_id, upstream_model) DO UPDATE SET
                    public_name = excluded.public_name,
                    selected = excluded.selected,
                    excluded = excluded.excluded,
                    missing = excluded.missing",
            )
            .bind(account_id)
            .bind(&row.upstream_model)
            .bind(&row.public_name)
            .bind(row.selected)
            .bind(row.excluded)
            .bind(row.missing)
            .bind(row.discovered_at)
            .execute(self.conn())
            .await
            .context("写入账号模型目录失败")?;
        }
        Ok(())
    }

    /// 删除多行目录记录。对应的调度目标由随后的调和收走。
    pub async fn delete_account_models(
        &mut self,
        account_id: &str,
        upstream_models: &[String],
    ) -> Result<()> {
        for upstream in upstream_models {
            sqlx::query("DELETE FROM account_models WHERE account_id = ? AND upstream_model = ?")
                .bind(account_id)
                .bind(upstream)
                .execute(self.conn())
                .await?;
        }
        Ok(())
    }

    // ------------------------------------------------------ 逻辑模型与目标

    /// 落库一次"配置调和"的全部增删改（§16.3）。新模型先于目标写入（外键），
    /// 逻辑模型最后删（目标先消失，外键才放行）。
    pub async fn apply_config_delta(
        &mut self,
        models: &[LogicalModel],
        targets: &[DispatchTarget],
        target_deletes: &[String],
        model_deletes: &[String],
    ) -> Result<()> {
        for model in models {
            sqlx::query(
                "INSERT INTO logical_models (id, group_id, name, enabled, created_at)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(&model.id)
            .bind(&model.group_id)
            .bind(&model.name)
            .bind(model.enabled)
            .bind(model.created_at.unix_timestamp())
            .execute(self.conn())
            .await
            .context("写入逻辑模型失败")?;
        }
        for target in targets {
            sqlx::query(
                "INSERT INTO dispatch_targets (id, logical_model_id, account_id, upstream_model,
                    limit_rpm, limit_tpm, limit_concurrency, enabled, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    logical_model_id = excluded.logical_model_id,
                    upstream_model = excluded.upstream_model,
                    limit_rpm = excluded.limit_rpm,
                    limit_tpm = excluded.limit_tpm,
                    limit_concurrency = excluded.limit_concurrency,
                    enabled = excluded.enabled",
            )
            .bind(&target.id)
            .bind(&target.logical_model_id)
            .bind(&target.account_id)
            .bind(&target.upstream_model)
            .bind(target.limits.rpm)
            .bind(target.limits.tpm)
            .bind(target.limits.max_concurrency)
            .bind(target.enabled)
            .bind(target.created_at.unix_timestamp())
            .execute(self.conn())
            .await
            .context("写入调度目标失败")?;
        }
        for id in target_deletes {
            sqlx::query("DELETE FROM dispatch_targets WHERE id = ?")
                .bind(id)
                .execute(self.conn())
                .await?;
        }
        for id in model_deletes {
            sqlx::query("DELETE FROM logical_models WHERE id = ?")
                .bind(id)
                .execute(self.conn())
                .await?;
        }
        Ok(())
    }

    // ---------------------------------------------------- 站点凭据与校准

    /// 覆盖写入站点级 New API 凭据（§6.4）。
    pub async fn upsert_new_api_site(
        &mut self,
        base_url: &str,
        user_id: &str,
        sealed_token: &[u8],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO new_api_sites (base_url, user_id, sealed_token, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(base_url) DO UPDATE SET
                user_id = excluded.user_id,
                sealed_token = excluded.sealed_token,
                updated_at = excluded.updated_at",
        )
        .bind(normalize_site(base_url))
        .bind(user_id)
        .bind(sealed_token)
        .bind(now_unix())
        .execute(self.conn())
        .await?;
        Ok(())
    }

    pub async fn delete_new_api_site(&mut self, base_url: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM new_api_sites WHERE base_url = ?")
            .bind(normalize_site(base_url))
            .execute(self.conn())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    /// 写一条校准对账记录（§6.8）。与改账号校准系数同一事务。
    pub async fn insert_calibration_record(&mut self, record: &CalibrationRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO calibration_records (id, account_id, logical_model, period_start,
                period_end, gateway_requests, reported, calibration, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(&record.account_id)
        .bind(&record.logical_model)
        .bind(record.period_start)
        .bind(record.period_end)
        .bind(record.gateway_requests)
        .bind(&record.reported)
        .bind(&record.calibration)
        .bind(record.created_at)
        .execute(self.conn())
        .await
        .context("写入校准记录失败")?;
        Ok(())
    }
}

/// 插入一行 Key 池记录。
///
/// 用序号错开创建时间，保证同一个 Key 池内部的顺序稳定可复现：排序键是
/// `created_at, id`，而一次写入的所有行时间相同，只有错开才能让列表顺序
/// 等于界面顺序。
pub(super) async fn insert_account_key(
    conn: &mut SqliteConnection,
    account_id: &str,
    key: &AccountKeyWrite,
    index: usize,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO upstream_account_keys
            (id, account_id, label, sealed_key, credential_digest,
             limit_rpm, limit_tpm, limit_concurrency, enabled, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key.id)
    .bind(account_id)
    .bind(&key.label)
    .bind(&key.sealed_key)
    .bind(&key.credential_digest)
    .bind(key.limits.rpm)
    .bind(key.limits.tpm)
    .bind(key.limits.max_concurrency)
    .bind(key.enabled)
    .bind(now_unix() + index as i64)
    .execute(conn)
    .await
    .context("写入账号 Key 池失败")?;
    Ok(())
}

// ------------------------------------------------------------------ 行映射

fn row_to_group(row: &sqlx::sqlite::SqliteRow) -> Result<Group> {
    Ok(Group {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        key_prefix: row.try_get("key_prefix")?,
        key_digest_hex: row.try_get("key_digest_hex")?,
        multiplier_limit: Multiplier::from_raw(row.try_get("multiplier_limit")?),
        weights: SchedulingWeights {
            multiplier: row.try_get::<i64, _>("weight_multiplier")? as u32,
            reliability: row.try_get::<i64, _>("weight_reliability")? as u32,
            first_token: row.try_get::<i64, _>("weight_first_token")? as u32,
            throughput: row.try_get::<i64, _>("weight_throughput")? as u32,
            cache_hit: row.try_get::<i64, _>("weight_cache_hit")? as u32,
        },
        queue_capacity: row.try_get::<i64, _>("queue_capacity")? as u32,
        max_wait_secs: row.try_get::<i64, _>("max_wait_secs")? as u32,
        allow_degrade: row.try_get("allow_degrade")?,
        allow_managed_background: row.try_get("allow_managed_background")?,
        created_at: to_time(row.try_get("created_at")?),
    })
}

/// 从三个可空列还原限制组。
fn row_to_limits(row: &sqlx::sqlite::SqliteRow) -> Result<Limits> {
    Ok(Limits {
        rpm: row
            .try_get::<Option<i64>, _>("limit_rpm")?
            .map(|v| v as u32),
        tpm: row
            .try_get::<Option<i64>, _>("limit_tpm")?
            .map(|v| v as u32),
        max_concurrency: row
            .try_get::<Option<i64>, _>("limit_concurrency")?
            .map(|v| v as u32),
    })
}

fn row_to_account(row: &sqlx::sqlite::SqliteRow) -> Result<Account> {
    let protocol: String = row.try_get("preferred_protocol")?;
    let mode: String = row.try_get("multiplier_mode")?;
    Ok(Account {
        id: row.try_get("id")?,
        // NULL 表示未分配（§4.2.3），不是一个叫 NULL 的分组。
        group_id: row.try_get("group_id")?,
        name: row.try_get("name")?,
        base_url: row.try_get("base_url")?,
        preferred_protocol: Protocol::parse(&protocol)
            .with_context(|| format!("数据库中的协议无法识别：{protocol}"))?,
        adaptive_protocol: row.try_get("adaptive_protocol")?,
        default_priority: row.try_get::<i64, _>("default_priority")? as i32,
        calibration: Multiplier::from_raw(row.try_get("calibration")?),
        multiplier_mode: MultiplierMode::parse(&mode)
            .with_context(|| format!("数据库中的倍率来源无法识别：{mode}"))?,
        manual_multiplier: Multiplier::from_raw(row.try_get("manual_multiplier")?),
        new_api_user_id: row.try_get("new_api_user_id")?,
        new_api_group: row.try_get("new_api_group")?,
        limits: row_to_limits(row)?,
        allow_private_network: row.try_get("allow_private_network")?,
        enabled: row.try_get("enabled")?,
        auto_sync: row.try_get("auto_sync")?,
        hide_original: row.try_get("hide_original")?,
        model_synced_at: row.try_get("model_synced_at")?,
        created_at: to_time(row.try_get("created_at")?),
    })
}

fn row_to_logical_model(row: &sqlx::sqlite::SqliteRow) -> Result<LogicalModel> {
    Ok(LogicalModel {
        id: row.try_get("id")?,
        group_id: row.try_get("group_id")?,
        name: row.try_get("name")?,
        enabled: row.try_get("enabled")?,
        created_at: to_time(row.try_get("created_at")?),
    })
}

fn row_to_target(row: &sqlx::sqlite::SqliteRow) -> Result<DispatchTarget> {
    Ok(DispatchTarget {
        id: row.try_get("id")?,
        logical_model_id: row.try_get("logical_model_id")?,
        account_id: row.try_get("account_id")?,
        upstream_model: row.try_get("upstream_model")?,
        limits: row_to_limits(row)?,
        enabled: row.try_get("enabled")?,
        created_at: to_time(row.try_get("created_at")?),
    })
}
