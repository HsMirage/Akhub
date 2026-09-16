//! 仓储层：领域对象的读写。所有 SQL 集中在这里，其余模块只见领域类型。

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;

use super::now_unix;
use crate::domain::{
    Account, DispatchTarget, Group, Limits, LogicalModel, ModelOrigin, Multiplier, MultiplierMode,
    Protocol, SchedulingWeights, UpstreamType,
};
/// 一个账号的全部加密凭据信封。
///
/// 更新时 `None` 表示"保持原值"——后台不提供读取完整 Key 的接口（§23.2），
/// 所以"没填"和"清空"必须是两件事，用 `Option` 而不是空字符串区分。
#[derive(Debug, Default)]
pub struct AccountSecrets {
    pub api_key: Option<Vec<u8>>,
    /// New API 倍率探针的访问令牌，与推理凭据是两把不同的 Key（§11.2）。
    pub new_api_token: Option<Vec<u8>>,
}

impl AccountSecrets {
    /// 新建账号时使用：API Key 必填。
    pub fn new(api_key: Vec<u8>, new_api_token: Option<Vec<u8>>) -> Self {
        Self {
            api_key: Some(api_key),
            new_api_token,
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

fn to_time(raw: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(raw).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", ulid::Ulid::generate())
}

impl Store {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ---------------------------------------------------------------- 管理员

    /// 是否尚未完成首次设置。
    pub async fn needs_setup(&self) -> Result<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_users")
            .fetch_one(&self.pool)
            .await?;
        Ok(count == 0)
    }

    /// 创建首个管理员。已存在管理员时拒绝，防止设置接口被重复调用。
    pub async fn create_admin(&self, username: &str, password_hash: &str) -> Result<()> {
        let now = now_unix();
        let affected = sqlx::query(
            "INSERT INTO admin_users (id, username, password_hash, created_at, updated_at)
             SELECT ?, ?, ?, ?, ? WHERE NOT EXISTS (SELECT 1 FROM admin_users)",
        )
        .bind(new_id("adm"))
        .bind(username)
        .bind(password_hash)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if affected == 0 {
            bail!("管理员已存在，首次设置只能执行一次");
        }
        Ok(())
    }

    /// 取出管理员的密码哈希，供登录校验使用。
    pub async fn admin_password_hash(&self, username: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE username = ?")
                .bind(username)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 覆盖管理员的密码哈希。返回是否真的改到了一行。
    pub async fn update_admin_password(&self, username: &str, password_hash: &str) -> Result<bool> {
        let affected = sqlx::query(
            "UPDATE admin_users SET password_hash = ?, updated_at = ? WHERE username = ?",
        )
        .bind(password_hash)
        .bind(now_unix())
        .bind(username)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// 读取一条应用级设置（后台可改的系统参数）。
    pub async fn app_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT value FROM app_settings WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 写入一条应用级设置。
    pub async fn set_app_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------------ 分组

    pub async fn insert_group(&self, group: &Group) -> Result<()> {
        sqlx::query(
            "INSERT INTO groups (id, name, key_prefix, key_digest_hex, multiplier_limit,
                weight_multiplier, weight_reliability, weight_first_token, weight_throughput,
                queue_capacity, allow_degrade, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(group.queue_capacity)
        .bind(group.allow_degrade)
        .bind(group.created_at.unix_timestamp())
        .execute(&self.pool)
        .await
        .context("写入分组失败")?;
        Ok(())
    }

    pub async fn update_group(&self, group: &Group) -> Result<()> {
        sqlx::query(
            "UPDATE groups SET name = ?, key_prefix = ?, key_digest_hex = ?, multiplier_limit = ?,
                weight_multiplier = ?, weight_reliability = ?, weight_first_token = ?,
                weight_throughput = ?, queue_capacity = ?, allow_degrade = ? WHERE id = ?",
        )
        .bind(&group.name)
        .bind(&group.key_prefix)
        .bind(&group.key_digest_hex)
        .bind(group.multiplier_limit.raw())
        .bind(group.weights.multiplier)
        .bind(group.weights.reliability)
        .bind(group.weights.first_token)
        .bind(group.weights.throughput)
        .bind(group.queue_capacity)
        .bind(group.allow_degrade)
        .bind(&group.id)
        .execute(&self.pool)
        .await
        .context("更新分组失败")?;
        Ok(())
    }

    pub async fn delete_group(&self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM groups WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn list_groups(&self) -> Result<Vec<Group>> {
        let rows = sqlx::query("SELECT * FROM groups ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_group).collect()
    }

    // ------------------------------------------------------------------ 账号

    /// 写入账号及其加密凭据。两者必须同一事务，避免出现无凭据的账号。
    pub async fn insert_account(&self, account: &Account, secrets: &AccountSecrets) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO upstream_accounts (id, group_id, name, upstream_type, base_url,
                preferred_protocol, adaptive_protocol, default_priority, calibration,
                multiplier_mode, manual_multiplier, new_api_user_id, new_api_group,
                limit_rpm, limit_tpm, limit_concurrency, allow_private_network, enabled,
                auto_sync, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&account.id)
        .bind(&account.group_id)
        .bind(&account.name)
        .bind(account.upstream_type.as_str())
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
        .bind(account.created_at.unix_timestamp())
        .execute(&mut *tx)
        .await
        .context("写入账号失败")?;

        sqlx::query(
            "INSERT INTO upstream_secrets (account_id, api_key, new_api_token, updated_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&account.id)
        .bind(&secrets.api_key)
        .bind(&secrets.new_api_token)
        .bind(now_unix())
        .execute(&mut *tx)
        .await
        .context("写入账号凭据失败")?;

        tx.commit().await?;
        Ok(())
    }

    /// 更新账号；凭据字段为空表示保持原值不变。
    pub async fn update_account(&self, account: &Account, secrets: &AccountSecrets) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE upstream_accounts SET group_id = ?, name = ?, upstream_type = ?, base_url = ?,
                preferred_protocol = ?, adaptive_protocol = ?, default_priority = ?,
                calibration = ?, multiplier_mode = ?, manual_multiplier = ?, new_api_user_id = ?,
                new_api_group = ?, limit_rpm = ?, limit_tpm = ?, limit_concurrency = ?,
                allow_private_network = ?, enabled = ?, auto_sync = ?
             WHERE id = ?",
        )
        .bind(&account.group_id)
        .bind(&account.name)
        .bind(account.upstream_type.as_str())
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
        .bind(&account.id)
        .execute(&mut *tx)
        .await
        .context("更新账号失败")?;

        // COALESCE 让"留空表示不变"在 SQL 里表达，避免先读后写的竞态。
        sqlx::query(
            "UPDATE upstream_secrets
                SET api_key = COALESCE(?, api_key),
                    new_api_token = COALESCE(?, new_api_token),
                    updated_at = ?
              WHERE account_id = ?",
        )
        .bind(&secrets.api_key)
        .bind(&secrets.new_api_token)
        .bind(now_unix())
        .bind(&account.id)
        .execute(&mut *tx)
        .await
        .context("更新账号凭据失败")?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_account(&self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM upstream_accounts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn list_accounts(&self) -> Result<Vec<Account>> {
        let rows = sqlx::query("SELECT * FROM upstream_accounts ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_account).collect()
    }

    /// 读取并返回账号的加密凭据信封。
    pub async fn account_sealed_key(&self, account_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(
            sqlx::query_scalar("SELECT api_key FROM upstream_secrets WHERE account_id = ?")
                .bind(account_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    // -------------------------------------------------- New API 站点级凭据

    /// 读取某个 Base URL 的站点级 New API 凭据（§6.4）。返回 (用户 ID, 密封令牌)。
    pub async fn new_api_site(&self, base_url: &str) -> Result<Option<(String, Vec<u8>)>> {
        let row = sqlx::query("SELECT user_id, sealed_token FROM new_api_sites WHERE base_url = ?")
            .bind(normalize_site(base_url))
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| Ok((row.try_get("user_id")?, row.try_get("sealed_token")?)))
            .transpose()
    }

    /// 站点级凭据列表（不回吐令牌本身）。
    pub async fn list_new_api_sites(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT base_url, user_id FROM new_api_sites ORDER BY base_url")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("base_url")?, row.try_get("user_id")?)))
            .collect()
    }

    /// 覆盖写入站点级凭据。
    pub async fn upsert_new_api_site(
        &self,
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 删除站点级凭据。
    pub async fn delete_new_api_site(&self, base_url: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM new_api_sites WHERE base_url = ?")
            .bind(normalize_site(base_url))
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    // ---------------------------------------------------------- Responses 状态

    /// 写入一条 Responses 状态链记录。gateway_id 是主键，重复写入覆盖旧值。
    pub async fn upsert_response_state(&self, state: &ResponseStateRow) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO response_states (gateway_id, group_id, logical_model,
                account_id, target_id, endpoint, upstream_id, sealed_body, protocol,
                created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&state.gateway_id)
        .bind(&state.group_id)
        .bind(&state.logical_model)
        .bind(&state.account_id)
        .bind(&state.target_id)
        .bind(&state.endpoint)
        .bind(&state.upstream_id)
        .bind(&state.sealed_body)
        .bind(&state.protocol)
        .bind(state.created_at)
        .bind(state.expires_at)
        .execute(&self.pool)
        .await
        .context("写入 Responses 状态失败")?;
        Ok(())
    }

    /// 按网关 ID 读取一条状态。分组不匹配一律按不存在处理（§26.8）。
    pub async fn response_state(
        &self,
        gateway_id: &str,
        group_id: &str,
    ) -> Result<Option<ResponseStateRow>> {
        let row =
            sqlx::query("SELECT * FROM response_states WHERE gateway_id = ? AND group_id = ?")
                .bind(gateway_id)
                .bind(group_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(Self::response_state_row).transpose()
    }

    fn response_state_row(row: sqlx::sqlite::SqliteRow) -> Result<ResponseStateRow> {
        Ok(ResponseStateRow {
            gateway_id: row.try_get("gateway_id")?,
            group_id: row.try_get("group_id")?,
            logical_model: row.try_get("logical_model")?,
            account_id: row.try_get("account_id")?,
            target_id: row.try_get("target_id")?,
            endpoint: row.try_get("endpoint")?,
            upstream_id: row.try_get("upstream_id")?,
            sealed_body: row.try_get("sealed_body")?,
            protocol: row.try_get("protocol")?,
            created_at: row.try_get("created_at")?,
            expires_at: row.try_get("expires_at")?,
        })
    }

    /// 删除一条 Responses 状态（本地正文、索引与映射一起消失，§15.2）。
    pub async fn delete_response_state(&self, gateway_id: &str, group_id: &str) -> Result<bool> {
        let affected =
            sqlx::query("DELETE FROM response_states WHERE gateway_id = ? AND group_id = ?")
                .bind(gateway_id)
                .bind(group_id)
                .execute(&self.pool)
                .await?
                .rows_affected();
        Ok(affected > 0)
    }

    /// 清理已过期的 Responses 状态。
    pub async fn prune_response_states(&self, older_than: i64) -> Result<u64> {
        let affected = sqlx::query("DELETE FROM response_states WHERE expires_at < ?")
            .bind(older_than)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected)
    }

    /// 当前未过期的 Responses 状态条数，供概览页展示。
    pub async fn count_response_states(&self, now: i64) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM response_states WHERE expires_at >= ?")
                .bind(now)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// 读取 New API 倍率探针的访问令牌信封。
    pub async fn account_sealed_new_api_token(&self, account_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(
            sqlx::query_scalar("SELECT new_api_token FROM upstream_secrets WHERE account_id = ?")
                .bind(account_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten(),
        )
    }

    // ------------------------------------------------------- 模型选择集与别名

    /// 覆盖写入一个账号的模型目录快照（§16.1）。
    ///
    /// 拉取成功后整体替换该账号的目录；`missing` 由调用方按"消失但已选"标好。
    pub async fn replace_account_models(
        &self,
        account_id: &str,
        models: &[AccountModelRow],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM account_models WHERE account_id = ?")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
        for model in models {
            sqlx::query(
                "INSERT INTO account_models (account_id, upstream_model, public_name,
                    selected, missing, discovered_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(account_id)
            .bind(&model.upstream_model)
            .bind(&model.public_name)
            .bind(model.selected)
            .bind(model.missing)
            .bind(model.discovered_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_account_models(&self, account_id: &str) -> Result<Vec<AccountModelRow>> {
        let rows =
            sqlx::query("SELECT * FROM account_models WHERE account_id = ? ORDER BY public_name")
                .bind(account_id)
                .fetch_all(&self.pool)
                .await?;
        rows.iter()
            .map(|row| {
                Ok(AccountModelRow {
                    upstream_model: row.try_get("upstream_model")?,
                    public_name: row.try_get("public_name")?,
                    selected: row.try_get("selected")?,
                    missing: row.try_get("missing")?,
                    discovered_at: row.try_get("discovered_at")?,
                })
            })
            .collect()
    }

    /// 改写一条目录记录的选择状态。选择集驱动调度目标（§16.3），这里只改标记。
    pub async fn set_account_model_selected(
        &self,
        account_id: &str,
        upstream_model: &str,
        selected: bool,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE account_models SET selected = ? WHERE account_id = ? AND upstream_model = ?",
        )
        .bind(selected)
        .bind(account_id)
        .bind(upstream_model)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 同一条记录的选择与消失标记一起改写（勾选与消失标记总是成对调和）。
    pub async fn set_account_model_flags(
        &self,
        account_id: &str,
        upstream_model: &str,
        selected: bool,
        missing: bool,
    ) -> Result<()> {
        sqlx::query("UPDATE account_models SET selected = ?, missing = ? WHERE account_id = ? AND upstream_model = ?")
            .bind(selected)
            .bind(missing)
            .bind(account_id)
            .bind(upstream_model)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 覆盖写入一个账号的模型别名表（§16.4）。
    pub async fn replace_account_aliases(
        &self,
        account_id: &str,
        aliases: &[AccountAliasRow],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM account_aliases WHERE account_id = ?")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
        for alias in aliases {
            sqlx::query(
                "INSERT INTO account_aliases (account_id, upstream_model, public_name)
                 VALUES (?, ?, ?)",
            )
            .bind(account_id)
            .bind(&alias.upstream_model)
            .bind(&alias.public_name)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_account_aliases(&self, account_id: &str) -> Result<Vec<AccountAliasRow>> {
        let rows = sqlx::query(
            "SELECT upstream_model, public_name FROM account_aliases WHERE account_id = ?
             ORDER BY upstream_model",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(AccountAliasRow {
                    upstream_model: row.try_get("upstream_model")?,
                    public_name: row.try_get("public_name")?,
                })
            })
            .collect()
    }

    /// 该账号各逻辑模型最近一段时间内的请求次数（§16.3 的取消勾选警告）。
    pub async fn recent_request_counts_by_account(
        &self,
        account_id: &str,
        since: i64,
    ) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT logical_model, COUNT(*) AS calls FROM request_records
             WHERE account_id = ? AND started_at >= ? GROUP BY logical_model",
        )
        .bind(account_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("logical_model")?, row.try_get("calls")?)))
            .collect()
    }

    /// 记录账号上次完成模型同步的时间。
    pub async fn touch_model_sync(&self, account_id: &str, at: i64) -> Result<()> {
        sqlx::query("UPDATE upstream_accounts SET model_synced_at = ? WHERE id = ?")
            .bind(at)
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // -------------------------------------------------------------- 逻辑模型

    pub async fn insert_logical_model(&self, model: &LogicalModel) -> Result<()> {
        sqlx::query(
            "INSERT INTO logical_models (id, group_id, name, origin, enabled, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&model.id)
        .bind(&model.group_id)
        .bind(&model.name)
        .bind(model.origin.as_str())
        .bind(model.enabled)
        .bind(model.created_at.unix_timestamp())
        .execute(&self.pool)
        .await
        .context("写入逻辑模型失败")?;
        Ok(())
    }

    pub async fn update_logical_model(&self, model: &LogicalModel) -> Result<()> {
        sqlx::query("UPDATE logical_models SET name = ?, enabled = ? WHERE id = ?")
            .bind(&model.name)
            .bind(model.enabled)
            .bind(&model.id)
            .execute(&self.pool)
            .await
            .context("更新逻辑模型失败")?;
        Ok(())
    }

    pub async fn delete_logical_model(&self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM logical_models WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn list_logical_models(&self) -> Result<Vec<LogicalModel>> {
        let rows = sqlx::query("SELECT * FROM logical_models ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_logical_model).collect()
    }

    // -------------------------------------------------------------- 调度目标

    pub async fn insert_target(&self, target: &DispatchTarget) -> Result<()> {
        sqlx::query(
            "INSERT INTO dispatch_targets (id, logical_model_id, account_id, upstream_model,
                priority_override, limit_rpm, limit_tpm, limit_concurrency, enabled, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&target.id)
        .bind(&target.logical_model_id)
        .bind(&target.account_id)
        .bind(&target.upstream_model)
        .bind(target.priority_override)
        .bind(target.limits.rpm)
        .bind(target.limits.tpm)
        .bind(target.limits.max_concurrency)
        .bind(target.enabled)
        .bind(target.created_at.unix_timestamp())
        .execute(&self.pool)
        .await
        .context("写入调度目标失败")?;
        Ok(())
    }

    pub async fn update_target(&self, target: &DispatchTarget) -> Result<()> {
        sqlx::query(
            "UPDATE dispatch_targets SET upstream_model = ?, priority_override = ?, limit_rpm = ?,
                limit_tpm = ?, limit_concurrency = ?, enabled = ?
             WHERE id = ?",
        )
        .bind(&target.upstream_model)
        .bind(target.priority_override)
        .bind(target.limits.rpm)
        .bind(target.limits.tpm)
        .bind(target.limits.max_concurrency)
        .bind(target.enabled)
        .bind(&target.id)
        .execute(&self.pool)
        .await
        .context("更新调度目标失败")?;
        Ok(())
    }

    pub async fn delete_target(&self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM dispatch_targets WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn list_targets(&self) -> Result<Vec<DispatchTarget>> {
        let rows = sqlx::query("SELECT * FROM dispatch_targets ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_target).collect()
    }

    // ------------------------------------------------------------ 请求元数据

    /// 批量写入请求元数据。由后台任务调用，不在热路径上（§19.4）。
    pub async fn insert_request_records(&self, records: &[RequestRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for record in records {
            sqlx::query(
                "INSERT OR REPLACE INTO request_records (request_id, started_at, duration_ms,
                    protocol, streaming, group_id, logical_model, target_id, account_id,
                    upstream_model, request_bytes, upstream_status, http_status, error_code,
                    endpoint, degraded,
                    effective_multiplier, cheapest_multiplier, dearest_multiplier,
                    attempts, queued_ms, sticky_hit,
                    first_token_ms, input_tokens, output_tokens, config_version)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&record.request_id)
            .bind(record.started_at)
            .bind(record.duration_ms)
            .bind(record.protocol.as_str())
            .bind(record.streaming)
            .bind(&record.group_id)
            .bind(&record.logical_model)
            .bind(&record.target_id)
            .bind(&record.account_id)
            .bind(&record.upstream_model)
            .bind(record.request_bytes)
            .bind(record.upstream_status)
            .bind(record.http_status)
            .bind(&record.error_code)
            .bind(&record.endpoint)
            .bind(&record.degraded)
            .bind(record.effective_multiplier.map(Multiplier::raw))
            .bind(record.cheapest_multiplier.map(Multiplier::raw))
            .bind(record.dearest_multiplier.map(Multiplier::raw))
            .bind(record.attempts)
            .bind(record.queued_ms)
            .bind(record.sticky_hit)
            .bind(record.first_token_ms)
            .bind(record.input_tokens)
            .bind(record.output_tokens)
            .bind(record.config_version)
            .execute(&mut *tx)
            .await?;

            // 同一请求重复写入时先清掉旧的尝试明细，避免残留。
            sqlx::query("DELETE FROM request_attempts WHERE request_id = ?")
                .bind(&record.request_id)
                .execute(&mut *tx)
                .await?;
            for attempt in &record.attempts_detail {
                sqlx::query(
                    "INSERT INTO request_attempts (request_id, seq, target_id, account_id,
                        upstream_model, endpoint, started_at, duration_ms, outcome,
                        error_code, counts_against_budget)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&record.request_id)
                .bind(attempt.seq)
                .bind(&attempt.target_id)
                .bind(&attempt.account_id)
                .bind(&attempt.upstream_model)
                .bind(&attempt.endpoint)
                .bind(attempt.started_at)
                .bind(attempt.duration_ms)
                .bind(&attempt.outcome)
                .bind(&attempt.error_code)
                .bind(attempt.counts_against_budget)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// 分页读取请求元数据，按开始时间倒序；尝试明细一并带出（§6.6）。
    pub async fn list_request_records(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<RequestRecord>> {
        // 同一秒内的记录按写入顺序倒序：写入顺序即完成顺序，后完成的在前。
        let rows = sqlx::query(
            "SELECT * FROM request_records ORDER BY started_at DESC, rowid DESC LIMIT ? OFFSET ?",
        )
        .bind(limit.clamp(1, 500))
        .bind(offset.max(0))
        .fetch_all(&self.pool)
        .await?;
        let mut records: Vec<RequestRecord> =
            rows.iter().map(row_to_record).collect::<Result<_>>()?;
        self.attach_attempts(&mut records).await?;
        Ok(records)
    }

    /// 把这一页请求的尝试明细一次性查出来并挂回去（避免逐行 N+1 查询）。
    async fn attach_attempts(&self, records: &mut [RequestRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let placeholders = std::iter::repeat_n("?", records.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT * FROM request_attempts WHERE request_id IN ({placeholders}) ORDER BY request_id, seq"
        );
        // 占位符按记录数生成，值全部走 bind；SQL 本身不含用户输入。
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for record in records.iter() {
            query = query.bind(&record.request_id);
        }
        let rows = query.fetch_all(&self.pool).await?;
        let mut by_request: std::collections::HashMap<String, Vec<AttemptRecord>> =
            std::collections::HashMap::new();
        for row in &rows {
            let request_id: String = row.try_get("request_id")?;
            by_request
                .entry(request_id)
                .or_default()
                .push(AttemptRecord {
                    seq: row.try_get("seq")?,
                    target_id: row.try_get("target_id")?,
                    account_id: row.try_get("account_id")?,
                    upstream_model: row.try_get("upstream_model")?,
                    endpoint: row.try_get("endpoint")?,
                    started_at: row.try_get("started_at")?,
                    duration_ms: row.try_get("duration_ms")?,
                    outcome: row.try_get("outcome")?,
                    error_code: row.try_get("error_code")?,
                    counts_against_budget: row.try_get("counts_against_budget")?,
                });
        }
        for record in records.iter_mut() {
            record.attempts_detail = by_request.remove(&record.request_id).unwrap_or_default();
        }
        Ok(())
    }

    /// 删除早于给定时间的请求元数据，分批执行避免长事务（§24.2）。
    pub async fn prune_request_records(&self, older_than: i64, batch: i64) -> Result<u64> {
        // 先取一批要删的 ID，主记录与尝试明细一起删，避免明细成为孤儿。
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT request_id FROM request_records WHERE started_at < ? LIMIT ?",
        )
        .bind(older_than)
        .bind(batch)
        .fetch_all(&self.pool)
        .await?;
        if ids.is_empty() {
            return Ok(0);
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut tx = self.pool.begin().await?;
        // 占位符按 ID 数生成，值全部走 bind；SQL 本身不含用户输入。
        let mut delete_attempts = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM request_attempts WHERE request_id IN ({placeholders})"
        )));
        let mut delete_records = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM request_records WHERE request_id IN ({placeholders})"
        )));
        for id in &ids {
            delete_attempts = delete_attempts.bind(id);
            delete_records = delete_records.bind(id);
        }
        delete_attempts.execute(&mut *tx).await?;
        let affected = delete_records.execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(affected)
    }

    /// 成本页聚合（§6.8）：按"逻辑模型 + 账号"统计成功的请求数与 Token 用量。
    ///
    /// 只统计成功请求（HTTP 2xx）：失败请求没有产生任何上游消耗，把它算进
    /// 流量占比会扭曲加权倍率。聚合在 SQL 里完成，成本页不拉明细行。
    ///
    /// Token 口径说明：`request_records` 有意不存正文与 usage（§6.6），所以
    /// 这里的 Token 用量以响应侧上报的输出 Token 之和为准——它已经随健康
    /// 评分进入内存，但按元数据表可得的口径只有请求数。Token 列当前恒为 0，
    /// 前端在缺 Token 时退化为按请求数占比展示，绝不虚构 token 数。
    pub async fn cost_usage(&self, since: i64) -> Result<Vec<CostUsageRow>> {
        let rows = sqlx::query(
            "SELECT group_id, logical_model, account_id, COUNT(*) AS requests,
                    SUM(COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)) AS tokens
             FROM request_records
             WHERE started_at >= ? AND http_status >= 200 AND http_status < 300
               AND group_id IS NOT NULL AND logical_model IS NOT NULL
               AND account_id IS NOT NULL
             GROUP BY group_id, logical_model, account_id",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(CostUsageRow {
                    group_id: row.try_get("group_id")?,
                    logical_model: row.try_get("logical_model")?,
                    account_id: row.try_get("account_id")?,
                    requests: row.try_get::<i64, _>("requests")?,
                    // 上游没上报 usage 的历史记录按 0 计，前端在整段区间都没有
                    // Token 时退化为请求数口径，绝不虚构 token 数（§6.8）。
                    tokens: row.try_get::<Option<i64>, _>("tokens")?.unwrap_or(0),
                })
            })
            .collect()
    }

    /// 同一统计区间内的加权倍率素材：该区间每个成功请求的有效倍率样本。
    ///
    /// 加权均倍率必须按请求级样本平均，而不是按账号倍率平均——后者会被
    /// 流量占比扭曲（§6.8 的口径要求）。SQL 聚合给出 (模型, 倍率, 次数)。
    pub async fn cost_multiplier_samples(&self, since: i64) -> Result<Vec<CostSampleRow>> {
        let rows = sqlx::query(
            "SELECT group_id, logical_model, effective_multiplier, COUNT(*) AS requests
             FROM request_records
             WHERE started_at >= ? AND http_status >= 200 AND http_status < 300
               AND group_id IS NOT NULL AND logical_model IS NOT NULL
               AND effective_multiplier IS NOT NULL
             GROUP BY group_id, logical_model, effective_multiplier",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(CostSampleRow {
                    group_id: row.try_get("group_id")?,
                    logical_model: row.try_get("logical_model")?,
                    effective_multiplier: Multiplier::from_raw(
                        row.try_get("effective_multiplier")?,
                    ),
                    requests: row.try_get::<i64, _>("requests")?,
                })
            })
            .collect()
    }

    // ------------------------------------------------------------ 配置备份

    // 备份提取（§23.5）：只读配置表，输出与 `BackupData` 的 JSON 字段对应。
    // 请求正文、Responses 历史、性能记录、粘性绑定与运行日志一律不进备份。

    /// 把一行备份 JSON 里必要的列取出来。
    fn backup_row(row: &sqlx::sqlite::SqliteRow, columns: &[&str]) -> Result<Value> {
        let mut object = serde_json::Map::new();
        for column in columns {
            // 备份列的取值顺序：文本 → 整数 → 布尔 → 可空文本 → NULL。
            // SQLite 里布尔就是整数，所以 i64 分支先命中时按 0/1 写出，
            // 恢复端 bool_field 会把它读回布尔。
            if let Ok(text) = row.try_get::<String, _>(column) {
                object.insert((*column).to_string(), Value::String(text));
            } else if let Ok(number) = row.try_get::<i64, _>(column) {
                object.insert((*column).to_string(), Value::from(number));
            } else if let Ok(flag) = row.try_get::<bool, _>(column) {
                object.insert((*column).to_string(), Value::from(flag));
            } else if let Some(value) = row.try_get::<Option<String>, _>(column)? {
                object.insert((*column).to_string(), Value::String(value));
            } else {
                object.insert((*column).to_string(), Value::Null);
            }
        }
        Ok(Value::Object(object))
    }

    pub async fn backup_groups(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT * FROM groups ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Self::backup_row(
                    row,
                    &[
                        "id",
                        "name",
                        "multiplier_limit",
                        "weight_multiplier",
                        "weight_reliability",
                        "weight_first_token",
                        "weight_throughput",
                        "queue_capacity",
                        "allow_degrade",
                        "created_at",
                    ],
                )
            })
            .collect()
    }

    pub async fn backup_accounts(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT * FROM upstream_accounts ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Self::backup_row(
                    row,
                    &[
                        "id",
                        "group_id",
                        "name",
                        "upstream_type",
                        "base_url",
                        "preferred_protocol",
                        "adaptive_protocol",
                        "default_priority",
                        "calibration",
                        "multiplier_mode",
                        "manual_multiplier",
                        "new_api_user_id",
                        "new_api_group",
                        "limit_rpm",
                        "limit_tpm",
                        "limit_concurrency",
                        "allow_private_network",
                        "enabled",
                        "auto_sync",
                        "created_at",
                    ],
                )
            })
            .collect()
    }

    /// 备份里的上游 Key 是**明文**：备份信封整体已被备份口令加密（§23.5），
    /// 恢复时用本机主密钥重新 seal。
    pub async fn backup_secrets(&self, cipher: &crate::security::Cipher) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT * FROM upstream_secrets")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let account_id: String = row.try_get("account_id")?;
                let api_key: Vec<u8> = row.try_get("api_key")?;
                let plaintext = String::from_utf8_lossy(&cipher.open(&api_key)?).into_owned();
                let token: Option<Vec<u8>> = row.try_get("new_api_token")?;
                let token_text = match token {
                    Some(sealed) => {
                        Some(String::from_utf8_lossy(&cipher.open(&sealed)?).into_owned())
                    }
                    None => None,
                };
                Ok(serde_json::json!({
                    "account_id": account_id,
                    "api_key": plaintext,
                    "new_api_token": token_text,
                }))
            })
            .collect()
    }

    pub async fn backup_logical_models(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT * FROM logical_models ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Self::backup_row(
                    row,
                    &["id", "group_id", "name", "origin", "enabled", "created_at"],
                )
            })
            .collect()
    }

    pub async fn backup_targets(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT * FROM dispatch_targets")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Self::backup_row(
                    row,
                    &[
                        "id",
                        "logical_model_id",
                        "account_id",
                        "upstream_model",
                        "priority_override",
                        "limit_rpm",
                        "limit_tpm",
                        "limit_concurrency",
                        "enabled",
                        "created_at",
                    ],
                )
            })
            .collect()
    }

    pub async fn backup_account_models(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT * FROM account_models")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Self::backup_row(
                    row,
                    &[
                        "account_id",
                        "upstream_model",
                        "public_name",
                        "selected",
                        "missing",
                        "discovered_at",
                    ],
                )
            })
            .collect()
    }

    pub async fn backup_account_aliases(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT * FROM account_aliases")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| Self::backup_row(row, &["account_id", "upstream_model", "public_name"]))
            .collect()
    }

    /// 恢复一份配置快照：单事务内清空现有配置并整体写入（§23.5）。
    ///
    /// 引用校验在调用方完成；这里保证原子性——任何一步失败整体回滚，
    /// 现有配置分毫不动。
    pub async fn import_backup(
        &self,
        data: &crate::security::backup::BackupData,
        cipher: &crate::security::Cipher,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // 子表先删：外键级联会带走账号的选择集、别名与凭据。
        sqlx::query("DELETE FROM dispatch_targets")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM logical_models")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM upstream_accounts")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM groups").execute(&mut *tx).await?;

        for group in &data.groups {
            sqlx::query(
                "INSERT INTO groups (id, name, key_prefix, key_digest_hex, multiplier_limit,
                    weight_multiplier, weight_reliability, weight_first_token, weight_throughput,
                    queue_capacity, allow_degrade, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(str_field(group, "id"))
            .bind(str_field(group, "name"))
            .bind(str_field(group, "key_prefix"))
            .bind(str_field(group, "key_digest_hex"))
            .bind(int_field(group, "multiplier_limit"))
            .bind(int_field(group, "weight_multiplier"))
            .bind(int_field(group, "weight_reliability"))
            .bind(int_field(group, "weight_first_token"))
            .bind(int_field(group, "weight_throughput"))
            .bind(int_field(group, "queue_capacity"))
            .bind(bool_field(group, "allow_degrade"))
            .bind(int_field(group, "created_at"))
            .execute(&mut *tx)
            .await?;
        }
        for account in &data.accounts {
            sqlx::query(
                "INSERT INTO upstream_accounts (id, group_id, name, upstream_type, base_url,
                    preferred_protocol, adaptive_protocol, default_priority, calibration,
                    multiplier_mode, manual_multiplier, new_api_user_id, new_api_group,
                    limit_rpm, limit_tpm, limit_concurrency, allow_private_network, enabled,
                    auto_sync, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(str_field(account, "id"))
            .bind(str_field(account, "group_id"))
            .bind(str_field(account, "name"))
            .bind(str_field(account, "upstream_type"))
            .bind(str_field(account, "base_url"))
            .bind(str_field(account, "preferred_protocol"))
            .bind(bool_field(account, "adaptive_protocol"))
            .bind(int_field(account, "default_priority"))
            .bind(int_field(account, "calibration"))
            .bind(str_field(account, "multiplier_mode"))
            .bind(int_field(account, "manual_multiplier"))
            .bind(str_opt_field(account, "new_api_user_id"))
            .bind(str_opt_field(account, "new_api_group"))
            .bind(int_opt_field(account, "limit_rpm"))
            .bind(int_opt_field(account, "limit_tpm"))
            .bind(int_opt_field(account, "limit_concurrency"))
            .bind(bool_field(account, "allow_private_network"))
            .bind(bool_field(account, "enabled"))
            .bind(bool_field(account, "auto_sync"))
            .bind(int_field(account, "created_at"))
            .execute(&mut *tx)
            .await?;
        }
        for secret in &data.secrets {
            // 本机主密钥重新加密：备份里的明文 Key 只在内存停留一瞬（§23.5）。
            let api_key = str_field(secret, "api_key");
            let sealed_key = cipher
                .seal(api_key.as_bytes())
                .context("备份恢复时重新加密 Key 失败")?;
            let sealed_token = match str_opt_field(secret, "new_api_token") {
                Some(token) => Some(
                    cipher
                        .seal(token.as_bytes())
                        .context("备份恢复时重新加密令牌失败")?,
                ),
                None => None,
            };
            sqlx::query(
                "INSERT INTO upstream_secrets (account_id, api_key, new_api_token, updated_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(str_field(secret, "account_id"))
            .bind(&sealed_key)
            .bind(&sealed_token)
            .bind(crate::storage::now_unix())
            .execute(&mut *tx)
            .await?;
        }
        for model in &data.logical_models {
            sqlx::query(
                "INSERT INTO logical_models (id, group_id, name, origin, enabled, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(str_field(model, "id"))
            .bind(str_field(model, "group_id"))
            .bind(str_field(model, "name"))
            .bind(str_field(model, "origin"))
            .bind(bool_field(model, "enabled"))
            .bind(int_field(model, "created_at"))
            .execute(&mut *tx)
            .await?;
        }
        for target in &data.dispatch_targets {
            sqlx::query(
                "INSERT INTO dispatch_targets (id, logical_model_id, account_id, upstream_model,
                    priority_override, limit_rpm, limit_tpm, limit_concurrency, enabled, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(str_field(target, "id"))
            .bind(str_field(target, "logical_model_id"))
            .bind(str_field(target, "account_id"))
            .bind(str_field(target, "upstream_model"))
            .bind(int_opt_field(target, "priority_override"))
            .bind(int_opt_field(target, "limit_rpm"))
            .bind(int_opt_field(target, "limit_tpm"))
            .bind(int_opt_field(target, "limit_concurrency"))
            .bind(bool_field(target, "enabled"))
            .bind(int_field(target, "created_at"))
            .execute(&mut *tx)
            .await?;
        }
        for row in &data.account_models {
            sqlx::query(
                "INSERT INTO account_models (account_id, upstream_model, public_name,
                    selected, missing, discovered_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(str_field(row, "account_id"))
            .bind(str_field(row, "upstream_model"))
            .bind(str_field(row, "public_name"))
            .bind(bool_field(row, "selected"))
            .bind(bool_field(row, "missing"))
            .bind(int_field(row, "discovered_at"))
            .execute(&mut *tx)
            .await?;
        }
        for row in &data.account_aliases {
            sqlx::query(
                "INSERT INTO account_aliases (account_id, upstream_model, public_name)
                 VALUES (?, ?, ?)",
            )
            .bind(str_field(row, "account_id"))
            .bind(str_field(row, "upstream_model"))
            .bind(str_field(row, "public_name"))
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    // ------------------------------------------------------------ 校准记录

    /// 写一条校准对账记录（§6.8）。
    pub async fn insert_calibration_record(&self, record: &CalibrationRecord) -> Result<()> {
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
        .execute(&self.pool)
        .await
        .context("写入校准记录失败")?;
        Ok(())
    }

    /// 一个账号最近的对账记录，新的在前。
    pub async fn list_calibration_records(
        &self,
        account_id: &str,
        limit: i64,
    ) -> Result<Vec<CalibrationRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM calibration_records WHERE account_id = ?
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(account_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(CalibrationRecord {
                    id: row.try_get("id")?,
                    account_id: row.try_get("account_id")?,
                    logical_model: row.try_get("logical_model")?,
                    period_start: row.try_get("period_start")?,
                    period_end: row.try_get("period_end")?,
                    gateway_requests: row.try_get("gateway_requests")?,
                    reported: row.try_get("reported")?,
                    calibration: row.try_get("calibration")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }

    // ------------------------------------------------ 动态状态与快照持久化

    /// 覆盖写入一个账号的倍率状态（§11.4）。
    pub async fn upsert_multiplier_snapshot(&self, snapshot: &MultiplierSnapshotRow) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO multiplier_snapshots (account_id, multiplier, source, status,
                observed_at, refreshed_at, stale_since, last_error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&snapshot.account_id)
        .bind(snapshot.multiplier.raw())
        .bind(snapshot.source.as_str())
        .bind(&snapshot.status)
        .bind(snapshot.observed_at)
        .bind(snapshot.refreshed_at)
        .bind(snapshot.stale_since)
        .bind(&snapshot.last_error)
        .execute(&self.pool)
        .await
        .context("写入倍率状态失败")?;
        Ok(())
    }

    pub async fn list_multiplier_snapshots(&self) -> Result<Vec<MultiplierSnapshotRow>> {
        let rows = sqlx::query("SELECT * FROM multiplier_snapshots")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let source: String = row.try_get("source")?;
                Ok(MultiplierSnapshotRow {
                    account_id: row.try_get("account_id")?,
                    multiplier: Multiplier::from_raw(row.try_get("multiplier")?),
                    source: MultiplierMode::parse(&source)
                        .with_context(|| format!("数据库中的倍率来源无法识别：{source}"))?,
                    status: row.try_get("status")?,
                    observed_at: row.try_get("observed_at")?,
                    refreshed_at: row.try_get("refreshed_at")?,
                    stale_since: row.try_get("stale_since")?,
                    last_error: row.try_get("last_error")?,
                })
            })
            .collect()
    }

    /// 批量覆盖粘性绑定。由 60 秒快照任务调用，不在热路径上（§19.4）。
    pub async fn save_sticky_bindings(&self, rows: &[StickyBindingRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for row in rows {
            sqlx::query(
                "INSERT OR REPLACE INTO sticky_bindings (sticky_key, group_id, logical_model,
                    target_id, bound_at, last_used_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&row.sticky_key)
            .bind(&row.group_id)
            .bind(&row.logical_model)
            .bind(&row.target_id)
            .bind(row.bound_at)
            .bind(row.last_used_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 读取尚未过期的粘性绑定。
    pub async fn load_sticky_bindings(&self, not_before: i64) -> Result<Vec<StickyBindingRow>> {
        let rows = sqlx::query("SELECT * FROM sticky_bindings WHERE last_used_at >= ?")
            .bind(not_before)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(StickyBindingRow {
                    sticky_key: row.try_get("sticky_key")?,
                    group_id: row.try_get("group_id")?,
                    logical_model: row.try_get("logical_model")?,
                    target_id: row.try_get("target_id")?,
                    bound_at: row.try_get("bound_at")?,
                    last_used_at: row.try_get("last_used_at")?,
                })
            })
            .collect()
    }

    /// 删除已过期或指向已消失目标的绑定。
    pub async fn prune_sticky_bindings(&self, older_than: i64) -> Result<u64> {
        let affected = sqlx::query(
            "DELETE FROM sticky_bindings
              WHERE last_used_at < ?
                 OR target_id NOT IN (SELECT id FROM dispatch_targets)",
        )
        .bind(older_than)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected)
    }

    /// 批量覆盖性能 EWMA 快照。
    pub async fn save_perf_snapshots(&self, rows: &[PerfSnapshotRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for row in rows {
            sqlx::query(
                "INSERT OR REPLACE INTO target_perf_snapshot (target_id, protocol, streaming,
                    samples, success_rate, first_token_ms, total_ms, output_tps, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&row.target_id)
            .bind(row.protocol.as_str())
            .bind(row.streaming)
            .bind(row.samples)
            .bind(row.success_rate)
            .bind(row.first_token_ms)
            .bind(row.total_ms)
            .bind(row.output_tps)
            .bind(row.updated_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 读取仍然新鲜的性能快照。超过 24 小时的快照由调用方通过 `not_before` 丢弃。
    pub async fn load_perf_snapshots(&self, not_before: i64) -> Result<Vec<PerfSnapshotRow>> {
        let rows = sqlx::query(
            "SELECT * FROM target_perf_snapshot
              WHERE updated_at >= ?
                AND target_id IN (SELECT id FROM dispatch_targets)",
        )
        .bind(not_before)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let protocol: String = row.try_get("protocol")?;
                Ok(PerfSnapshotRow {
                    target_id: row.try_get("target_id")?,
                    protocol: Protocol::parse(&protocol)
                        .with_context(|| format!("数据库中的协议无法识别：{protocol}"))?,
                    streaming: row.try_get("streaming")?,
                    samples: row.try_get("samples")?,
                    success_rate: row.try_get("success_rate")?,
                    first_token_ms: row.try_get("first_token_ms")?,
                    total_ms: row.try_get("total_ms")?,
                    output_tps: row.try_get("output_tps")?,
                    updated_at: row.try_get("updated_at")?,
                })
            })
            .collect()
    }

    /// 丢弃过期或指向已消失目标的性能快照。
    pub async fn prune_perf_snapshots(&self, older_than: i64) -> Result<u64> {
        let affected = sqlx::query(
            "DELETE FROM target_perf_snapshot
              WHERE updated_at < ?
                 OR target_id NOT IN (SELECT id FROM dispatch_targets)",
        )
        .bind(older_than)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected)
    }

    /// 记录一条管理操作审计。
    pub async fn record_audit(
        &self,
        actor: &str,
        action: &str,
        object: &str,
        result: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO admin_audit_log (id, occurred_at, actor, action, object, result)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(new_id("aud"))
        .bind(now_unix())
        .bind(actor)
        .bind(action)
        .bind(object)
        .bind(result)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// 一条不含正文的请求元数据（§24.1）。
#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub request_id: String,
    pub started_at: i64,
    pub duration_ms: i64,
    pub protocol: Protocol,
    pub streaming: bool,
    pub group_id: Option<String>,
    pub logical_model: Option<String>,
    pub target_id: Option<String>,
    pub account_id: Option<String>,
    pub upstream_model: Option<String>,
    pub request_bytes: i64,
    pub upstream_status: Option<i64>,
    pub http_status: i64,
    pub error_code: Option<String>,
    /// 实际使用的上游端点，跨协议时与下游入口不同（§14.3）。
    pub endpoint: Option<String>,
    /// 为完成本次请求丢弃的白名单能力，逗号分隔；无降级时为空（§14.8）。
    pub degraded: Option<String>,
    /// 本次实际使用的有效倍率，以及同一逻辑模型内当时的倍率区间（§11.6）。
    pub effective_multiplier: Option<Multiplier>,
    pub cheapest_multiplier: Option<Multiplier>,
    pub dearest_multiplier: Option<Multiplier>,
    pub attempts: i64,
    pub queued_ms: i64,
    pub sticky_hit: bool,
    /// 首个语义块到达的时间（流式才有），毫秒（§6.6）。
    pub first_token_ms: Option<i64>,
    /// 本次请求的输入/输出 Token；上游没上报时为空，绝不估算（§6.8）。
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    /// 产生这条记录时的配置快照版本（§6.6）。
    pub config_version: Option<i64>,
    /// 每次上游尝试的明细（§6.6）。写入时与主记录同一事务。
    pub attempts_detail: Vec<AttemptRecord>,
}

/// 一次上游尝试的明细（§6.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    pub seq: i64,
    pub target_id: Option<String>,
    pub account_id: Option<String>,
    pub upstream_model: Option<String>,
    pub endpoint: Option<String>,
    pub started_at: i64,
    pub duration_ms: i64,
    /// `ok` / `failed` / `missing_endpoint`。
    pub outcome: String,
    pub error_code: Option<String>,
    /// 这次失败是否计入尝试预算：廉价的连接失败不计（§13.1）。
    pub counts_against_budget: bool,
}

/// 成本页的一行流量聚合：某分组某逻辑模型在某账号上的成功请求数（§6.8）。
#[derive(Debug, Clone)]
pub struct CostUsageRow {
    pub group_id: String,
    pub logical_model: String,
    pub account_id: String,
    pub requests: i64,
    /// request_records 不存 usage，恒为 0；留作阶段 5C 的分钟聚合接入口。
    pub tokens: i64,
}

/// 成本页的一行倍率样本：某分组某逻辑模型在某个有效倍率上的请求次数。
/// 加权均倍率按请求级样本平均，而不是按账号倍率平均（§6.8 口径）。
#[derive(Debug, Clone)]
pub struct CostSampleRow {
    pub group_id: String,
    pub logical_model: String,
    pub effective_multiplier: Multiplier,
    pub requests: i64,
}

/// 校准助手的一条对账记录（§6.8）。
#[derive(Debug, Clone)]
pub struct CalibrationRecord {
    pub id: String,
    pub account_id: String,
    pub logical_model: String,
    pub period_start: i64,
    pub period_end: i64,
    /// 对账区间内本账号在该模型上的网关侧请求数。
    pub gateway_requests: i64,
    /// 站点后台报的该模型扣费倍率（字符串十进制）。
    pub reported: String,
    /// 反算出的校准系数（字符串十进制）。
    pub calibration: String,
    pub created_at: i64,
}

/// 一条持久化的账号倍率状态（§11.4）。
#[derive(Debug, Clone)]
pub struct MultiplierSnapshotRow {
    pub account_id: String,
    /// 最后已知的**上游**倍率，尚未乘校准系数。
    pub multiplier: Multiplier,
    pub source: MultiplierMode,
    pub status: String,
    pub observed_at: Option<i64>,
    pub refreshed_at: i64,
    pub stale_since: Option<i64>,
    pub last_error: Option<String>,
}

/// 一条持久化的粘性绑定（§10.2）。
#[derive(Debug, Clone)]
pub struct StickyBindingRow {
    pub sticky_key: String,
    pub group_id: String,
    pub logical_model: String,
    pub target_id: String,
    pub bound_at: i64,
    pub last_used_at: i64,
}

/// 一条持久化的性能 EWMA 快照（§9.3）。
#[derive(Debug, Clone)]
pub struct PerfSnapshotRow {
    pub target_id: String,
    pub protocol: Protocol,
    pub streaming: bool,
    pub samples: i64,
    pub success_rate: f64,
    pub first_token_ms: f64,
    pub total_ms: f64,
    pub output_tps: f64,
    pub updated_at: i64,
}

/// 一条 Responses 状态链记录（§15.1、§15.2）。
#[derive(Debug, Clone)]
pub struct ResponseStateRow {
    pub gateway_id: String,
    pub group_id: String,
    pub logical_model: String,
    pub account_id: Option<String>,
    pub target_id: Option<String>,
    pub endpoint: Option<String>,
    pub upstream_id: Option<String>,
    /// 加密的可重放正文。`None` 即 store:false 或保留期为 0 的最小定位模式。
    pub sealed_body: Option<Vec<u8>>,
    /// 生成该响应的入口协议。
    pub protocol: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// 账号模型目录里的一条记录：上游真名、对外名与选择状态（§16.2）。
#[derive(Debug, Clone)]
pub struct AccountModelRow {
    /// 上游真名（规范化后，保留大小写）。
    pub upstream_model: String,
    /// 别名应用后的对外名。
    pub public_name: String,
    pub selected: bool,
    pub missing: bool,
    pub discovered_at: i64,
}

/// 账号级模型别名：上游真名 → 对外名（§16.4）。
#[derive(Debug, Clone)]
pub struct AccountAliasRow {
    pub upstream_model: String,
    pub public_name: String,
}

/// 备份 JSON 对象里的字符串字段；缺失按空串处理（恢复前的校验应已兜底）。
fn str_field(object: &serde_json::Value, key: &str) -> String {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn str_opt_field(object: &serde_json::Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn int_field(object: &serde_json::Value, key: &str) -> i64 {
    object
        .get(key)
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default()
}

fn int_opt_field(object: &serde_json::Value, key: &str) -> Option<i64> {
    object.get(key).and_then(serde_json::Value::as_i64)
}

fn bool_field(object: &serde_json::Value, key: &str) -> bool {
    match object.get(key) {
        Some(value) => value
            .as_bool()
            .unwrap_or_else(|| value.as_i64().unwrap_or(0) != 0),
        None => false,
    }
}

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
        },
        queue_capacity: row.try_get::<i64, _>("queue_capacity")? as u32,
        allow_degrade: row.try_get("allow_degrade")?,
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
    let upstream_type: String = row.try_get("upstream_type")?;
    let protocol: String = row.try_get("preferred_protocol")?;
    let mode: String = row.try_get("multiplier_mode")?;
    Ok(Account {
        id: row.try_get("id")?,
        group_id: row.try_get("group_id")?,
        name: row.try_get("name")?,
        upstream_type: UpstreamType::parse(&upstream_type)
            .with_context(|| format!("数据库中的上游类型无法识别：{upstream_type}"))?,
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
        model_synced_at: row.try_get("model_synced_at")?,
        created_at: to_time(row.try_get("created_at")?),
    })
}

fn row_to_logical_model(row: &sqlx::sqlite::SqliteRow) -> Result<LogicalModel> {
    let origin: String = row.try_get("origin")?;
    Ok(LogicalModel {
        id: row.try_get("id")?,
        group_id: row.try_get("group_id")?,
        name: row.try_get("name")?,
        origin: ModelOrigin::parse(&origin)
            .with_context(|| format!("数据库中的逻辑模型来源无法识别：{origin}"))?,
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
        priority_override: row
            .try_get::<Option<i64>, _>("priority_override")?
            .map(|v| v as i32),
        limits: row_to_limits(row)?,
        enabled: row.try_get("enabled")?,
        created_at: to_time(row.try_get("created_at")?),
    })
}

/// 站点级凭据的匹配键：去掉首尾空白与结尾斜杠，大小写不敏感由调用方保证。
fn normalize_site(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn row_to_record(row: &sqlx::sqlite::SqliteRow) -> Result<RequestRecord> {
    let protocol: String = row.try_get("protocol")?;
    Ok(RequestRecord {
        request_id: row.try_get("request_id")?,
        started_at: row.try_get("started_at")?,
        duration_ms: row.try_get("duration_ms")?,
        protocol: Protocol::parse(&protocol)
            .with_context(|| format!("数据库中的协议无法识别：{protocol}"))?,
        streaming: row.try_get("streaming")?,
        group_id: row.try_get("group_id")?,
        logical_model: row.try_get("logical_model")?,
        target_id: row.try_get("target_id")?,
        account_id: row.try_get("account_id")?,
        upstream_model: row.try_get("upstream_model")?,
        request_bytes: row.try_get("request_bytes")?,
        upstream_status: row.try_get("upstream_status")?,
        http_status: row.try_get("http_status")?,
        error_code: row.try_get("error_code")?,
        endpoint: row.try_get("endpoint")?,
        degraded: row.try_get("degraded")?,
        effective_multiplier: row
            .try_get::<Option<i64>, _>("effective_multiplier")?
            .map(Multiplier::from_raw),
        cheapest_multiplier: row
            .try_get::<Option<i64>, _>("cheapest_multiplier")?
            .map(Multiplier::from_raw),
        dearest_multiplier: row
            .try_get::<Option<i64>, _>("dearest_multiplier")?
            .map(Multiplier::from_raw),
        attempts: row.try_get("attempts")?,
        queued_ms: row.try_get("queued_ms")?,
        sticky_hit: row.try_get("sticky_hit")?,
        first_token_ms: row.try_get("first_token_ms")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        config_version: row.try_get("config_version")?,
        // 尝试明细由 `attach_attempts` 单独填充。
        attempts_detail: Vec::new(),
    })
}

/// 生成领域对象 ID，供上层构造新记录时使用。
pub mod ids {
    pub fn group() -> String {
        super::new_id("grp")
    }
    pub fn account() -> String {
        super::new_id("acc")
    }
    pub fn logical_model() -> String {
        super::new_id("lm")
    }
    pub fn target() -> String {
        super::new_id("tgt")
    }
    pub fn calibration() -> String {
        super::new_id("cal")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ModelOrigin, SchedulingWeights};

    async fn store() -> Store {
        Store::new(crate::storage::open_in_memory().await.unwrap())
    }

    fn sample_group() -> Group {
        Group {
            id: ids::group(),
            name: "主力".into(),
            key_prefix: "akh-abcdefg".into(),
            key_digest_hex: "digest-1".into(),
            multiplier_limit: Multiplier::ONE,
            weights: SchedulingWeights::default(),
            queue_capacity: 100,
            allow_degrade: true,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    fn sample_account(group_id: &str) -> Account {
        Account {
            id: ids::account(),
            group_id: group_id.into(),
            name: "账号A".into(),
            upstream_type: UpstreamType::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            preferred_protocol: Protocol::AnthropicMessages,
            adaptive_protocol: true,
            default_priority: 50,
            calibration: Multiplier::ONE,
            multiplier_mode: MultiplierMode::Manual,
            manual_multiplier: Multiplier::parse("0.5").unwrap(),
            new_api_user_id: None,
            new_api_group: None,
            limits: Limits::default(),
            allow_private_network: false,
            enabled: true,
            auto_sync: false,
            model_synced_at: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    fn sealed(api_key: &[u8]) -> AccountSecrets {
        AccountSecrets::new(api_key.to_vec(), None)
    }

    #[tokio::test]
    async fn first_admin_can_only_be_created_once() {
        let store = store().await;
        assert!(store.needs_setup().await.unwrap());
        store.create_admin("admin", "hash").await.unwrap();
        assert!(!store.needs_setup().await.unwrap());
        assert!(store.create_admin("other", "hash").await.is_err());
    }

    #[tokio::test]
    async fn group_roundtrips_with_fixed_point_multiplier() {
        let store = store().await;
        let mut group = sample_group();
        group.multiplier_limit = Multiplier::parse("0.823456").unwrap();
        store.insert_group(&group).await.unwrap();

        let loaded = store.list_groups().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].multiplier_limit, group.multiplier_limit);
        assert_eq!(loaded[0].weights, SchedulingWeights::default());
    }

    #[tokio::test]
    async fn account_and_secret_are_written_together() {
        let store = store().await;
        let group = sample_group();
        store.insert_group(&group).await.unwrap();
        let account = sample_account(&group.id);
        store
            .insert_account(&account, &sealed(b"sealed"))
            .await
            .unwrap();

        assert_eq!(
            store
                .account_sealed_key(&account.id)
                .await
                .unwrap()
                .unwrap(),
            b"sealed"
        );
        let loaded = store.list_accounts().await.unwrap();
        assert_eq!(loaded[0].preferred_protocol, Protocol::AnthropicMessages);
        assert_eq!(
            loaded[0].configured_effective_multiplier(),
            Multiplier::parse("0.5").unwrap()
        );
    }

    #[tokio::test]
    async fn an_empty_credential_patch_keeps_the_existing_key() {
        let store = store().await;
        let group = sample_group();
        store.insert_group(&group).await.unwrap();
        let mut account = sample_account(&group.id);
        store
            .insert_account(&account, &sealed(b"original"))
            .await
            .unwrap();

        // 只改名字，不碰凭据：后台不提供读回完整 Key 的接口，所以"没填"
        // 必须原样保留，而不是把 Key 清空。
        account.name = "账号A改名".into();
        store
            .update_account(&account, &AccountSecrets::default())
            .await
            .unwrap();
        assert_eq!(
            store
                .account_sealed_key(&account.id)
                .await
                .unwrap()
                .unwrap(),
            b"original"
        );

        store
            .update_account(&account, &sealed(b"rotated"))
            .await
            .unwrap();
        assert_eq!(
            store
                .account_sealed_key(&account.id)
                .await
                .unwrap()
                .unwrap(),
            b"rotated"
        );
    }

    #[tokio::test]
    async fn deleting_a_group_cascades_to_accounts_and_models() {
        let store = store().await;
        let group = sample_group();
        store.insert_group(&group).await.unwrap();
        let account = sample_account(&group.id);
        store
            .insert_account(&account, &sealed(b"sealed"))
            .await
            .unwrap();

        let model = LogicalModel {
            id: ids::logical_model(),
            group_id: group.id.clone(),
            name: "claude-sonnet-4-5".into(),
            origin: ModelOrigin::Manual,
            enabled: true,
            created_at: OffsetDateTime::now_utc(),
        };
        store.insert_logical_model(&model).await.unwrap();
        store
            .insert_target(&DispatchTarget {
                id: ids::target(),
                logical_model_id: model.id.clone(),
                account_id: account.id.clone(),
                upstream_model: "claude-sonnet-4-5-20250929".into(),
                priority_override: None,
                limits: Limits::default(),
                enabled: true,
                created_at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();

        assert!(store.delete_group(&group.id).await.unwrap());
        assert!(store.list_accounts().await.unwrap().is_empty());
        assert!(store.list_logical_models().await.unwrap().is_empty());
        assert!(store.list_targets().await.unwrap().is_empty());
        assert!(
            store
                .account_sealed_key(&account.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn request_records_are_batched_and_pruned() {
        let store = store().await;
        let records: Vec<_> = (0..3)
            .map(|i| RequestRecord {
                request_id: format!("req_{i}"),
                started_at: 1_000 + i,
                duration_ms: 12,
                protocol: Protocol::OpenAiChat,
                streaming: false,
                group_id: None,
                logical_model: Some("glm-4.6".into()),
                target_id: None,
                account_id: None,
                upstream_model: None,
                request_bytes: 512,
                upstream_status: Some(200),
                http_status: 200,
                error_code: None,
                endpoint: Some("chat_completions".into()),
                degraded: None,
                effective_multiplier: Some(Multiplier::parse("0.5").unwrap()),
                cheapest_multiplier: Some(Multiplier::parse("0.5").unwrap()),
                dearest_multiplier: Some(Multiplier::ONE),
                attempts: 1,
                first_token_ms: None,
                input_tokens: None,
                output_tokens: None,
                config_version: None,
                attempts_detail: Vec::new(),
                queued_ms: 0,
                sticky_hit: false,
            })
            .collect();
        store.insert_request_records(&records).await.unwrap();

        let listed = store.list_request_records(10, 0).await.unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].request_id, "req_2", "应当按开始时间倒序");
        assert_eq!(
            listed[0].dearest_multiplier,
            Some(Multiplier::ONE),
            "成本基准倍率只能在请求发生时记录，事后无法重算"
        );

        assert_eq!(store.prune_request_records(1_002, 100).await.unwrap(), 2);
        assert_eq!(store.list_request_records(10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stale_snapshots_are_not_loaded_back_after_a_restart() {
        let store = store().await;
        let group = sample_group();
        store.insert_group(&group).await.unwrap();
        let account = sample_account(&group.id);
        store
            .insert_account(&account, &sealed(b"sealed"))
            .await
            .unwrap();
        let model = LogicalModel {
            id: ids::logical_model(),
            group_id: group.id.clone(),
            name: "glm-4.6".into(),
            origin: ModelOrigin::Manual,
            enabled: true,
            created_at: OffsetDateTime::now_utc(),
        };
        store.insert_logical_model(&model).await.unwrap();
        let target = DispatchTarget {
            id: ids::target(),
            logical_model_id: model.id.clone(),
            account_id: account.id.clone(),
            upstream_model: "glm-4.6".into(),
            priority_override: None,
            limits: Limits::default(),
            enabled: true,
            created_at: OffsetDateTime::now_utc(),
        };
        store.insert_target(&target).await.unwrap();

        let fresh = PerfSnapshotRow {
            target_id: target.id.clone(),
            protocol: Protocol::OpenAiChat,
            streaming: true,
            samples: 30,
            success_rate: 0.98,
            first_token_ms: 900.0,
            total_ms: 4_000.0,
            output_tps: 42.0,
            updated_at: 10_000,
        };
        let stale = PerfSnapshotRow {
            streaming: false,
            updated_at: 100,
            ..fresh.clone()
        };
        store.save_perf_snapshots(&[fresh, stale]).await.unwrap();

        // 超过保鲜期的快照宁可丢弃也不能拿来打分（§20.1）。
        let loaded = store.load_perf_snapshots(1_000).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].streaming);
        assert_eq!(store.prune_perf_snapshots(1_000).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn sticky_bindings_survive_a_reload_but_expire_and_follow_deletions() {
        let store = store().await;
        let group = sample_group();
        store.insert_group(&group).await.unwrap();
        let account = sample_account(&group.id);
        store
            .insert_account(&account, &sealed(b"sealed"))
            .await
            .unwrap();
        let model = LogicalModel {
            id: ids::logical_model(),
            group_id: group.id.clone(),
            name: "glm-4.6".into(),
            origin: ModelOrigin::Manual,
            enabled: true,
            created_at: OffsetDateTime::now_utc(),
        };
        store.insert_logical_model(&model).await.unwrap();
        let target = DispatchTarget {
            id: ids::target(),
            logical_model_id: model.id.clone(),
            account_id: account.id.clone(),
            upstream_model: "glm-4.6".into(),
            priority_override: None,
            limits: Limits::default(),
            enabled: true,
            created_at: OffsetDateTime::now_utc(),
        };
        store.insert_target(&target).await.unwrap();

        store
            .save_sticky_bindings(&[
                StickyBindingRow {
                    sticky_key: "prefix-a".into(),
                    group_id: group.id.clone(),
                    logical_model: "glm-4.6".into(),
                    target_id: target.id.clone(),
                    bound_at: 9_000,
                    last_used_at: 10_000,
                },
                StickyBindingRow {
                    sticky_key: "prefix-old".into(),
                    group_id: group.id.clone(),
                    logical_model: "glm-4.6".into(),
                    target_id: target.id.clone(),
                    bound_at: 10,
                    last_used_at: 20,
                },
            ])
            .await
            .unwrap();

        let loaded = store.load_sticky_bindings(1_000).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].sticky_key, "prefix-a");

        // 目标被删除后，指向它的绑定必须一起消失，否则重启会复活一个死目标。
        store.delete_target(&target.id).await.unwrap();
        assert_eq!(store.prune_sticky_bindings(0).await.unwrap(), 2);
    }
}
