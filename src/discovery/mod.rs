//! 模型发现与选择集（§16 的修订版）。
//!
//! 账号目录（`account_models`）是"上游列表 + 别名 + 启用状态"的唯一真相。模型
//! 管理不要求管理员另外创建"逻辑模型"：目录里的一行 `上游真名 → 对外名` 自动
//! 映射到一个同组逻辑模型；多行对外名相同，就归并到同一个逻辑模型下成为多个
//! 候选目标（§16.3、§16.4）。目标由目录行调和生成，优先级继承账号人工优先级。
//!
//! 分两层：`pub async fn xxx(state, ...)` 是一次完整的配置写入（网络请求放在事务
//! 之前）；`*_in_tx` 只在调用方已开的 [`ConfigTx`] 里干活，让账号保存能把改行、
//! 搬家与调和放进同一个事务（§21）。

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;

use crate::app::SharedState;
use crate::config::Revision;
use crate::domain::{Account, DispatchTarget, Limits, LogicalModel};
use crate::storage::ConfigTx;
use crate::storage::now_unix;
use crate::storage::store::{AccountModelRow, ids};
use crate::upstream::UpstreamError;

/// 取消勾选前的二次确认信息（§16.3）：该模型最近 24 小时的调用次数。
#[derive(Debug, Clone)]
pub struct UnselectWarning {
    pub public_name: String,
    pub calls: i64,
}

/// 停用 / 删除有流量的模型未确认：事务回滚，什么都没改。
///
/// 做成错误是因为它必须让写事务回滚、且不能推进修订号；调用方用
/// `downcast_ref` 取出来展示。
#[derive(Debug)]
pub struct NeedsConfirm(pub Vec<UnselectWarning>);

impl std::fmt::Display for NeedsConfirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} 个模型最近 24 小时有流量，需要确认", self.0.len())
    }
}

impl std::error::Error for NeedsConfirm {}

/// 模型管理对话框里的一行。
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub row: AccountModelRow,
    /// 本次拉取新出现的模型，前端用"只看新增"过滤。
    pub is_new: bool,
}

/// 调和一行目录记录时希望达到的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowIntent {
    /// 确保存在启用中的目标，并跟随当前别名。
    Ensure,
    /// 保留目标但停用（上游已消失，等待重现）。
    Disable,
    /// 移除目标（已经从选择集里撤下）。
    Remove,
}

/// "最近有流量"的判定窗口（§16.3）。
const TRAFFIC_WINDOW_SECS: i64 = 24 * 3600;

// -------------------------------------------------------------- 拉取与合并

/// 用账号 Key 池拉上游模型列表（§4.2.1）：第一把启用的 Key，401/403 换下一把。
///
/// 只有"这把 Key 不被接受"才换：超时与 5xx 是站点的问题，换 Key 只会把同一个
/// 故障重放一遍。
async fn fetch_upstream_models(state: &SharedState, account: &Account) -> Result<Vec<String>> {
    let keys = state.store.list_account_key_rows(&account.id).await?;
    let mut last_error = None;
    for row in keys.iter().filter(|row| row.enabled) {
        let plaintext = state.cipher.open(&row.sealed_key)?;
        let api_key = String::from_utf8_lossy(&plaintext).into_owned();
        match crate::upstream::fetch_model_list(
            state.upstream.http_for(account.allow_private_network),
            &account.base_url,
            account.preferred_protocol,
            &api_key,
        )
        .await
        {
            Ok(names) => return Ok(names),
            Err(error @ UpstreamError::BadStatus(401 | 403)) => last_error = Some(error),
            Err(error) => return Err(error).context("上游模型列表请求失败"),
        }
    }
    match last_error {
        Some(error) => Err(error).context("上游模型列表请求失败"),
        None => bail!("账号还没有启用的 API Key"),
    }
}

/// 拉取上游模型列表并合并进账号目录（§16.1 第 1–5 步、§16.5）。
///
/// 成功后整体替换目录快照、调和调度目标并返回对话框数据；请求失败时这里
/// 直接返回错误，调用方保留原目录与选择集，只把错误展示出来。
pub async fn refresh_catalog(
    state: &SharedState,
    account: &Account,
    revision: Revision,
) -> Result<Vec<CatalogEntry>> {
    let upstream_names = fetch_upstream_models(state, account).await?;
    // 0 个有效模型与请求失败同等对待：保留原列表和选择集，只报错（§16.1）。
    // 否则上游一次抽风返回空列表就会把整份目录和选择集清空。
    if upstream_names.is_empty() {
        bail!("上游返回 0 个有效模型，已保留原目录");
    }
    state
        .write_config(revision, async |tx| -> Result<Vec<CatalogEntry>> {
            let account = current_account(tx, &account.id).await?;
            let entries = merge_catalog_in_tx(tx, &account, &upstream_names).await?;
            reconcile_in_tx(tx, &account, account.auto_sync).await?;
            Ok(entries)
        })
        .await
}

/// 事务开始前读到的账号可能已经过期；事务里以库为准。
async fn current_account(tx: &mut ConfigTx, id: &str) -> Result<Account> {
    tx.account(id).await?.context("账号不存在")
}

/// 把一次拉取结果合并进目录。
async fn merge_catalog_in_tx(
    tx: &mut ConfigTx,
    account: &Account,
    upstream_names: &[String],
) -> Result<Vec<CatalogEntry>> {
    let previous = tx.list_account_models(&account.id).await?;
    let previous_by_name: HashMap<&str, &AccountModelRow> = previous
        .iter()
        .map(|row| (row.upstream_model.as_str(), row))
        .collect();

    // 多账号行为（§16.2）：全新账号的列表里，分组内已存在的同名逻辑模型
    // 默认勾选——你已经表达过"我要这个模型"。未分配账号在被分配进分组时
    // 由 [`default_select_in_tx`] 补上同一件事。
    let group_models = if previous.is_empty() {
        group_model_names(tx, account).await?
    } else {
        HashSet::new()
    };

    let now = now_unix();
    let upstream_set: HashSet<&str> = upstream_names.iter().map(String::as_str).collect();
    let mut entries = Vec::with_capacity(upstream_names.len());
    for name in upstream_names {
        let known = previous_by_name.get(name.as_str());
        let row = match known {
            // 已启用的保持启用，曾明确取消的保持停用（§16.2）。
            Some(prev) => AccountModelRow {
                missing: false,
                ..(*prev).clone()
            },
            None => AccountModelRow {
                upstream_model: name.clone(),
                public_name: name.clone(),
                selected: group_models.contains(name),
                excluded: false,
                missing: false,
                discovered_at: now,
            },
        };
        entries.push(CatalogEntry {
            row,
            is_new: known.is_none(),
        });
    }
    // 消失的模型（§16.5）：已启用的保留记录并标记 missing；停用的移出目录。
    for prev in &previous {
        if upstream_set.contains(prev.upstream_model.as_str()) || !prev.selected {
            continue;
        }
        entries.push(CatalogEntry {
            row: AccountModelRow {
                missing: true,
                ..prev.clone()
            },
            is_new: false,
        });
    }
    let rows: Vec<AccountModelRow> = entries.iter().map(|entry| entry.row.clone()).collect();
    tx.replace_account_models(&account.id, &rows).await?;
    Ok(entries)
}

/// 账号所在分组已有的逻辑模型名。未分配账号没有分组，返回空集。
async fn group_model_names(tx: &mut ConfigTx, account: &Account) -> Result<HashSet<String>> {
    let Some(group_id) = account.group_id.as_deref() else {
        return Ok(HashSet::new());
    };
    Ok(tx
        .list_logical_models()
        .await?
        .into_iter()
        .filter(|model| model.group_id == group_id)
        .map(|model| model.name)
        .collect())
}

/// 账号被分配进分组时，补上"同组已有的同名模型默认勾选"（§16.2）。
///
/// 只动管理员从没碰过的行：启用的本来就启用，亲手停用过的（`excluded`）必须
/// 保持停用。要在调和之前调用，勾选才会变成目标。
pub async fn default_select_in_tx(tx: &mut ConfigTx, account: &Account) -> Result<usize> {
    let names = group_model_names(tx, account).await?;
    let rows: Vec<AccountModelRow> = tx
        .list_account_models(&account.id)
        .await?
        .into_iter()
        .filter(|row| row.untouched() && names.contains(&row.public_name))
        .map(|row| AccountModelRow {
            selected: true,
            ..row
        })
        .collect();
    tx.upsert_account_models(&account.id, &rows).await?;
    Ok(rows.len())
}

/// 托管模式的一轮模型同步（§16.2）。
///
/// 拉取成功后把上游全部模型纳入调度——忽略启用标记；拉取失败时什么都不改。
/// 与手动选择的关键区别：托管不写选择集标记，关闭托管后立即回到管理员
/// 之前启用的那几个模型。这是后台行为，不推进配置修订号（§7.4）。
pub async fn sync_managed(state: &SharedState, account: &Account) -> Result<usize> {
    let upstream_names = fetch_upstream_models(state, account).await?;
    if upstream_names.is_empty() {
        bail!("上游返回 0 个有效模型，已保留原目录");
    }
    let managed = state
        .write_config(Revision::Background, async |tx| -> Result<usize> {
            let account = current_account(tx, &account.id).await?;
            merge_catalog_in_tx(tx, &account, &upstream_names).await?;
            // 托管无视选择集：所有未消失的目录行都纳入调度。
            reconcile_in_tx(tx, &account, true).await?;
            let catalog = tx.list_account_models(&account.id).await?;
            Ok(catalog.iter().filter(|row| !row.missing).count())
        })
        .await?;
    state
        .store
        .touch_model_sync(&account.id, now_unix())
        .await?;
    Ok(managed)
}

/// 一键移除目录里所有"上游已消失"的行（§16.5，2026-10-15 修订）。
///
/// 不逐行校验名字：这个方法存在的意义就是**把清理做完**，目录里一条历史遗留的
/// 非法名字不该让整批清理失败（它本来就是待清理的对象）。
pub async fn remove_missing(
    state: &SharedState,
    account: &Account,
    revision: Revision,
) -> Result<usize> {
    state
        .write_config(revision, async |tx| -> Result<usize> {
            let account = current_account(tx, &account.id).await?;
            let missing: Vec<String> = tx
                .list_account_models(&account.id)
                .await?
                .into_iter()
                .filter(|row| row.missing)
                .map(|row| row.upstream_model)
                .collect();
            tx.delete_account_models(&account.id, &missing).await?;
            reconcile_in_tx(tx, &account, account.auto_sync).await?;
            Ok(missing.len())
        })
        .await
}

/// 手动添加一个上游模型并立即纳入调度（§16.5）。
///
/// 指定的对外名写入目录行，因此后续刷新会保住这个名字；已存在的记录只会
/// 被重新启用并更新对外配置，不会被整表拉取覆盖。
pub async fn add_manual_model(
    state: &SharedState,
    account: &Account,
    upstream_model: &str,
    public_name: Option<&str>,
    revision: Revision,
) -> Result<()> {
    let upstream = validate_model_id(upstream_model)?;
    let public = match public_name {
        Some(alias) if !alias.trim().is_empty() => normalize_alias(&upstream, alias)?,
        _ => upstream.clone(),
    };
    state
        .write_config(revision, async |tx| -> Result<()> {
            let account = current_account(tx, &account.id).await?;
            let discovered_at = tx
                .list_account_models(&account.id)
                .await?
                .into_iter()
                .find(|row| row.upstream_model == upstream)
                .map_or_else(now_unix, |row| row.discovered_at);
            let row = AccountModelRow {
                upstream_model: upstream.clone(),
                public_name: public.clone(),
                selected: true,
                excluded: false,
                missing: false,
                discovered_at,
            };
            tx.upsert_account_models(&account.id, std::slice::from_ref(&row))
                .await?;
            reconcile_in_tx(tx, &account, account.auto_sync).await?;
            Ok(())
        })
        .await
}

// ------------------------------------------------------------------ 批量应用

/// 一次批量应用里的单行改动（§16.3）。
///
/// 界面的勾选是"本地草稿 + 防抖提交"：连点十几个复选框只发一次请求，服务端
/// 也只调和一遍目标、只切换一次配置。
#[derive(Debug, Clone)]
pub struct ModelChange {
    pub upstream_model: String,
    /// `None` 表示不改下游模型名；空字符串表示清空、回到上游原名。
    pub alias: Option<String>,
    pub selected: Option<bool>,
    /// 从目录里删除这一行（连同它的调度目标）。
    pub delete: bool,
}

/// 批量应用模型目录改动：模型的唯一写路径（§16.3、§16.8）。
///
/// 停用 / 删除有流量的模型未确认时返回 [`NeedsConfirm`] 错误，整体回滚。
pub async fn apply_changes(
    state: &SharedState,
    account: &Account,
    changes: &[ModelChange],
    force: bool,
    revision: Revision,
) -> Result<usize> {
    if changes.is_empty() {
        return Ok(0);
    }
    // 请求记录不是配置，事务开始前读，不占写锁。
    let traffic = if !force
        && changes
            .iter()
            .any(|change| change.delete || change.selected == Some(false))
    {
        recent_traffic(state, account).await?
    } else {
        HashMap::new()
    };
    state
        .write_config(revision, async |tx| -> Result<usize> {
            let account = current_account(tx, &account.id).await?;
            let catalog = tx.list_account_models(&account.id).await?;
            let by_name: HashMap<&str, &AccountModelRow> = catalog
                .iter()
                .map(|row| (row.upstream_model.as_str(), row))
                .collect();

            let mut seen: HashSet<String> = HashSet::with_capacity(changes.len());
            let mut writes: Vec<AccountModelRow> = Vec::new();
            let mut deletes: Vec<String> = Vec::new();
            let mut warnings: Vec<UnselectWarning> = Vec::new();
            for change in changes {
                let upstream = validate_model_id(&change.upstream_model)?;
                if !seen.insert(upstream.clone()) {
                    bail!("模型 {upstream} 在同一次请求里出现了两次");
                }
                let Some(row) = by_name.get(upstream.as_str()).copied() else {
                    bail!("模型不在当前目录里：{upstream}");
                };
                let turning_off = row.selected && (change.delete || change.selected == Some(false));
                if turning_off
                    && let Some(calls) = traffic.get(&row.public_name).copied()
                    && calls > 0
                {
                    warnings.push(UnselectWarning {
                        public_name: row.public_name.clone(),
                        calls,
                    });
                }
                if change.delete {
                    deletes.push(upstream);
                    continue;
                }
                let mut updated = row.clone();
                if let Some(alias) = &change.alias {
                    updated.public_name = normalize_alias(&upstream, alias)?;
                }
                if let Some(selected) = change.selected {
                    // 只有"启用 → 停用"才算管理员亲手排除过；从没启用过的行被
                    // 提交一个 false 不改变它"没碰过"的身份。
                    if selected {
                        updated.excluded = false;
                    } else if row.selected {
                        updated.excluded = true;
                    }
                    updated.selected = selected;
                }
                if updated != *row {
                    writes.push(updated);
                }
            }
            if !warnings.is_empty() {
                return Err(NeedsConfirm(warnings).into());
            }
            tx.upsert_account_models(&account.id, &writes).await?;
            tx.delete_account_models(&account.id, &deletes).await?;
            reconcile_in_tx(tx, &account, account.auto_sync).await?;
            Ok(writes.len() + deletes.len())
        })
        .await
}

// -------------------------------------------------------------- 目标调和核心

/// 启动时把账号目录与目标收敛一次。后台行为，不推进修订号。
pub async fn reconcile_account(
    state: &SharedState,
    account: &Account,
    include_all: bool,
) -> Result<()> {
    state
        .write_config(Revision::Background, async |tx| -> Result<()> {
            reconcile_in_tx(tx, account, include_all).await
        })
        .await
}

/// 把账号目录里的每一行调和成对应的调度目标。
///
/// `include_all = true` 用于自动同步（托管）：忽略启用标记，所有未消失的
/// 模型都参与调度；`false` 用于手动选择：只有启用且未消失的模型有目标。
/// 目录里已经没有的目标（删除行、被上游移出）一并撤下，零目标的逻辑模型随之
/// 清理（§16.3）。
pub async fn reconcile_in_tx(
    tx: &mut ConfigTx,
    account: &Account,
    include_all: bool,
) -> Result<()> {
    // 未分配账号不参与调度（§4.2.3）：目录为空集的效果就是撤下全部目标，
    // 目录行与选择集原封不动，重新分配时按目录重建。
    let catalog = match account.group_id {
        Some(_) => tx.list_account_models(&account.id).await?,
        None => Vec::new(),
    };
    let mut targets = tx.list_targets().await?;
    let mut models = tx.list_logical_models().await?;

    let intents: HashMap<&str, RowIntent> = catalog
        .iter()
        .map(|row| {
            let intent = if !(include_all || row.selected) {
                RowIntent::Remove
            } else if row.missing {
                RowIntent::Disable
            } else {
                RowIntent::Ensure
            };
            (row.upstream_model.as_str(), intent)
        })
        .collect();

    // 本账号的目标按上游模型名建索引。逐行扫全表是 O(目录行数 × 目标数)，
    // 几百个模型的账号上一次勾选就会明显卡住。
    let mut owned: HashMap<String, usize> = HashMap::with_capacity(catalog.len());
    for (index, target) in targets.iter().enumerate() {
        if target.account_id == account.id {
            owned.insert(target.upstream_model.clone(), index);
        }
    }

    let mut model_index: HashMap<(String, String), usize> = models
        .iter()
        .enumerate()
        .map(|(index, model)| ((model.group_id.clone(), model.name.clone()), index))
        .collect();

    let mut target_writes: Vec<DispatchTarget> = Vec::new();
    let mut target_deletes: Vec<String> = Vec::new();
    let mut model_writes: Vec<LogicalModel> = Vec::new();

    // 还有目标的逻辑模型：先算别的账号的；本账号要保留的目标在下面逐行补上。
    // 不能把本账号现有目标所在的模型直接算进来：换组或改名时目标会搬走，
    // 旧模型得跟着被清理。
    let mut live: HashSet<String> = HashSet::new();
    for target in &targets {
        if target.account_id != account.id {
            live.insert(target.logical_model_id.clone());
        } else if matches!(
            intents.get(target.upstream_model.as_str()),
            None | Some(RowIntent::Remove)
        ) {
            target_deletes.push(target.id.clone());
        }
    }

    for row in &catalog {
        let intent = intents[row.upstream_model.as_str()];
        let position = owned.get(&row.upstream_model).copied();
        if intent == RowIntent::Remove || (intent == RowIntent::Disable && position.is_none()) {
            continue;
        }
        // 能走到这里说明目录非空，而未分配账号的目录视为空。
        let group_id = account.group_id.as_deref().context("账号没有分组")?;
        let model = ensure_logical_model(
            &mut models,
            &mut model_writes,
            &mut model_index,
            group_id,
            &row.public_name,
        );
        let desired_id = models[model].id.clone();
        live.insert(desired_id.clone());
        let enabled = intent == RowIntent::Ensure;
        match position {
            // 原地改写而不是删了重建：目标 ID 不变，健康与性能状态才接得上。
            Some(index) => {
                let target = &mut targets[index];
                if target.logical_model_id != desired_id || target.enabled != enabled {
                    target.logical_model_id = desired_id;
                    target.enabled = enabled;
                    target_writes.push(target.clone());
                }
            }
            None => target_writes.push(DispatchTarget {
                id: ids::target(),
                logical_model_id: desired_id,
                account_id: account.id.clone(),
                upstream_model: row.upstream_model.clone(),
                limits: Limits::default(),
                enabled,
                created_at: OffsetDateTime::now_utc(),
            }),
        }
    }

    // 零目标的逻辑模型随最后一个目标一起清理（§16.3）。
    let model_deletes: Vec<String> = models
        .iter()
        .filter(|model| !live.contains(&model.id))
        .map(|model| model.id.clone())
        .collect();

    tx.apply_config_delta(
        &model_writes,
        &target_writes,
        &target_deletes,
        &model_deletes,
    )
    .await
}

/// 按分组与对外名定位逻辑模型；不存在时登记待创建并返回它在 models 里的下标。
///
/// 新建的模型同时写进 pending 与内存索引，所以同一次调和里多个目录行要同一个
/// 对外名时只会建一个——"两个上游名归并到同一个下游名"正是靠这一点才成为同一
/// 个逻辑模型（§16.4）。
fn ensure_logical_model(
    models: &mut Vec<LogicalModel>,
    pending: &mut Vec<LogicalModel>,
    index: &mut HashMap<(String, String), usize>,
    group_id: &str,
    name: &str,
) -> usize {
    let key = (group_id.to_string(), name.to_string());
    if let Some(existing) = index.get(&key) {
        return *existing;
    }
    let model = LogicalModel {
        id: ids::logical_model(),
        group_id: group_id.to_string(),
        name: name.to_string(),
        enabled: true,
        created_at: OffsetDateTime::now_utc(),
    };
    models.push(model.clone());
    pending.push(model);
    let position = models.len() - 1;
    index.insert(key, position);
    position
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

// ------------------------------------------------------------------ 校验

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

/// 对外名的校验：允许清除（空串表示跟随上游真名）。
fn normalize_alias(upstream: &str, raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(upstream.to_string());
    }
    if trimmed.chars().any(char::is_whitespace) {
        bail!("对外名不能包含空白字符");
    }
    if trimmed.chars().count() > 100 {
        bail!("对外名不能超过 100 个字符");
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

    #[test]
    fn clearing_an_alias_falls_back_to_the_upstream_name() {
        assert_eq!(normalize_alias("gpt-5.6-sol", "").unwrap(), "gpt-5.6-sol");
        assert_eq!(
            normalize_alias("gpt-5.6-sol", " gpt-5.6-sol-openai ").unwrap(),
            "gpt-5.6-sol-openai"
        );
    }
}
