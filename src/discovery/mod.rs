//! 模型发现与选择集（§16 的修订版）。
//!
//! 账号目录（`account_models`）是"上游列表 + 别名 + 启用状态 + 隐藏原始名"
//! 的唯一真相。模型管理不再要求管理员另外创建"逻辑模型"：目录里的一行
//! `上游真名 → 对外名` 会自动映射到一个同组逻辑模型；多行的对外名相同，
//! 就会自动归并到同一个逻辑模型下成为多个候选目标（§16.3、§16.4）。
//!
//! 页面不再直接编辑"调度目标"：目标由目录行自动调和生成，优先级统一继承
//! 账号默认人工优先级，层内分配由综合评分决定（§9.2 修订）。

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;

use crate::app::SharedState;
use crate::domain::{Account, DispatchTarget, Limits, LogicalModel, ModelOrigin};
use crate::storage::now_unix;
use crate::storage::store::{AccountModelRow, ids};

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

/// 模型管理对话框里的一行。
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub upstream_model: String,
    /// 对外名：未设置别名时等于上游真名。
    pub public_name: String,
    /// 是否隐藏原始上游名。
    pub hide_original: bool,
    pub selected: bool,
    pub missing: bool,
    /// 本次拉取新出现的模型，前端用"只看新增"过滤。
    pub is_new: bool,
    /// 管理员**明确取消过勾选**的模型（§16.2）。
    ///
    /// 与"从没出现过"必须分开：模型管理要显示"你排除过"，
    /// 否则一个被主动排除的模型和一个从未见过的模型长得一模一样，
    /// 管理员会以为自己之前的操作没生效。
    pub excluded: bool,
}

/// 调和一行目录记录时希望达到的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowIntent {
    /// 确保存在启用中的目标，并跟随当前别名 / 隐藏设置。
    Ensure,
    /// 保留目标但停用（上游已消失，等待重现）。
    Disable,
    /// 移除目标（已经从选择集里撤下）。
    Remove,
}

/// "最近有流量"的判定窗口（§16.3）。
const TRAFFIC_WINDOW_SECS: i64 = 24 * 3600;

// -------------------------------------------------------------- 拉取与合并

/// 拉取上游模型列表并合并进账号目录（§16.1 第 1–5 步、§16.5）。
///
/// 成功后整体替换目录快照、调和调度目标并返回对话框数据；请求失败时这里
/// 直接返回错误，调用方保留原目录与选择集，只把错误展示出来。
pub async fn refresh_catalog(state: &SharedState, account: &Account) -> Result<Vec<CatalogEntry>> {
    let sealed = state
        .store
        .account_sealed_key(&account.id)
        .await?
        .context("账号还没有保存 API Key")?;
    let key = state.cipher.open(&sealed)?;
    let api_key = String::from_utf8_lossy(&key).into_owned();
    let upstream_names = crate::upstream::fetch_model_list(
        state.upstream.http_for(account.allow_private_network),
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

    // 旧版别名表里的数据只作为兜底：v8 迁移已回填到 account_models，
    // 目录行里保存的 public_name 才是当前唯一的真相。
    let legacy_aliases: HashMap<String, String> = state
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
        let known = previous_by_name.get(name.as_str());
        let public_name = known
            .map(|prev| prev.public_name.clone())
            .or_else(|| legacy_aliases.get(name).cloned())
            .unwrap_or_else(|| name.clone());
        let selected = match known {
            // 已启用的保持启用，曾明确取消的保持停用（§16.2）。
            Some(prev) => prev.selected,
            None => group_models.contains(&public_name),
        };
        let hide_original = known.is_some_and(|prev| prev.hide_original);
        rows.push(AccountModelRow {
            upstream_model: name.clone(),
            public_name: public_name.clone(),
            hide_original,
            selected,
            missing: false,
            discovered_at: known.map(|p| p.discovered_at).unwrap_or(now),
        });
        entries.push(CatalogEntry {
            upstream_model: name.clone(),
            public_name,
            hide_original,
            selected,
            missing: false,
            is_new: known.is_none(),
            // 有历史记录、且当时是未启用状态 → 管理员明确排除过。
            excluded: known.is_some_and(|prev| !prev.selected),
        });
    }

    // 消失的模型（§16.5）：已启用的保留记录并标记 missing；停用的移出目录。
    for prev in &previous {
        if upstream_set.contains(prev.upstream_model.as_str()) || !prev.selected {
            continue;
        }
        rows.push(AccountModelRow {
            upstream_model: prev.upstream_model.clone(),
            public_name: prev.public_name.clone(),
            hide_original: prev.hide_original,
            selected: true,
            missing: true,
            discovered_at: prev.discovered_at,
        });
        entries.push(CatalogEntry {
            upstream_model: prev.upstream_model.clone(),
            public_name: prev.public_name.clone(),
            hide_original: prev.hide_original,
            selected: true,
            missing: true,
            is_new: false,
            excluded: false,
        });
    }

    state
        .store
        .replace_account_models(&account.id, &rows)
        .await?;
    // 目录是唯一真相：刷新后把目标调和到目录状态（别名变化、消失/重现都
    // 在这一步收敛）。
    reconcile_account(state, account, account.auto_sync).await?;
    Ok(entries)
}

/// 托管模式的一轮模型同步（§16.2）。
///
/// 拉取成功后把上游全部模型纳入调度——忽略启用标记；拉取失败时什么都不改。
/// 与手动选择的关键区别：托管不写选择集标记，关闭托管后立即回到管理员
/// 之前启用的那几个模型。
pub async fn sync_managed(state: &SharedState, account: &Account) -> Result<usize> {
    refresh_catalog(state, account).await?;
    // 托管无视选择集：即使这个账号对象还是旧快照（测试会直接调用这里），
    // 也要把所有未消失的目录行纳入调度。
    reconcile_account(state, account, true).await?;
    let catalog = state.store.list_account_models(&account.id).await?;
    let known: HashSet<&str> = catalog.iter().map(|r| r.upstream_model.as_str()).collect();

    // 托管期间被上游整个移出目录的模型没有行可调和，孤儿目标在这里清除。
    for target in state.store.list_targets().await? {
        if target.account_id != account.id || known.contains(target.upstream_model.as_str()) {
            continue;
        }
        state.store.delete_target(&target.id).await?;
        cleanup_model_if_empty(state, &target.logical_model_id).await?;
    }

    state.reload_config().await?;
    state
        .store
        .touch_model_sync(&account.id, now_unix())
        .await?;
    Ok(catalog.iter().filter(|row| !row.missing).count())
}

/// 关闭托管：把调度目标收回选择集（§16.2）。
///
/// 托管期间选择集标记原封未动，所以这里按标记调和即可回到之前的启用模型。
pub async fn unhost(state: &SharedState, account: &Account) -> Result<()> {
    reconcile_account(state, account, false).await?;
    Ok(())
}

// ---------------------------------------------------------------- 单行增删改

/// 修改一行目录的下游模型名 / 启用状态，并立刻调和目标。
///
/// - `alias = Some("")` 表示清空下游模型名，回到上游原名；
/// - 「隐藏原始模型名」是账号级开关（`Account::hide_original`），不在这里逐行设置；
/// - 停用会移除对应调度目标，但保留目录行与下游模型名，下次重新启用时恢复。
pub async fn update_model(
    state: &SharedState,
    account: &Account,
    upstream_model: &str,
    alias: Option<&str>,
    selected: Option<bool>,
) -> Result<()> {
    let upstream = validate_model_id(upstream_model)?;
    let mut rows = state.store.list_account_models(&account.id).await?;
    let row = rows
        .iter_mut()
        .find(|row| row.upstream_model == upstream)
        .context("模型不在当前目录里")?;

    if let Some(alias) = alias {
        row.public_name = normalize_alias(&upstream, alias)?;
    }
    if let Some(selected) = selected {
        row.selected = selected;
    }
    let updated = row.clone();
    state
        .store
        .upsert_account_model(&account.id, &updated)
        .await?;
    reconcile_account(state, account, account.auto_sync).await?;
    Ok(())
}

/// 兼容旧别名接口：目录里还没有这一行时，先建一条未启用记录，等"获取模型"
/// 或用户启用时再生成目标（§16.4）。
pub async fn set_alias(
    state: &SharedState,
    account: &Account,
    upstream_model: &str,
    alias: &str,
) -> Result<()> {
    let upstream = validate_model_id(upstream_model)?;
    let rows = state.store.list_account_models(&account.id).await?;
    if rows.iter().any(|row| row.upstream_model == upstream) {
        return update_model(state, account, &upstream, Some(alias), None).await;
    }
    let public_name = normalize_alias(&upstream, alias)?;
    let row = AccountModelRow {
        upstream_model: upstream,
        hide_original: false,
        public_name,
        selected: false,
        missing: false,
        discovered_at: now_unix(),
    };
    state.store.upsert_account_model(&account.id, &row).await?;
    if account.auto_sync {
        reconcile_account(state, account, true).await?;
    }
    Ok(())
}

/// 从目录里永久删除一行，同时移除它的调度目标。
pub async fn delete_model(
    state: &SharedState,
    account: &Account,
    upstream_model: &str,
) -> Result<bool> {
    let upstream = validate_model_id(upstream_model)?;
    let removed = state
        .store
        .delete_account_model(&account.id, &upstream)
        .await?;
    if !removed {
        return Ok(false);
    }
    if let Some(target) = find_target_for_upstream(state, account, &upstream).await? {
        state.store.delete_target(&target.id).await?;
        cleanup_model_if_empty(state, &target.logical_model_id).await?;
    }
    state.reload_config().await?;
    Ok(true)
}

/// 把多个上游模型快速归并到同一个对外名（§16.4）。
///
/// 这是"同一模型在不同站点叫不同名字"的最短路径：选中几个目录行，
/// 指定一个对外名（可直接挑同组已有模型），一次提交完成归并。
pub async fn merge_models(
    state: &SharedState,
    account: &Account,
    upstream_models: &[String],
    public_name: &str,
) -> Result<usize> {
    if upstream_models.is_empty() {
        bail!("请至少选择一个上游模型");
    }
    let public_name = validate_public_name(public_name)?;
    let mut rows = state.store.list_account_models(&account.id).await?;
    let wanted: HashSet<&str> = upstream_models.iter().map(String::as_str).collect();

    let mut merged = 0usize;
    for row in rows.iter_mut() {
        if !wanted.contains(row.upstream_model.as_str()) {
            continue;
        }
        row.public_name = public_name.clone();
        row.selected = true;
        state.store.upsert_account_model(&account.id, row).await?;
        merged += 1;
    }
    if merged == 0 {
        bail!("选中的模型不在当前目录里");
    }
    reconcile_account(state, account, account.auto_sync).await?;
    Ok(merged)
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
) -> Result<()> {
    let upstream = validate_model_id(upstream_model)?;
    let public = match public_name {
        Some(alias) if !alias.trim().is_empty() => normalize_alias(&upstream, alias)?,
        _ => upstream.clone(),
    };
    let mut rows = state.store.list_account_models(&account.id).await?;
    let row = match rows.iter_mut().find(|r| r.upstream_model == upstream) {
        Some(row) => {
            row.selected = true;
            row.missing = false;
            row.public_name = public;
            row.clone()
        }
        None => AccountModelRow {
            upstream_model: upstream.clone(),
            public_name: public,
            hide_original: false,
            selected: true,
            missing: false,
            discovered_at: now_unix(),
        },
    };
    state.store.upsert_account_model(&account.id, &row).await?;
    reconcile_account(state, account, account.auto_sync).await?;
    Ok(())
}

// ---------------------------------------------------------------- 选择集兼容

/// 把账号的选择集批量应用为期望状态（§16.2、§16.3）。
///
/// `desired` 同时接受上游真名与对外名，兼容旧前端；新前端使用逐行接口。
/// 移除有流量的模型需要 `force` 确认，未确认时整体不动。
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
        if is_desired(row, desired) {
            continue;
        }
        if let Some(target) = find_target_for_upstream(state, account, &row.upstream_model).await?
            && target.enabled
        {
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

    for row in &catalog {
        let want = is_desired(row, desired);
        if row.selected != want {
            state
                .store
                .set_account_model_selected(&account.id, &row.upstream_model, want)
                .await?;
        }
    }
    let before = state
        .store
        .list_targets()
        .await?
        .iter()
        .filter(|t| t.account_id == account.id)
        .count();
    reconcile_account(state, account, false).await?;
    let after = state
        .store
        .list_targets()
        .await?
        .iter()
        .filter(|t| t.account_id == account.id)
        .count();
    Ok(Selection::Applied(SelectionOutcome {
        created: after.saturating_sub(before),
        removed: before.saturating_sub(after),
    }))
}

fn is_desired(row: &AccountModelRow, desired: &HashSet<String>) -> bool {
    desired.contains(&row.upstream_model) || desired.contains(&row.public_name)
}

// -------------------------------------------------------------- 目标调和核心

/// 把账号目录里的每一行调和成对应的调度目标。
///
/// `include_all = true` 用于自动同步（托管）：忽略启用标记，所有未消失的
/// 模型都参与调度；`false` 用于手动选择：只有启用且未消失的模型有目标。
/// 返回是否改动了配置，调用方依赖这个信息决定要不要 reload。
pub async fn reconcile_account(
    state: &SharedState,
    account: &Account,
    include_all: bool,
) -> Result<bool> {
    let catalog = state.store.list_account_models(&account.id).await?;
    let mut targets = state.store.list_targets().await?;
    let mut models = state.store.list_logical_models().await?;
    let mut changed = false;

    // 旧版手工创建可能留下一组"同账号 + 同上游模型"的重复目标；新版模型目录
    // 每行只对应一个目标，先清掉重复项再调和，避免后面移动目标时撞唯一约束。
    let mut seen_targets: HashSet<(String, String)> = HashSet::new();
    let mut index = 0;
    while index < targets.len() {
        if targets[index].account_id != account.id {
            index += 1;
            continue;
        }
        let key = (
            targets[index].account_id.clone(),
            targets[index].upstream_model.clone(),
        );
        if seen_targets.insert(key) {
            index += 1;
        } else {
            let duplicate = targets.remove(index);
            state.store.delete_target(&duplicate.id).await?;
            changed = true;
        }
    }

    for row in &catalog {
        let wanted = include_all || row.selected;
        let intent = if !wanted {
            RowIntent::Remove
        } else if row.missing {
            RowIntent::Disable
        } else {
            RowIntent::Ensure
        };

        let position = targets
            .iter()
            .position(|t| t.account_id == account.id && t.upstream_model == row.upstream_model);

        match (intent, position) {
            (RowIntent::Ensure, Some(index)) => {
                let desired =
                    ensure_logical_model(state, &account.group_id, &row.public_name).await?;
                let target = &mut targets[index];
                let mut dirty = false;
                if target.logical_model_id != desired.id {
                    target.logical_model_id = desired.id.clone();
                    dirty = true;
                }
                if target.enabled != !row.missing {
                    target.enabled = !row.missing;
                    dirty = true;
                }
                if target.hide_original != account.hide_original {
                    target.hide_original = account.hide_original;
                    dirty = true;
                }
                // 历史字段不再参与调度，调和时顺手清掉，避免旧库恢复后行为分叉。
                if target.priority_override.is_some() {
                    target.priority_override = None;
                    dirty = true;
                }
                if dirty {
                    state.store.update_target(target).await?;
                    changed = true;
                }
                models.push(desired);
            }
            (RowIntent::Ensure, None) => {
                let desired =
                    ensure_logical_model(state, &account.group_id, &row.public_name).await?;
                let target = DispatchTarget {
                    id: ids::target(),
                    logical_model_id: desired.id.clone(),
                    account_id: account.id.clone(),
                    upstream_model: row.upstream_model.clone(),
                    hide_original: account.hide_original,
                    // 优先级统一继承账号默认人工优先级（§9.2 修订）。
                    priority_override: None,
                    limits: Limits::default(),
                    enabled: true,
                    created_at: OffsetDateTime::now_utc(),
                };
                state.store.insert_target(&target).await?;
                targets.push(target);
                models.push(desired);
                changed = true;
            }
            (RowIntent::Disable, Some(index)) => {
                if targets[index].enabled {
                    targets[index].enabled = false;
                    state.store.update_target(&targets[index]).await?;
                    changed = true;
                }
            }
            (RowIntent::Remove, Some(index)) => {
                let target = targets.remove(index);
                state.store.delete_target(&target.id).await?;
                changed = true;
            }
            (RowIntent::Disable | RowIntent::Remove, None) => {}
        }
    }

    // 零目标的自动创建逻辑模型随最后一个目标一起清理（§16.3）。
    let live: HashSet<String> = targets
        .iter()
        .map(|target| target.logical_model_id.clone())
        .collect();
    for model in models.iter().filter(|m| m.origin == ModelOrigin::Auto) {
        if !live.contains(&model.id) {
            state.store.delete_logical_model(&model.id).await?;
            changed = true;
        }
    }

    if changed {
        state.reload_config().await?;
    }
    Ok(changed)
}

/// 按分组与对外名找到逻辑模型；不存在时自动创建（§16.3）。
async fn ensure_logical_model(
    state: &SharedState,
    group_id: &str,
    name: &str,
) -> Result<LogicalModel> {
    if let Some(model) = find_logical_model(state, group_id, name).await? {
        return Ok(model);
    }
    let model = LogicalModel {
        id: ids::logical_model(),
        group_id: group_id.to_string(),
        name: name.to_string(),
        origin: ModelOrigin::Auto,
        enabled: true,
        created_at: OffsetDateTime::now_utc(),
    };
    state.store.insert_logical_model(&model).await?;
    Ok(model)
}

/// 自动创建的逻辑模型失去最后一个目标时随之清理（§16.3）。
async fn cleanup_model_if_empty(state: &SharedState, model_id: &str) -> Result<()> {
    let Some(model) = state
        .store
        .list_logical_models()
        .await?
        .into_iter()
        .find(|m| m.id == model_id)
    else {
        return Ok(());
    };
    if model.origin != ModelOrigin::Auto {
        return Ok(());
    }
    let remaining = state
        .store
        .list_targets()
        .await?
        .iter()
        .any(|t| t.logical_model_id == model.id);
    if !remaining {
        state.store.delete_logical_model(&model.id).await?;
    }
    Ok(())
}

/// 按"账号 + 上游模型"定位目标；一个账号的同一上游模型最多只有一个目标。
async fn find_target_for_upstream(
    state: &SharedState,
    account: &Account,
    upstream_model: &str,
) -> Result<Option<DispatchTarget>> {
    Ok(state
        .store
        .list_targets()
        .await?
        .into_iter()
        .find(|t| t.account_id == account.id && t.upstream_model == upstream_model))
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

fn validate_public_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("对外名不能为空");
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

    #[test]
    fn selection_accepts_both_upstream_and_public_names() {
        let row = AccountModelRow {
            upstream_model: "gpt-5.6-sol-openai".into(),
            public_name: "gpt-5.6-sol".into(),
            hide_original: true,
            selected: false,
            missing: false,
            discovered_at: 0,
        };
        let by_upstream: HashSet<String> = ["gpt-5.6-sol-openai".to_string()].into();
        let by_public: HashSet<String> = ["gpt-5.6-sol".to_string()].into();
        assert!(is_desired(&row, &by_upstream));
        assert!(is_desired(&row, &by_public));
    }
}
