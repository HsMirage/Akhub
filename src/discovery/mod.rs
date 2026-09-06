//! 模型发现与选择集（§16）。
//!
//! 账号目录（`account_models`）是"上游列表 + 别名 + 管理员勾选"三者的合成
//! 快照；勾选驱动调度目标的自动生成与清理（§16.3）。任何一次拉取失败都
//! 不会改动已有目录（§16.1）。

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;

use crate::app::SharedState;
use crate::domain::{Account, DispatchTarget, Limits, LogicalModel, ModelOrigin};
use crate::storage::now_unix;
use crate::storage::store::{AccountAliasRow, AccountModelRow, ids};

/// 取消勾选前的二次确认信息（§16.3）：该模型最近 24 小时的调用次数。
#[derive(Debug, Clone)]
pub struct UnselectWarning {
    pub public_name: String,
    pub calls: i64,
}

/// 一次批量勾选净增删的目标数。
#[derive(Debug, Default)]
pub struct SelectionOutcome {
    pub created: usize,
    pub removed: usize,
}

/// 批量勾选的处理结果：存在有流量的移除且未确认时，返回警告并不改动任何数据。
#[derive(Debug)]
pub enum Selection {
    Applied(SelectionOutcome),
    NeedsConfirm(Vec<UnselectWarning>),
}

/// 勾选对话框里的一行（§16.2）。
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub upstream_model: String,
    pub public_name: String,
    pub selected: bool,
    pub missing: bool,
    /// 本次拉取新出现的模型，前端用"只看新增"过滤。
    pub is_new: bool,
}

/// "最近有流量"的判定窗口（§16.3）。
const TRAFFIC_WINDOW_SECS: i64 = 24 * 3600;

// -------------------------------------------------------------- 拉取与合并

/// 拉取上游模型列表并合并进账号目录（§16.1 第 1–5 步、§16.5）。
///
/// 成功后整体替换目录快照并返回对话框数据；请求失败时这里直接返回错误，
/// 调用方保留原目录与选择集，只把错误展示出来。
pub async fn refresh_catalog(state: &SharedState, account: &Account) -> Result<Vec<CatalogEntry>> {
    let sealed = state
        .store
        .account_sealed_key(&account.id)
        .await?
        .context("账号还没有保存 API Key")?;
    let key = state.cipher.open(&sealed)?;
    let api_key = String::from_utf8_lossy(&key).into_owned();
    let upstream_names = crate::upstream::fetch_model_list(
        state.upstream.http(),
        &account.base_url,
        account.preferred_protocol,
        &api_key,
    )
    .await
    .context("上游模型列表请求失败")?;
    // 0 个有效模型与请求失败同等对待：保留原列表和选择集，只报错（§16.1）。
    // 否则上游一次抽风返回空列表就会把整份目录和选择集清空。
    if upstream_names.is_empty() {
        bail!("上游返回 0 个有效模型，已保留原目录");
    }

    // 别名在勾选对话框之前应用（§16.4），对话框与选择集里出现的都是对外名。
    let aliases: HashMap<String, String> = state
        .store
        .list_account_aliases(&account.id)
        .await?
        .into_iter()
        .map(|a| (a.upstream_model, a.public_name))
        .collect();

    let previous = state.store.list_account_models(&account.id).await?;
    let previous_by_name: HashMap<&str, &AccountModelRow> = previous
        .iter()
        .map(|row| (row.upstream_model.as_str(), row))
        .collect();

    // 多账号行为（§16.2）：全新账号的列表里，分组内已存在的同名逻辑模型
    // 默认勾选——你已经表达过"我要这个模型"。
    let group_models: HashSet<String> = if previous.is_empty() {
        state
            .store
            .list_logical_models()
            .await?
            .iter()
            .filter(|m| m.group_id == account.group_id)
            .map(|m| m.name.clone())
            .collect()
    } else {
        HashSet::new()
    };

    let now = now_unix();
    let upstream_set: HashSet<&str> = upstream_names.iter().map(String::as_str).collect();

    let mut rows = Vec::with_capacity(upstream_names.len());
    let mut entries = Vec::with_capacity(upstream_names.len());
    for name in &upstream_names {
        let public_name = aliases.get(name).cloned().unwrap_or_else(|| name.clone());
        let known = previous_by_name.get(name.as_str());
        let selected = match known {
            // 已勾选的保持勾选，曾明确取消的保持不勾（§16.2）。
            Some(prev) => prev.selected,
            None => group_models.contains(&public_name),
        };
        rows.push(AccountModelRow {
            upstream_model: name.clone(),
            public_name: public_name.clone(),
            selected,
            missing: false,
            discovered_at: known.map(|p| p.discovered_at).unwrap_or(now),
        });
        entries.push(CatalogEntry {
            upstream_model: name.clone(),
            public_name,
            selected,
            missing: false,
            is_new: known.is_none(),
        });
    }

    // 消失的模型（§16.5）：已选的保留记录并标记 missing；未选的移出目录。
    for prev in &previous {
        if upstream_set.contains(prev.upstream_model.as_str()) || !prev.selected {
            continue;
        }
        rows.push(AccountModelRow {
            upstream_model: prev.upstream_model.clone(),
            public_name: prev.public_name.clone(),
            selected: true,
            missing: true,
            discovered_at: prev.discovered_at,
        });
        entries.push(CatalogEntry {
            upstream_model: prev.upstream_model.clone(),
            public_name: prev.public_name.clone(),
            selected: true,
            missing: true,
            is_new: false,
        });
    }

    state
        .store
        .replace_account_models(&account.id, &rows)
        .await?;
    sync_missing_targets(state, account, &previous, &rows).await?;
    Ok(entries)
}

/// 托管模式的一轮模型同步（§16.2）。
///
/// 拉取成功后把上游全部模型纳入调度——忽略选择集；拉取失败时什么都不改。
/// 与勾选路径的关键区别：托管不写选择集标记，关闭托管后立即回到管理员
/// 之前勾好的那几个模型。
pub async fn sync_managed(state: &SharedState, account: &Account) -> Result<usize> {
    refresh_catalog(state, account).await?;
    let catalog = state.store.list_account_models(&account.id).await?;
    let mut count = 0usize;
    for row in &catalog {
        if !row.missing {
            reconcile_row(state, account, row, true, false).await?;
            count += 1;
            continue;
        }
        // 上游已消失：停用而不是移除，目标配置与记录都保留（§16.5）。
        if let Some((_, target)) =
            locate_target(state, account, &row.public_name, &row.upstream_model).await?
            && target.enabled
        {
            let mut disabled = target.clone();
            disabled.enabled = false;
            state.store.update_target(&disabled).await?;
        }
    }

    // 托管期间被上游整个移出目录的模型没有行可调和，孤儿目标在这里清除。
    let known: HashSet<&str> = catalog.iter().map(|r| r.upstream_model.as_str()).collect();
    let models = state.store.list_logical_models().await?;
    for target in state.store.list_targets().await? {
        if target.account_id != account.id || known.contains(target.upstream_model.as_str()) {
            continue;
        }
        if let Some(model) = models.iter().find(|m| m.id == target.logical_model_id) {
            state.store.delete_target(&target.id).await?;
            cleanup_auto_model(state, model).await?;
        }
    }

    state.reload_config().await?;
    state
        .store
        .touch_model_sync(&account.id, now_unix())
        .await?;
    Ok(count)
}

/// 关闭托管：把调度目标收回选择集（§16.2）。
///
/// 托管期间选择集标记原封未动，所以这里按标记调和即可回到之前的勾选。
pub async fn unhost(state: &SharedState, account: &Account) -> Result<()> {
    let catalog = state.store.list_account_models(&account.id).await?;
    let desired: HashSet<String> = catalog
        .iter()
        .filter(|row| row.selected)
        .map(|row| row.public_name.clone())
        .collect();
    match apply_selection(state, account, &desired, true).await? {
        Selection::Applied(_) => Ok(()),
        // force=true 不会产生确认请求，这只是类型系统的穷尽性要求。
        Selection::NeedsConfirm(_) => Ok(()),
    }
}

/// 模型的消失与重现要同步到调度目标的启停（§16.5：停止新请求）。
async fn sync_missing_targets(
    state: &SharedState,
    account: &Account,
    previous: &[AccountModelRow],
    rows: &[AccountModelRow],
) -> Result<()> {
    let mut changed = false;
    for row in rows {
        let was_missing = previous
            .iter()
            .find(|p| p.upstream_model == row.upstream_model)
            .map(|p| p.missing)
            .unwrap_or(false);
        if was_missing == row.missing {
            continue;
        }
        if let Some((_, target)) =
            locate_target(state, account, &row.public_name, &row.upstream_model).await?
        {
            // 消失 → 停用；重现 → 恢复。与目标当前状态一致时不用动。
            if target.enabled == row.missing {
                let mut updated = target.clone();
                updated.enabled = !row.missing;
                state.store.update_target(&updated).await?;
                changed = true;
            }
        }
    }
    if changed {
        state.reload_config().await?;
    }
    Ok(())
}

// ---------------------------------------------------------------- 选择集

/// 把账号的选择集批量应用为期望状态（§16.2、§16.3）。
///
/// `desired` 是期望处于选择集内的**对外名**全集——对话框操作的就是对外名，
/// 同一对外名下的多个上游模型一起勾选、一起归并到同一个逻辑模型。这里
/// 负责把目录、调度目标调和到一致。移除有流量的模型需要 `force` 确认，
/// 未确认时整体不动。
pub async fn apply_selection(
    state: &SharedState,
    account: &Account,
    desired: &HashSet<String>,
    force: bool,
) -> Result<Selection> {
    let catalog = state.store.list_account_models(&account.id).await?;
    let traffic = recent_traffic(state, account).await?;

    // 先算清楚哪些移除需要确认：需要时整体拒绝，不做一半留一半。
    let mut conflicts = Vec::new();
    for row in &catalog {
        if desired.contains(&row.public_name) {
            continue;
        }
        if let Some((_, target)) =
            locate_target(state, account, &row.public_name, &row.upstream_model).await?
        {
            // 已停用的目标本来就不接新请求，移除它不需要打扰管理员。
            if !target.enabled {
                continue;
            }
            let calls = traffic.get(&row.public_name).copied().unwrap_or(0);
            if calls > 0 {
                conflicts.push(UnselectWarning {
                    public_name: row.public_name.clone(),
                    calls,
                });
            }
        }
    }
    if !conflicts.is_empty() && !force {
        return Ok(Selection::NeedsConfirm(conflicts));
    }

    let mut outcome = SelectionOutcome::default();
    for row in &catalog {
        let want = desired.contains(&row.public_name);
        match reconcile_row(state, account, row, want, true).await? {
            Change::Created => outcome.created += 1,
            Change::Removed => outcome.removed += 1,
            Change::None => {}
        }
    }
    state.reload_config().await?;
    Ok(Selection::Applied(outcome))
}

/// 手动添加一个上游模型并立即纳入调度（§16.5）。
///
/// 手动指定的对外名同时写入别名表，这样后续拉取也能保住这个名字；
/// 手动模型不会覆盖目录里已有的记录，只会把它重新选上。
pub async fn add_manual_model(
    state: &SharedState,
    account: &Account,
    upstream_model: &str,
    public_name: Option<&str>,
) -> Result<()> {
    let upstream = validate_model_id(upstream_model)?;
    let public = match public_name.map(str::trim).filter(|p| !p.is_empty()) {
        Some(name) => {
            if name.chars().count() > 100 {
                bail!("对外名不能超过 100 个字符");
            }
            name.to_string()
        }
        None => upstream.clone(),
    };

    if public != upstream {
        let mut aliases = state.store.list_account_aliases(&account.id).await?;
        aliases.retain(|a| a.upstream_model != upstream);
        aliases.push(AccountAliasRow {
            upstream_model: upstream.clone(),
            public_name: public.clone(),
        });
        aliases.sort_by(|a, b| a.upstream_model.cmp(&b.upstream_model));
        state
            .store
            .replace_account_aliases(&account.id, &aliases)
            .await?;
    }

    let mut rows = state.store.list_account_models(&account.id).await?;
    let row = match rows.iter_mut().find(|r| r.upstream_model == upstream) {
        Some(row) => {
            row.selected = true;
            row.missing = false;
            row.public_name = public;
            row.clone()
        }
        None => {
            let row = AccountModelRow {
                upstream_model: upstream,
                public_name: public,
                selected: true,
                missing: false,
                discovered_at: now_unix(),
            };
            rows.push(row.clone());
            row
        }
    };
    state
        .store
        .replace_account_models(&account.id, &rows)
        .await?;

    reconcile_row(state, account, &row, true, true).await?;
    state.reload_config().await?;
    Ok(())
}

/// 单行目录记录的目标变更。
enum Change {
    None,
    Created,
    Removed,
}

/// 把一行目录记录调和到期望状态：建目标、删目标或启停。
///
/// `update_flags` 决定是否把选择集标记改成 `want`：托管模式必须传 `false`，
/// 否则全量托管会污染选择集，关闭托管后就回不去了（§16.2）。
async fn reconcile_row(
    state: &SharedState,
    account: &Account,
    row: &AccountModelRow,
    want: bool,
    update_flags: bool,
) -> Result<Change> {
    let change = match locate_target(state, account, &row.public_name, &row.upstream_model).await? {
        Some((_, target)) if want => {
            // 消失中的模型保持停用，重现时由 sync_missing_targets 恢复。
            if !target.enabled && !row.missing {
                let mut enabled = target.clone();
                enabled.enabled = true;
                state.store.update_target(&enabled).await?;
            }
            Change::None
        }
        Some((model, target)) => {
            state.store.delete_target(&target.id).await?;
            cleanup_auto_model(state, &model).await?;
            Change::Removed
        }
        None if want => {
            let model = match find_logical_model(state, &account.group_id, &row.public_name).await?
            {
                Some(model) => model,
                None => create_auto_model(state, account, &row.public_name).await?,
            };
            // 优先级留空即继承账号默认人工优先级（§16.3）。
            let target = DispatchTarget {
                id: ids::target(),
                logical_model_id: model.id,
                account_id: account.id.clone(),
                upstream_model: row.upstream_model.clone(),
                priority_override: None,
                limits: Limits::default(),
                enabled: !row.missing,
                created_at: OffsetDateTime::now_utc(),
            };
            state.store.insert_target(&target).await?;
            Change::Created
        }
        None => Change::None,
    };

    if update_flags && row.selected != want {
        state
            .store
            .set_account_model_selected(&account.id, &row.upstream_model, want)
            .await?;
    }
    Ok(change)
}

/// 自动创建的逻辑模型失去最后一个目标时随之清理（§16.3）。
async fn cleanup_auto_model(state: &SharedState, model: &LogicalModel) -> Result<()> {
    if model.origin != ModelOrigin::Auto {
        return Ok(());
    }
    let remaining = state
        .store
        .list_targets()
        .await?
        .iter()
        .filter(|t| t.logical_model_id == model.id)
        .count();
    if remaining == 0 {
        state.store.delete_logical_model(&model.id).await?;
    }
    Ok(())
}

async fn create_auto_model(
    state: &SharedState,
    account: &Account,
    name: &str,
) -> Result<LogicalModel> {
    let model = LogicalModel {
        id: ids::logical_model(),
        group_id: account.group_id.clone(),
        name: name.to_string(),
        // 勾选自动创建的模型零目标时随之清理（§4.4、§16.3）。
        origin: ModelOrigin::Auto,
        enabled: true,
        created_at: OffsetDateTime::now_utc(),
    };
    state.store.insert_logical_model(&model).await?;
    Ok(model)
}

/// 按分组与对外名定位逻辑模型。
async fn find_logical_model(
    state: &SharedState,
    group_id: &str,
    name: &str,
) -> Result<Option<LogicalModel>> {
    Ok(state
        .store
        .list_logical_models()
        .await?
        .into_iter()
        .find(|m| m.group_id == group_id && m.name == name))
}

/// 定位"对外名对应逻辑模型 + 账号 + 上游模型名"指向的调度目标。
///
/// 返回逻辑模型本身，移除目标后要靠它判断自动清理。
async fn locate_target(
    state: &SharedState,
    account: &Account,
    public_name: &str,
    upstream_model: &str,
) -> Result<Option<(LogicalModel, DispatchTarget)>> {
    let Some(model) = find_logical_model(state, &account.group_id, public_name).await? else {
        return Ok(None);
    };
    let target = state.store.list_targets().await?.into_iter().find(|t| {
        t.logical_model_id == model.id
            && t.account_id == account.id
            && t.upstream_model == upstream_model
    });
    Ok(target.map(|t| (model, t)))
}

/// 该账号各逻辑模型最近 24 小时的调用次数（§16.3）。
async fn recent_traffic(state: &SharedState, account: &Account) -> Result<HashMap<String, i64>> {
    let since = now_unix() - TRAFFIC_WINDOW_SECS;
    Ok(state
        .store
        .recent_request_counts_by_account(&account.id, since)
        .await?
        .into_iter()
        .collect())
}

/// 手动输入与别名都要过的模型名校验，与拉取时的清洗规则一致（§16.1）。
fn validate_model_id(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("模型名不能为空");
    }
    if trimmed.chars().any(char::is_whitespace) {
        bail!("模型名不能包含空白字符");
    }
    if trimmed.len() > 256 {
        bail!("模型名不能超过 256 个字符");
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_reject_empty_whitespace_and_oversized_names() {
        assert!(validate_model_id(" gpt-4o ").is_ok());
        assert!(validate_model_id("").is_err());
        assert!(validate_model_id("  ").is_err());
        assert!(validate_model_id("a b").is_err());
        assert!(validate_model_id(&"x".repeat(257)).is_err());
    }
}
