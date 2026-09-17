//! 后台资源接口：分组、账号、逻辑模型、调度目标与只读视图。

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::{Admin, AdminError, AdminResult};
use crate::app::SharedState;
use crate::auth::session;
use crate::discovery;
use crate::domain::{
    Account, DispatchTarget, Group, Limits, LogicalModel, ModelOrigin, Multiplier, MultiplierMode,
    Protocol, SchedulingWeights, UpstreamType,
};
use crate::security::url_guard;
use crate::storage::store::{AccountSecrets, ids};

// ------------------------------------------------------------------ 概览

/// 概览页数据（§6.2 的第一期子集）。
pub async fn overview(State(state): State<SharedState>, _: Admin) -> AdminResult<Json<Value>> {
    let config = state.config.current();
    let group_count = config.groups.len();
    let model_count: usize = config.groups.iter().map(|g| g.models.len()).sum();
    let target_count: usize = config
        .groups
        .iter()
        .flat_map(|g| g.models.values())
        .map(|m| m.targets.len())
        .sum();
    let listable: usize = config
        .groups
        .iter()
        .flat_map(|g| g.models.values())
        .filter(|m| m.is_listable())
        .count();

    // 倍率告警（§11.4）：宽限期内黄色，硬停或探针系统性故障红色。
    let now = crate::storage::now_unix();
    let multipliers = state.runtime.multipliers.view();
    let mut stale = Vec::new();
    let mut unknown = Vec::new();
    for group in &config.groups {
        let mut seen = std::collections::HashSet::new();
        for target in group.models.values().flat_map(|m| m.targets.iter()) {
            if !seen.insert(target.account.id.clone()) {
                continue;
            }
            let effective =
                multipliers.effective(&target.account, group.group.multiplier_limit, now);
            match effective.status {
                crate::multiplier::Status::Stale => stale.push(json!({
                    "account_id": target.account.id,
                    "name": target.account.name,
                    "stale_for": effective.stale_for,
                })),
                crate::multiplier::Status::Unknown => unknown.push(json!({
                    "account_id": target.account.id,
                    "name": target.account.name,
                })),
                crate::multiplier::Status::Known => {}
            }
        }
    }

    // 各目标的运行状态汇总，让概览一眼看出有多少目标在冷却或硬停。
    let mut status_counts = std::collections::BTreeMap::new();
    for target in config
        .groups
        .iter()
        .flat_map(|g| g.models.values())
        .flat_map(|m| m.targets.iter())
    {
        let health = state.runtime.health.target(&target.target.id);
        let account = state.runtime.health.account(&target.account.id);
        *status_counts
            .entry(health.status(&account).as_str())
            .or_insert(0usize) += 1;
    }

    // 运行指标（§6.2）：窗口内的请求量/成功率/延迟分位、队列超时、最近错误
    // 与最近配置变化。窗口固定 24 小时，响应里带上窗口长度供前端标注。
    let window_secs: i64 = 24 * 3600;
    let stats = state
        .store
        .request_stats(now - window_secs, 2000)
        .await
        .map_err(AdminError::internal)?;
    let recent_errors = state
        .store
        .recent_errors(now - window_secs, 5)
        .await
        .map_err(AdminError::internal)?;
    let recent_changes = state
        .store
        .recent_audit(5)
        .await
        .map_err(AdminError::internal)?;
    let in_flight = state.runtime.in_flight();
    let queued: u32 = config
        .groups
        .iter()
        .map(|group| {
            state
                .runtime
                .queues
                .waiting(&group.group.id, group.group.queue_capacity)
        })
        .sum();
    let mut durations = stats.durations.clone();
    durations.sort_unstable();
    let success_rate = (stats.total > 0).then(|| stats.success as f64 / stats.total as f64);

    Ok(Json(json!({
        "config_version": config.version,
        "groups": group_count,
        "logical_models": model_count,
        "listable_models": listable,
        "dispatch_targets": target_count,
        "target_status": status_counts,
        "multiplier_stale": stale,
        "multiplier_unknown": unknown,
        "probe_systemic_failure": multipliers.systemic_failure(now),
        "sticky_bindings": state.runtime.sticky.len(),
        // 已证实不存在的上游端点条数。不为零说明有账号的首选协议填错了，
        // 或者上游确实只有一条路（§14.2）。
        "missing_endpoints": state.runtime.evidence.len(std::time::Instant::now()),
        "dropped_request_records": state.recorder.dropped(),
        "master_key_from_env": state.master_key_from_env,
        // 运行指标（§6.2）。
        "window_secs": window_secs,
        "requests": stats.total,
        "success_rate": success_rate,
        "avg_latency_ms": (stats.total > 0).then(|| stats.durations.iter().sum::<i64>() / stats.total),
        "p50_latency_ms": percentile(&durations, 0.50),
        "p95_latency_ms": percentile(&durations, 0.95),
        "in_flight": in_flight,
        "queued": queued,
        "queue_timeouts": stats.queue_timeouts,
        "recent_errors": recent_errors.iter().map(|error| json!({
            "request_id": error.request_id,
            "started_at": error.started_at,
            "logical_model": error.logical_model,
            "target_id": error.target_id,
            "http_status": error.http_status,
            "error_code": error.error_code,
        })).collect::<Vec<_>>(),
        "recent_changes": recent_changes.iter().map(|entry| json!({
            "occurred_at": entry.occurred_at,
            "actor": entry.actor,
            "action": entry.action,
            "object": entry.object,
            "result": entry.result,
        })).collect::<Vec<_>>(),
    })))
}

/// 取已排序样本的百分位（最近邻，样本为空返回 None）。
fn percentile(sorted: &[i64], quantile: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted.get(index).copied()
}

/// 系统设置的可改范围；前端用它做输入校验，后端保存时再校验一次。
const SETTINGS_LIMITS: &str = r#"{
    "request_timeout_secs": {"min": 5, "max": 86400},
    "max_request_bytes": {"min": 1024, "max": 268435456},
    "retention_days": {"min": 0, "max": 3650},
    "response_state_days": {"min": 0, "max": 3650},
    "shutdown_grace_secs": {"min": 5, "max": 3600},
    "multiplier_refresh_secs": {"min": 30, "max": 86400},
    "model_sync_secs": {"min": 60, "max": 86400}
}"#;

fn settings_json(settings: &crate::app::Settings) -> Value {
    json!({
        "request_timeout_secs": settings.request_timeout.as_secs(),
        "max_request_bytes": settings.max_request_bytes,
        "retention_days": settings.retention_days,
        "response_state_days": settings.response_state_days,
        "shutdown_grace_secs": settings.shutdown_grace.as_secs(),
        "multiplier_refresh_secs": settings.multiplier_refresh.as_secs(),
        "model_sync_secs": settings.model_sync.as_secs(),
        // 内置能力目录的版本（§6.7：当前版本、模型目录版本和适配器版本）。
        "capability_catalog_revision": crate::capability::builtin().revision().to_string(),
        "version": env!("CARGO_PKG_VERSION"),
        // 这些字段在进程启动时读取一次，保存后要等下次重启才生效。
        "restart_required": ["shutdown_grace_secs"],
        "limits": serde_json::from_str::<Value>(SETTINGS_LIMITS).unwrap_or(Value::Null),
    })
}

/// 系统设置（§6.7）：读当前值、可改范围与"下次重启生效"的字段。
pub async fn get_settings(State(state): State<SharedState>, _: Admin) -> AdminResult<Json<Value>> {
    Ok(Json(settings_json(&state.settings.get())))
}

/// 保存系统设置：校验 → 持久化 → 立即热生效。
///
/// `shutdown_grace_secs` 例外：关闭宽限期在进程启动时读取，保存后等下次
/// 重启生效，响应里的 `restart_required` 会把它标出来。
pub async fn update_settings(
    State(state): State<SharedState>,
    admin: Admin,
    Json(payload): Json<crate::app::PersistedSettings>,
) -> AdminResult<Json<Value>> {
    payload
        .validate()
        .map_err(|error| AdminError::bad_request(error.to_string()))?;
    let next = payload.apply_to((*state.settings.get()).clone());
    let stored = serde_json::to_string(&crate::app::PersistedSettings::from_settings(&next))
        .map_err(AdminError::internal)?;
    state
        .store
        .set_app_setting(crate::app::SETTINGS_KEY, &stored)
        .await
        .map_err(AdminError::internal)?;
    let applied = state.settings.replace(next);
    audit(&state, &admin, "update_settings", "system").await;
    Ok(Json(settings_json(&applied)))
}

/// 修改管理员密码（§23.2）。改完吊销全部会话，并给当前浏览器发一张新的。
pub async fn change_password(
    State(state): State<SharedState>,
    admin: Admin,
    Json(payload): Json<PasswordChange>,
) -> AdminResult<Response> {
    let hash = state
        .store
        .admin_password_hash(&admin.username)
        .await
        .map_err(AdminError::internal)?
        .ok_or_else(|| AdminError::unauthorized("管理员账号不存在"))?;
    if !session::verify_password(&payload.current_password, &hash) {
        return Err(AdminError::unauthorized("当前密码不正确"));
    }
    if payload.new_password.chars().count() < 12 {
        return Err(AdminError::bad_request("新密码至少 12 个字符"));
    }
    if payload.new_password == payload.current_password {
        return Err(AdminError::bad_request("新密码不能与当前密码相同"));
    }
    let new_hash = session::hash_password(&payload.new_password).map_err(AdminError::internal)?;
    if !state
        .store
        .update_admin_password(&admin.username, &new_hash)
        .await
        .map_err(AdminError::internal)?
    {
        return Err(AdminError::unauthorized("管理员账号不存在"));
    }
    // 其他设备上的旧会话立即失效；当前浏览器换一张新会话，避免改完被踢下线。
    state.sessions.revoke_all();
    let token = state
        .sessions
        .create(&admin.username)
        .map_err(AdminError::internal)?;
    audit(&state, &admin, "change_password", &admin.username).await;
    Ok((
        [(header::SET_COOKIE, super::session_cookie_header(&token))],
        Json(json!({"username": admin.username})),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct PasswordChange {
    pub current_password: String,
    pub new_password: String,
}

// ------------------------------------------------------------------ 分组

#[derive(Deserialize)]
pub struct GroupPayload {
    pub name: String,
    pub multiplier_limit: Multiplier,
    pub weights: Option<SchedulingWeights>,
    pub queue_capacity: Option<u32>,
    /// 层内全忙时最多等多久（秒）；0=跟随请求总超时。
    pub max_wait_secs: Option<u32>,
    pub allow_degrade: Option<bool>,
}

#[derive(Deserialize)]
pub struct GroupPatch {
    pub name: Option<String>,
    pub multiplier_limit: Option<Multiplier>,
    pub weights: Option<SchedulingWeights>,
    pub queue_capacity: Option<u32>,
    pub max_wait_secs: Option<u32>,
    pub allow_degrade: Option<bool>,
}

#[derive(Serialize)]
pub struct GroupDto {
    pub id: String,
    pub name: String,
    /// 只显示前缀；完整 Key 仅在创建与重新生成时返回一次（§6.3）。
    pub key_prefix: String,
    pub multiplier_limit: Multiplier,
    pub weights: SchedulingWeights,
    pub queue_capacity: u32,
    /// 队列最长等待（秒）；0=跟随请求总超时（§6.3）。
    pub max_wait_secs: u32,
    pub allow_degrade: bool,
    pub logical_models: usize,
    pub dispatch_targets: usize,
}

fn group_dto(state: &SharedState, group: &Group) -> GroupDto {
    let config = state.config.current();
    let view = config.group_by_id(&group.id);
    GroupDto {
        id: group.id.clone(),
        name: group.name.clone(),
        key_prefix: group.key_prefix.clone(),
        multiplier_limit: group.multiplier_limit,
        weights: group.weights,
        queue_capacity: group.queue_capacity,
        max_wait_secs: group.max_wait_secs,
        allow_degrade: group.allow_degrade,
        logical_models: view.map(|v| v.models.len()).unwrap_or(0),
        dispatch_targets: view
            .map(|v| v.models.values().map(|m| m.targets.len()).sum())
            .unwrap_or(0),
    }
}

pub async fn list_groups(
    State(state): State<SharedState>,
    _: Admin,
) -> AdminResult<Json<Vec<GroupDto>>> {
    let groups = state
        .store
        .list_groups()
        .await
        .map_err(AdminError::internal)?;
    Ok(Json(groups.iter().map(|g| group_dto(&state, g)).collect()))
}

pub async fn get_group(
    State(state): State<SharedState>,
    _: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<GroupDto>> {
    let group = find_group(&state, &id).await?;
    Ok(Json(group_dto(&state, &group)))
}

/// 创建分组并签发下游 Key。Key 明文只在这一次响应中出现。
pub async fn create_group(
    State(state): State<SharedState>,
    admin: Admin,
    Json(payload): Json<GroupPayload>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let name = require_name(&payload.name, "分组名称")?;
    let weights = validate_weights(payload.weights.unwrap_or_default())?;

    let (key, prefix) = crate::security::generate_group_key().map_err(AdminError::internal)?;
    let group = Group {
        id: ids::group(),
        name,
        key_prefix: prefix,
        key_digest_hex: state.key_digest.digest_hex(&key),
        multiplier_limit: payload.multiplier_limit,
        weights,
        queue_capacity: payload.queue_capacity.unwrap_or(100),
        max_wait_secs: validate_max_wait(payload.max_wait_secs)?,
        allow_degrade: payload.allow_degrade.unwrap_or(true),
        created_at: OffsetDateTime::now_utc(),
    };

    state
        .store
        .insert_group(&group)
        .await
        .map_err(|e| conflict_or_internal(e, "分组名称已存在"))?;
    reload(&state).await?;
    audit(&state, &admin, "create_group", &group.id).await;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "group": group_dto(&state, &group),
            "key": &*key,
            "notice": "这是唯一一次显示完整 Key，请立即保存",
        })),
    ))
}

pub async fn update_group(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
    Json(patch): Json<GroupPatch>,
) -> AdminResult<Json<GroupDto>> {
    let mut group = find_group(&state, &id).await?;
    if let Some(name) = patch.name {
        group.name = require_name(&name, "分组名称")?;
    }
    if let Some(limit) = patch.multiplier_limit {
        group.multiplier_limit = limit;
    }
    if let Some(weights) = patch.weights {
        group.weights = validate_weights(weights)?;
    }
    if let Some(max_wait) = patch.max_wait_secs {
        group.max_wait_secs = validate_max_wait(Some(max_wait))?;
    }
    if let Some(capacity) = patch.queue_capacity {
        group.queue_capacity = capacity;
    }
    if let Some(allow) = patch.allow_degrade {
        group.allow_degrade = allow;
    }

    state
        .store
        .update_group(&group)
        .await
        .map_err(|e| conflict_or_internal(e, "分组名称已存在"))?;
    reload(&state).await?;
    audit(&state, &admin, "update_group", &group.id).await;
    Ok(Json(group_dto(&state, &group)))
}

pub async fn delete_group(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    if !state
        .store
        .delete_group(&id)
        .await
        .map_err(AdminError::internal)?
    {
        return Err(AdminError::not_found("分组不存在"));
    }
    reload(&state).await?;
    audit(&state, &admin, "delete_group", &id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// 重新生成下游 Key。旧 Key 在配置切换的那一刻立即失效。
pub async fn regenerate_key(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let mut group = find_group(&state, &id).await?;
    let (key, prefix) = crate::security::generate_group_key().map_err(AdminError::internal)?;
    group.key_prefix = prefix;
    group.key_digest_hex = state.key_digest.digest_hex(&key);

    state
        .store
        .update_group(&group)
        .await
        .map_err(AdminError::internal)?;
    reload(&state).await?;
    audit(&state, &admin, "regenerate_group_key", &group.id).await;

    Ok(Json(json!({
        "group": group_dto(&state, &group),
        "key": &*key,
        "notice": "旧 Key 已立即失效",
    })))
}

// ------------------------------------------------------------------ 账号

#[derive(Deserialize)]
pub struct AccountPayload {
    pub group_id: String,
    pub name: String,
    pub upstream_type: UpstreamType,
    pub base_url: String,
    pub api_key: String,
    pub preferred_protocol: Protocol,
    pub adaptive_protocol: Option<bool>,
    pub default_priority: Option<i32>,
    pub calibration: Option<Multiplier>,
    pub multiplier_mode: Option<MultiplierMode>,
    pub manual_multiplier: Option<Multiplier>,
    /// New API 探针的访问令牌，与推理用的 API Key 是两把不同的凭据（§11.2）。
    pub new_api_token: Option<String>,
    pub new_api_user_id: Option<String>,
    pub new_api_group: Option<String>,
    #[serde(default)]
    pub limits: Limits,
    pub allow_private_network: Option<bool>,
    pub enabled: Option<bool>,
    /// 模型自动同步：全量托管上游模型，忽略选择集（§16.2）。
    pub auto_sync: Option<bool>,
}

#[derive(Deserialize)]
pub struct AccountPatch {
    pub name: Option<String>,
    pub base_url: Option<String>,
    /// 只允许覆盖写入，后台不提供读取完整 Key 的接口（§23.2）。
    pub api_key: Option<String>,
    pub preferred_protocol: Option<Protocol>,
    pub adaptive_protocol: Option<bool>,
    pub default_priority: Option<i32>,
    pub calibration: Option<Multiplier>,
    pub multiplier_mode: Option<MultiplierMode>,
    pub manual_multiplier: Option<Multiplier>,
    pub new_api_token: Option<String>,
    pub new_api_user_id: Option<String>,
    pub new_api_group: Option<String>,
    pub limits: Option<Limits>,
    pub allow_private_network: Option<bool>,
    pub enabled: Option<bool>,
    pub auto_sync: Option<bool>,
}

#[derive(Serialize)]
pub struct AccountDto {
    pub id: String,
    pub group_id: String,
    pub name: String,
    pub upstream_type: UpstreamType,
    pub base_url: String,
    pub preferred_protocol: Protocol,
    pub adaptive_protocol: bool,
    pub default_priority: i32,
    pub calibration: Multiplier,
    pub multiplier_mode: MultiplierMode,
    pub manual_multiplier: Multiplier,
    /// 此刻真正生效的有效倍率，已含动态刷新结果与校准系数。
    pub effective_multiplier: Multiplier,
    /// `known` / `multiplier_stale` / `multiplier_unknown`（§12.2）。
    pub multiplier_status: &'static str,
    /// 倍率已过期多少秒，供"倍率已过期 N 分钟"提示。
    pub multiplier_stale_for: Option<i64>,
    pub multiplier_error: Option<String>,
    pub new_api_user_id: Option<String>,
    pub new_api_group: Option<String>,
    /// New API 自动倍率的提示：没填分组时按最高档保守估算（§11.2）。
    pub multiplier_note: Option<&'static str>,
    /// 是否已保存 New API 访问令牌。绝不回吐令牌本身。
    pub has_new_api_token: bool,
    /// 账号自己没填凭据、但该 Base URL 有站点级凭据（§6.4）。
    pub uses_site_credentials: bool,
    pub limits: Limits,
    pub allow_private_network: bool,
    pub enabled: bool,
    /// 模型自动同步状态。打开时后台任务全量托管（§16.2）。
    pub auto_sync: bool,
    /// 上一次模型同步完成的时间。
    pub model_synced_at: Option<i64>,
}

async fn account_dto(state: &SharedState, account: &Account, has_token: bool) -> AccountDto {
    let config = state.config.current();
    let limit = config
        .group_by_id(&account.group_id)
        .map(|group| group.group.multiplier_limit)
        .unwrap_or(Multiplier::ONE);
    let effective =
        state
            .runtime
            .multipliers
            .view()
            .effective(account, limit, crate::storage::now_unix());
    let last_error = state
        .runtime
        .multipliers
        .view()
        .entries()
        .find(|(id, _)| *id == &account.id)
        .and_then(|(_, entry)| entry.last_error.clone());
    // 账号自己没存令牌、但站点级凭据已配置：界面上要显示"使用站点凭据"。
    let uses_site_credentials = account.multiplier_mode == MultiplierMode::NewApi
        && !has_token
        && state
            .store
            .new_api_site(&account.base_url)
            .await
            .ok()
            .flatten()
            .is_some();

    AccountDto {
        id: account.id.clone(),
        group_id: account.group_id.clone(),
        name: account.name.clone(),
        upstream_type: account.upstream_type,
        base_url: account.base_url.clone(),
        preferred_protocol: account.preferred_protocol,
        adaptive_protocol: account.adaptive_protocol,
        default_priority: account.default_priority,
        calibration: account.calibration,
        multiplier_mode: account.multiplier_mode,
        manual_multiplier: account.manual_multiplier,
        effective_multiplier: effective.value,
        multiplier_status: effective.status.as_str(),
        multiplier_stale_for: effective.stale_for,
        multiplier_error: last_error,
        new_api_user_id: account.new_api_user_id.clone(),
        new_api_group: account.new_api_group.clone(),
        multiplier_note: (account.multiplier_mode == MultiplierMode::NewApi
            && account.new_api_group.is_none())
        .then_some("未填写分组：探针按可用分组的最高倍率保守估算，可能高于这把 Key 的真实档位"),
        has_new_api_token: has_token,
        uses_site_credentials,
        limits: account.limits,
        allow_private_network: account.allow_private_network,
        enabled: account.enabled,
        auto_sync: account.auto_sync,
        model_synced_at: account.model_synced_at,
    }
}

pub async fn list_accounts(
    State(state): State<SharedState>,
    _: Admin,
) -> AdminResult<Json<Vec<AccountDto>>> {
    let accounts = state
        .store
        .list_accounts()
        .await
        .map_err(AdminError::internal)?;
    let mut dtos = Vec::with_capacity(accounts.len());
    for account in &accounts {
        let has_token = state
            .store
            .account_sealed_new_api_token(&account.id)
            .await
            .map_err(AdminError::internal)?
            .is_some();
        dtos.push(account_dto(&state, account, has_token).await);
    }
    Ok(Json(dtos))
}

pub async fn create_account(
    State(state): State<SharedState>,
    admin: Admin,
    Json(payload): Json<AccountPayload>,
) -> AdminResult<(StatusCode, Json<AccountDto>)> {
    find_group(&state, &payload.group_id).await?;
    let name = require_name(&payload.name, "账号名称")?;
    if payload.api_key.trim().is_empty() {
        return Err(AdminError::bad_request("API Key 不能为空"));
    }
    let allow_private = payload.allow_private_network.unwrap_or(false);
    let base_url = validate_base_url(&payload.base_url, allow_private)?;
    let priority = validate_priority(payload.default_priority.unwrap_or(50))?;
    let mode = payload.multiplier_mode.unwrap_or(MultiplierMode::Manual);
    let token = payload
        .new_api_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let site_available = state
        .store
        .new_api_site(&base_url)
        .await
        .map_err(AdminError::internal)?
        .is_some();
    validate_multiplier_source(
        mode,
        token,
        payload.new_api_user_id.as_deref(),
        site_available,
    )?;

    let account = Account {
        id: ids::account(),
        group_id: payload.group_id,
        name,
        upstream_type: payload.upstream_type,
        base_url,
        preferred_protocol: payload.preferred_protocol,
        adaptive_protocol: payload.adaptive_protocol.unwrap_or(true),
        default_priority: priority,
        calibration: payload.calibration.unwrap_or(Multiplier::ONE),
        multiplier_mode: mode,
        manual_multiplier: payload.manual_multiplier.unwrap_or(Multiplier::ONE),
        new_api_user_id: trimmed(payload.new_api_user_id),
        new_api_group: trimmed(payload.new_api_group),
        limits: validate_limits(payload.limits)?,
        allow_private_network: allow_private,
        enabled: payload.enabled.unwrap_or(true),
        auto_sync: payload.auto_sync.unwrap_or(false),
        model_synced_at: None,
        created_at: OffsetDateTime::now_utc(),
    };

    let secrets = AccountSecrets::new(
        seal(&state, payload.api_key.trim())?,
        token.map(|t| seal(&state, t)).transpose()?,
    );
    state
        .store
        .insert_account(&account, &secrets)
        .await
        .map_err(|e| conflict_or_internal(e, "同一分组内账号名称已存在"))?;
    reload(&state).await?;
    audit(&state, &admin, "create_account", &account.id).await;

    let has_token = token.is_some();
    Ok((
        StatusCode::CREATED,
        Json(account_dto(&state, &account, has_token).await),
    ))
}

pub async fn update_account(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
    Json(patch): Json<AccountPatch>,
) -> AdminResult<Json<AccountDto>> {
    let mut account = find_account(&state, &id).await?;
    let had_token = state
        .store
        .account_sealed_new_api_token(&id)
        .await
        .map_err(AdminError::internal)?
        .is_some();

    if let Some(name) = patch.name {
        account.name = require_name(&name, "账号名称")?;
    }
    if let Some(allow) = patch.allow_private_network {
        account.allow_private_network = allow;
    }
    if let Some(base_url) = patch.base_url {
        account.base_url = validate_base_url(&base_url, account.allow_private_network)?;
    }
    if let Some(protocol) = patch.preferred_protocol {
        account.preferred_protocol = protocol;
    }
    if let Some(adaptive) = patch.adaptive_protocol {
        account.adaptive_protocol = adaptive;
    }
    if let Some(priority) = patch.default_priority {
        account.default_priority = validate_priority(priority)?;
    }
    if let Some(calibration) = patch.calibration {
        account.calibration = calibration;
    }
    if let Some(mode) = patch.multiplier_mode {
        account.multiplier_mode = mode;
    }
    if let Some(multiplier) = patch.manual_multiplier {
        account.manual_multiplier = multiplier;
    }
    if let Some(user_id) = patch.new_api_user_id {
        account.new_api_user_id = trimmed(Some(user_id));
    }
    if let Some(group) = patch.new_api_group {
        account.new_api_group = trimmed(Some(group));
    }
    if let Some(limits) = patch.limits {
        account.limits = validate_limits(limits)?;
    }
    if let Some(enabled) = patch.enabled {
        account.enabled = enabled;
    }
    // 关闭托管要收回全量目标（§16.2）：托管期间选择集标记原封未动，
    // 所以按标记调和就能回到之前勾选的模型。
    let was_managed = account.auto_sync;
    if let Some(auto_sync) = patch.auto_sync {
        account.auto_sync = auto_sync;
    }

    let token = patch
        .new_api_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let site_available = state
        .store
        .new_api_site(&account.base_url)
        .await
        .map_err(AdminError::internal)?
        .is_some();
    validate_multiplier_source(
        account.multiplier_mode,
        // 已经存过令牌的账号不必每次改配置都重填。
        token.or(had_token.then_some("已保存")),
        account.new_api_user_id.as_deref(),
        site_available,
    )?;

    let secrets = AccountSecrets {
        api_key: patch
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(|key| seal(&state, key))
            .transpose()?,
        new_api_token: token.map(|t| seal(&state, t)).transpose()?,
    };
    state
        .store
        .update_account(&account, &secrets)
        .await
        .map_err(|e| conflict_or_internal(e, "同一分组内账号名称已存在"))?;
    // 凭据换了就清掉"Key 失效"的硬停：这正是 §12.3 允许的恢复途径。
    if secrets.api_key.is_some() {
        state.runtime.health.clear_account_faults(&account.id);
    }
    reload(&state).await?;
    if was_managed && !account.auto_sync {
        discovery::unhost(&state, &account)
            .await
            .map_err(AdminError::internal)?;
    }
    audit(&state, &admin, "update_account", &account.id).await;

    Ok(Json(
        account_dto(&state, &account, had_token || token.is_some()).await,
    ))
}

/// 立即刷新一个账号的自动倍率（§11.3 的账号级刷新按钮）。
///
/// 同步等待探针结果：成功返回新的有效倍率，失败把探测错误以 502 带回前端。
pub async fn refresh_account_multiplier(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let account = find_account(&state, &id).await?;
    if !account.multiplier_mode.is_automatic() {
        return Err(AdminError::bad_request(
            "手动倍率不需要刷新，直接修改数值即可",
        ));
    }
    // 与后台定时任务共用同一条探测与落库路径，但同步等待结果：按钮点下去
    // 就是一次真实探测，而不是"排进下一轮"。
    let reading = crate::multiplier::refresh::refresh_account_now(&state, &account)
        .await
        .map_err(|error| {
            AdminError::new(
                StatusCode::BAD_GATEWAY,
                format!(
                    "倍率探测失败：{}",
                    crate::security::redact::text(&error.to_string())
                ),
            )
        })?;
    audit(&state, &admin, "refresh_multiplier", &account.id).await;
    Ok(Json(json!({
        "refreshed": true,
        "effective_multiplier": reading.multiplier,
        "observed_at": reading.observed_at,
        "notice": format!("已重新探测：当前有效倍率 {}", reading.multiplier),
    })))
}

/// 列出该账号可用的 New API 分组与倍率（§6.4 的"分组"下拉框）。
///
/// 解密访问令牌只在本进程内使用，绝不回吐；返回按倍率升序，最便宜的排前面。
pub async fn account_multiplier_groups(
    State(state): State<SharedState>,
    _: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let account = find_account(&state, &id).await?;
    if account.multiplier_mode != MultiplierMode::NewApi {
        return Err(AdminError::bad_request("只有 New API 自动倍率需要选择分组"));
    }
    // 账号凭据优先，其次站点级凭据（§6.4）。
    let context = crate::multiplier::refresh::Context {
        store: state.store.clone(),
        cipher: state.cipher.clone(),
        upstream: state.upstream.clone(),
        registry: std::sync::Arc::clone(&state.runtime.multipliers),
    };
    let Some((token, user_id)) =
        crate::multiplier::refresh::new_api_credentials(&context, &account)
            .await
            .map_err(AdminError::internal)?
    else {
        return Err(AdminError::bad_request(
            "还缺少 New API 访问令牌与用户 ID：可在本账号填写，或在设置页按站点配置一次",
        ));
    };

    let groups = crate::multiplier::probe::new_api_groups(
        &state.upstream,
        &account.base_url,
        &token,
        &user_id,
        account.allow_private_network,
    )
    .await
    .map_err(|error| {
        AdminError::new(
            StatusCode::BAD_GATEWAY,
            format!(
                "拉取分组失败：{}",
                crate::security::redact::text(&error.to_string())
            ),
        )
    })?;
    Ok(Json(json!({
        "groups": groups
            .iter()
            .map(|group| json!({
                "name": group.name,
                "ratio": group.ratio,
                "description": group.description,
            }))
            .collect::<Vec<_>>()
    })))
}

// ------------------------------------------------- New API 站点级凭据（§6.4）

#[derive(Deserialize)]
pub struct NewApiSitePayload {
    pub base_url: String,
    /// 留空表示沿用已保存的令牌；首次配置必须提供。
    pub access_token: Option<String>,
    pub user_id: String,
}

#[derive(Deserialize)]
pub struct NewApiSiteQuery {
    pub base_url: String,
}

/// 列出已配置的站点级凭据（只回吐 Base URL 与用户 ID，令牌绝不回吐）。
pub async fn list_new_api_sites(
    State(state): State<SharedState>,
    _: Admin,
) -> AdminResult<Json<Value>> {
    let sites = state
        .store
        .list_new_api_sites()
        .await
        .map_err(AdminError::internal)?;
    Ok(Json(json!({
        "sites": sites
            .iter()
            .map(|(base_url, user_id)| json!({
                "base_url": base_url,
                "user_id": user_id,
            }))
            .collect::<Vec<_>>()
    })))
}

/// 保存站点级凭据：同一 Base URL 下的账号自动继承（§6.4）。
pub async fn save_new_api_site(
    State(state): State<SharedState>,
    admin: Admin,
    Json(payload): Json<NewApiSitePayload>,
) -> AdminResult<Json<Value>> {
    let base_url = payload.base_url.trim().trim_end_matches('/').to_string();
    if base_url.is_empty() {
        return Err(AdminError::bad_request("Base URL 不能为空"));
    }
    let user_id = payload.user_id.trim().to_string();
    if user_id.is_empty() {
        return Err(AdminError::bad_request("用户 ID 不能为空"));
    }
    // 令牌留空表示保持原值；首次配置必须给一个。
    let sealed = match trimmed(payload.access_token) {
        Some(token) => seal(&state, &token)?,
        None => state
            .store
            .new_api_site(&base_url)
            .await
            .map_err(AdminError::internal)?
            .map(|(_, sealed)| sealed)
            .ok_or_else(|| AdminError::bad_request("首次配置必须提供 New API 访问令牌"))?,
    };
    state
        .store
        .upsert_new_api_site(&base_url, &user_id, &sealed)
        .await
        .map_err(AdminError::internal)?;
    audit(&state, &admin, "save_new_api_site", &base_url).await;
    Ok(Json(json!({"base_url": base_url, "user_id": user_id})))
}

/// 删除站点级凭据；已单独填过凭据的账号不受影响。
pub async fn delete_new_api_site(
    State(state): State<SharedState>,
    admin: Admin,
    Query(query): Query<NewApiSiteQuery>,
) -> AdminResult<StatusCode> {
    let base_url = query.base_url.trim().trim_end_matches('/').to_string();
    state
        .store
        .delete_new_api_site(&base_url)
        .await
        .map_err(AdminError::internal)?;
    audit(&state, &admin, "delete_new_api_site", &base_url).await;
    Ok(StatusCode::NO_CONTENT)
}

fn seal(state: &SharedState, plaintext: &str) -> AdminResult<Vec<u8>> {
    state
        .cipher
        .seal(plaintext.as_bytes())
        .map_err(AdminError::internal)
}

fn trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub async fn delete_account(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    if !state
        .store
        .delete_account(&id)
        .await
        .map_err(AdminError::internal)?
    {
        return Err(AdminError::not_found("账号不存在"));
    }
    reload(&state).await?;
    audit(&state, &admin, "delete_account", &id).await;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------- 逻辑模型

#[derive(Deserialize)]
pub struct LogicalModelPayload {
    pub group_id: String,
    pub name: String,
    pub enabled: Option<bool>,
}

#[derive(Deserialize)]
pub struct LogicalModelPatch {
    pub name: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Serialize)]
pub struct LogicalModelDto {
    pub id: String,
    pub group_id: String,
    pub name: String,
    pub origin: ModelOrigin,
    pub enabled: bool,
    pub dispatch_targets: usize,
    /// 是否会出现在 `/v1/models` 中。
    pub listed: bool,
}

/// 分组内可以从上游目录里挑的模型名（§6.5，创建逻辑模型时的下拉选择）。
pub async fn group_available_models(
    State(state): State<SharedState>,
    _: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    find_group(&state, &id).await?;
    let rows = state
        .store
        .group_available_models(&id)
        .await
        .map_err(AdminError::internal)?;
    let mut grouped: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (public_name, account_name) in rows {
        grouped.entry(public_name).or_default().push(account_name);
    }
    Ok(Json(json!({
        "models": grouped
            .into_iter()
            .map(|(public_name, accounts)| json!({
                "public_name": public_name,
                "accounts": accounts,
            }))
            .collect::<Vec<_>>()
    })))
}

pub async fn list_logical_models(
    State(state): State<SharedState>,
    _: Admin,
) -> AdminResult<Json<Vec<LogicalModelDto>>> {
    let models = state
        .store
        .list_logical_models()
        .await
        .map_err(AdminError::internal)?;
    let config = state.config.current();
    Ok(Json(
        models
            .iter()
            .map(|model| {
                let view = config
                    .group_by_id(&model.group_id)
                    .and_then(|g| g.models.get(&model.name));
                LogicalModelDto {
                    id: model.id.clone(),
                    group_id: model.group_id.clone(),
                    name: model.name.clone(),
                    origin: model.origin,
                    enabled: model.enabled,
                    dispatch_targets: view.map(|v| v.targets.len()).unwrap_or(0),
                    listed: view.map(|v| v.is_listable()).unwrap_or(false),
                }
            })
            .collect(),
    ))
}

pub async fn create_logical_model(
    State(state): State<SharedState>,
    admin: Admin,
    Json(payload): Json<LogicalModelPayload>,
) -> AdminResult<(StatusCode, Json<LogicalModelDto>)> {
    find_group(&state, &payload.group_id).await?;
    let name = require_name(&payload.name, "逻辑模型名称")?;

    let model = LogicalModel {
        id: ids::logical_model(),
        group_id: payload.group_id,
        name,
        // 后台显式创建的一律是手工模型：零目标时保留记录（§4.4）。
        origin: ModelOrigin::Manual,
        enabled: payload.enabled.unwrap_or(true),
        created_at: OffsetDateTime::now_utc(),
    };
    state
        .store
        .insert_logical_model(&model)
        .await
        .map_err(|e| conflict_or_internal(e, "同一分组内逻辑模型名称已存在"))?;
    reload(&state).await?;
    audit(&state, &admin, "create_logical_model", &model.id).await;

    Ok((
        StatusCode::CREATED,
        Json(LogicalModelDto {
            id: model.id,
            group_id: model.group_id,
            name: model.name,
            origin: model.origin,
            enabled: model.enabled,
            dispatch_targets: 0,
            listed: false,
        }),
    ))
}

pub async fn update_logical_model(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
    Json(patch): Json<LogicalModelPatch>,
) -> AdminResult<StatusCode> {
    let mut model = state
        .store
        .list_logical_models()
        .await
        .map_err(AdminError::internal)?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| AdminError::not_found("逻辑模型不存在"))?;

    if let Some(name) = patch.name {
        model.name = require_name(&name, "逻辑模型名称")?;
    }
    if let Some(enabled) = patch.enabled {
        model.enabled = enabled;
    }
    state
        .store
        .update_logical_model(&model)
        .await
        .map_err(|e| conflict_or_internal(e, "同一分组内逻辑模型名称已存在"))?;
    reload(&state).await?;
    audit(&state, &admin, "update_logical_model", &model.id).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_logical_model(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    if !state
        .store
        .delete_logical_model(&id)
        .await
        .map_err(AdminError::internal)?
    {
        return Err(AdminError::not_found("逻辑模型不存在"));
    }
    reload(&state).await?;
    audit(&state, &admin, "delete_logical_model", &id).await;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------- 调度目标

#[derive(Deserialize)]
pub struct TargetPayload {
    pub logical_model_id: String,
    pub account_id: String,
    pub upstream_model: String,
    pub priority_override: Option<i32>,
    #[serde(default)]
    pub limits: Limits,
    pub enabled: Option<bool>,
}

#[derive(Deserialize)]
pub struct TargetPatch {
    pub upstream_model: Option<String>,
    pub priority_override: Option<Option<i32>>,
    pub limits: Option<Limits>,
    pub enabled: Option<bool>,
}

#[derive(Serialize)]
pub struct TargetDto {
    pub id: String,
    pub logical_model_id: String,
    pub account_id: String,
    pub upstream_model: String,
    pub priority_override: Option<i32>,
    /// 实际生效的优先级：目标覆盖值优先于账号默认值（§9.2）。
    pub priority: i32,
    pub limits: Limits,
    /// 账号默认值与目标覆盖合并后的实际限制。
    pub effective_limits: Limits,
    pub enabled: bool,
    /// 运行状态：`active` / `cooldown` / `half_open` / …（§12.2）。
    pub status: &'static str,
    /// 冷却剩余秒数。
    pub cooldown_secs: Option<u64>,
    pub inflight: u32,
    /// 综合评分与四个分维得分。权重调错时靠这一列自我诊断（§6.9）。
    pub score: Option<ScoreDto>,
}

/// 分维得分。流式与非流式分开统计，这里给出该模型下样本更多的那一份。
#[derive(Serialize)]
pub struct ScoreDto {
    pub total: f64,
    pub multiplier: f64,
    pub reliability: f64,
    pub first_token: f64,
    pub throughput: f64,
    pub samples: u64,
    /// 样本不足 20 时性能三维用的是保守中性分（§9.4）。
    pub warm: bool,
}

fn target_dto(state: &SharedState, target: &DispatchTarget) -> TargetDto {
    let config = state.config.current();
    let view = config
        .groups
        .iter()
        .flat_map(|group| group.models.values())
        .flat_map(|model| model.targets.iter())
        .find(|candidate| candidate.target.id == target.id);

    let (priority, effective_limits) = view
        .map(|view| (view.priority, view.limits()))
        .unwrap_or((50, target.limits));
    let health = state.runtime.health.target(&target.id);
    let account = state.runtime.health.account(&target.account_id);

    TargetDto {
        id: target.id.clone(),
        logical_model_id: target.logical_model_id.clone(),
        account_id: target.account_id.clone(),
        upstream_model: target.upstream_model.clone(),
        priority_override: target.priority_override,
        priority,
        limits: target.limits,
        effective_limits,
        enabled: target.enabled,
        status: health.status(&account).as_str(),
        cooldown_secs: health
            .cooldown_remaining(tokio::time::Instant::now())
            .map(|d| d.as_secs()),
        inflight: health.inflight(),
        score: view.and_then(|view| score_dto(state, view)),
    }
}

/// 用与调度完全相同的算法算出这个目标当前的综合评分。
///
/// 后台看到的分数必须和调度器用的是同一个数字，否则诊断毫无意义。
fn score_dto(state: &SharedState, view: &crate::config::TargetView) -> Option<ScoreDto> {
    let config = state.config.current();
    let group = config.group_by_id(&view.account.group_id)?;
    let multipliers = state.runtime.multipliers.view();
    let now = crate::storage::now_unix();

    let model = group
        .models
        .values()
        .find(|model| model.model.id == view.target.logical_model_id)?;
    // 性能统计按"协议 + 是否流式"分开。跨协议之后一个目标可能同时服务三种
    // 下游协议，展示时取这个模型下样本最多的那一维，且整个模型统一用它——
    // 否则参照系不一致，分数就不可比了。
    let samples_of = |dimension: crate::routing::score::Dimension| -> u64 {
        model
            .targets
            .iter()
            .map(|target| {
                state
                    .runtime
                    .perf
                    .stats(&target.target.id, dimension)
                    .samples
            })
            .sum()
    };
    let dimension = [
        Protocol::OpenAiChat,
        Protocol::OpenAiResponses,
        Protocol::AnthropicMessages,
    ]
    .into_iter()
    .flat_map(|protocol| {
        [true, false].map(|streaming| crate::routing::score::Dimension {
            protocol,
            streaming,
        })
    })
    .max_by_key(|dimension| samples_of(*dimension))
    .unwrap_or(crate::routing::score::Dimension {
        protocol: view.account.preferred_protocol,
        streaming: false,
    });
    let candidates: Vec<_> = model
        .targets
        .iter()
        .map(|target| {
            let effective =
                multipliers.effective(&target.account, group.group.multiplier_limit, now);
            crate::routing::score::Candidate {
                target_id: target.target.id.clone(),
                multiplier: effective.value,
                multiplier_stale: effective.status == crate::multiplier::Status::Stale,
                stats: state.runtime.perf.stats(&target.target.id, dimension),
            }
        })
        .collect();
    let cheapest = group
        .models
        .values()
        .flat_map(|model| model.targets.iter())
        .map(|target| {
            multipliers
                .effective(&target.account, group.group.multiplier_limit, now)
                .value
        })
        .min();

    let scores = crate::routing::score::score_all(&candidates, group.group.weights, cheapest);
    let index = model
        .targets
        .iter()
        .position(|target| target.target.id == view.target.id)?;
    let score = scores.get(index)?;
    let stats = state.runtime.perf.stats(&view.target.id, dimension);
    Some(ScoreDto {
        total: round4(score.total),
        multiplier: round4(score.multiplier),
        reliability: round4(score.reliability),
        first_token: round4(score.first_token),
        throughput: round4(score.throughput),
        samples: stats.samples,
        warm: stats.is_warm(),
    })
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

pub async fn list_targets(
    State(state): State<SharedState>,
    _: Admin,
) -> AdminResult<Json<Vec<TargetDto>>> {
    let targets = state
        .store
        .list_targets()
        .await
        .map_err(AdminError::internal)?;
    Ok(Json(
        targets.iter().map(|t| target_dto(&state, t)).collect(),
    ))
}

pub async fn create_target(
    State(state): State<SharedState>,
    admin: Admin,
    Json(payload): Json<TargetPayload>,
) -> AdminResult<(StatusCode, Json<TargetDto>)> {
    let model = state
        .store
        .list_logical_models()
        .await
        .map_err(AdminError::internal)?
        .into_iter()
        .find(|m| m.id == payload.logical_model_id)
        .ok_or_else(|| AdminError::not_found("逻辑模型不存在"))?;
    let account = find_account(&state, &payload.account_id).await?;

    // 分组是调度硬边界：跨组绑定必须在写入时就被拒绝（§4.1）。
    if account.group_id != model.group_id {
        return Err(AdminError::bad_request(
            "账号与逻辑模型不在同一分组，不允许跨分组绑定",
        ));
    }
    if payload.upstream_model.trim().is_empty() {
        return Err(AdminError::bad_request("上游模型名不能为空"));
    }
    if let Some(priority) = payload.priority_override {
        validate_priority(priority)?;
    }

    let target = DispatchTarget {
        id: ids::target(),
        logical_model_id: model.id,
        account_id: account.id,
        upstream_model: payload.upstream_model.trim().to_string(),
        priority_override: payload.priority_override,
        limits: validate_limits(payload.limits)?,
        enabled: payload.enabled.unwrap_or(true),
        created_at: OffsetDateTime::now_utc(),
    };
    state
        .store
        .insert_target(&target)
        .await
        .map_err(|e| conflict_or_internal(e, "该账号与模型的组合已经是调度目标"))?;
    reload(&state).await?;
    audit(&state, &admin, "create_target", &target.id).await;

    Ok((StatusCode::CREATED, Json(target_dto(&state, &target))))
}

pub async fn update_target(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
    Json(patch): Json<TargetPatch>,
) -> AdminResult<Json<TargetDto>> {
    let mut target = state
        .store
        .list_targets()
        .await
        .map_err(AdminError::internal)?
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| AdminError::not_found("调度目标不存在"))?;

    if let Some(model) = patch.upstream_model {
        if model.trim().is_empty() {
            return Err(AdminError::bad_request("上游模型名不能为空"));
        }
        target.upstream_model = model.trim().to_string();
    }
    if let Some(priority) = patch.priority_override {
        if let Some(value) = priority {
            validate_priority(value)?;
        }
        target.priority_override = priority;
    }
    if let Some(limits) = patch.limits {
        target.limits = validate_limits(limits)?;
    }
    if let Some(enabled) = patch.enabled {
        target.enabled = enabled;
    }

    state
        .store
        .update_target(&target)
        .await
        .map_err(AdminError::internal)?;
    reload(&state).await?;
    audit(&state, &admin, "update_target", &target.id).await;
    Ok(Json(target_dto(&state, &target)))
}

pub async fn delete_target(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    if !state
        .store
        .delete_target(&id)
        .await
        .map_err(AdminError::internal)?
    {
        return Err(AdminError::not_found("调度目标不存在"));
    }
    reload(&state).await?;
    audit(&state, &admin, "delete_target", &id).await;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------- 模型目录与选择集（§16）

/// 账号模型目录里的一行，供勾选对话框展示（§16.2）。
#[derive(Serialize)]
pub struct AccountModelDto {
    pub upstream_model: String,
    pub public_name: String,
    pub selected: bool,
    pub missing: bool,
    /// 仅"获取模型"响应里有意义：本次拉取新出现的模型。
    pub is_new: bool,
}

fn catalog_entry_dto(entry: &discovery::CatalogEntry) -> AccountModelDto {
    AccountModelDto {
        upstream_model: entry.upstream_model.clone(),
        public_name: entry.public_name.clone(),
        selected: entry.selected,
        missing: entry.missing,
        is_new: entry.is_new,
    }
}

/// 当前模型目录。不请求上游，供重新打开对话框或展示选择集使用。
pub async fn list_account_models(
    State(state): State<SharedState>,
    _: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<Vec<AccountModelDto>>> {
    find_account(&state, &id).await?;
    let rows = state
        .store
        .list_account_models(&id)
        .await
        .map_err(AdminError::internal)?;
    Ok(Json(
        rows.iter()
            .map(|row| AccountModelDto {
                upstream_model: row.upstream_model.clone(),
                public_name: row.public_name.clone(),
                selected: row.selected,
                missing: row.missing,
                is_new: false,
            })
            .collect(),
    ))
}

/// 拉取上游模型列表，应用别名并与选择集合并（§16.1）。
///
/// 拉取报错、格式异常或 0 个有效模型时保留原目录，返回 502 交由前端
/// 展示错误且不弹对话框。
pub async fn refresh_account_models(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<Vec<AccountModelDto>>> {
    let account = find_account(&state, &id).await?;
    let entries = discovery::refresh_catalog(&state, &account)
        .await
        .map_err(|error| {
            AdminError::new(
                StatusCode::BAD_GATEWAY,
                format!("拉取模型列表失败：{error:#}"),
            )
        })?;
    audit(&state, &admin, "refresh_account_models", &id).await;
    Ok(Json(entries.iter().map(catalog_entry_dto).collect()))
}

#[derive(Deserialize)]
pub struct SelectionPayload {
    /// 期望处于选择集内的上游模型名全集。
    pub selected: Vec<String>,
    /// 最近 24 小时有流量的模型被取消勾选时需要二次确认（§16.3）。
    #[serde(default)]
    pub force: bool,
}

/// 批量应用选择集：勾选自动生成/归并调度目标，取消勾选移除目标（§16.3）。
pub async fn select_account_models(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
    Json(payload): Json<SelectionPayload>,
) -> AdminResult<Response> {
    let account = find_account(&state, &id).await?;
    // 托管中全部模型已在调度，选择集被忽略（§16.2）；改勾选只会误导管理员。
    if account.auto_sync {
        return Err(AdminError::conflict(
            "该账号已开启模型自动同步，勾选对话框不可用；如需手动选择请先关闭自动同步",
        ));
    }
    let desired: HashSet<String> = payload
        .selected
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();

    match discovery::apply_selection(&state, &account, &desired, payload.force)
        .await
        .map_err(AdminError::internal)?
    {
        discovery::Selection::NeedsConfirm(warnings) => Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "以下模型最近 24 小时有流量，取消勾选前请确认",
                "warnings": warnings
                    .iter()
                    .map(|w| json!({"public_name": w.public_name, "calls": w.calls}))
                    .collect::<Vec<_>>(),
            })),
        )
            .into_response()),
        discovery::Selection::Applied(outcome) => {
            audit(&state, &admin, "select_account_models", &id).await;
            Ok(Json(json!({
                "created_targets": outcome.created,
                "removed_targets": outcome.removed,
            }))
            .into_response())
        }
    }
}

/// 立即执行一轮托管同步（§16.2）：全部上游模型纳入调度，忽略选择集。
pub async fn sync_account_models(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let account = find_account(&state, &id).await?;
    if !account.auto_sync {
        return Err(AdminError::bad_request(
            "该账号未开启模型自动同步，请用“获取模型”和勾选对话框管理",
        ));
    }
    let managed = discovery::sync_managed(&state, &account)
        .await
        .map_err(|error| {
            AdminError::new(StatusCode::BAD_GATEWAY, format!("模型同步失败：{error:#}"))
        })?;
    audit(&state, &admin, "sync_account_models", &id).await;
    Ok(Json(json!({ "managed_models": managed })))
}

#[derive(Deserialize)]
pub struct ManualModelPayload {
    pub upstream_model: String,
    /// 手动指定的对外名；与上游名不同时会一并写入别名表。
    pub public_name: Option<String>,
}

/// 手动输入上游模型名并纳入调度（§16.5：模型列表接口不可用时）。
pub async fn add_manual_model(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
    Json(payload): Json<ManualModelPayload>,
) -> AdminResult<StatusCode> {
    let account = find_account(&state, &id).await?;
    discovery::add_manual_model(
        &state,
        &account,
        &payload.upstream_model,
        payload.public_name.as_deref(),
    )
    .await
    .map_err(|error| AdminError::bad_request(format!("{error:#}")))?;
    audit(&state, &admin, "add_manual_model", &id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, Deserialize)]
pub struct AliasDto {
    pub upstream_model: String,
    pub public_name: String,
}

#[derive(Deserialize)]
pub struct AliasPayload {
    pub aliases: Vec<AliasDto>,
}

pub async fn list_aliases(
    State(state): State<SharedState>,
    _: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<Vec<AliasDto>>> {
    find_account(&state, &id).await?;
    let aliases = state
        .store
        .list_account_aliases(&id)
        .await
        .map_err(AdminError::internal)?;
    Ok(Json(
        aliases
            .into_iter()
            .map(|a| AliasDto {
                upstream_model: a.upstream_model,
                public_name: a.public_name,
            })
            .collect(),
    ))
}

/// 覆盖写入账号的模型别名表（§16.4）。别名在下一次"获取模型"时生效。
pub async fn update_aliases(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
    Json(payload): Json<AliasPayload>,
) -> AdminResult<StatusCode> {
    find_account(&state, &id).await?;
    let mut aliases = Vec::with_capacity(payload.aliases.len());
    let mut seen = HashSet::new();
    for alias in payload.aliases {
        let upstream = alias.upstream_model.trim();
        let public = alias.public_name.trim();
        if upstream.is_empty() || public.is_empty() {
            return Err(AdminError::bad_request("别名两端都不能为空"));
        }
        if upstream.chars().count() > 256 || public.chars().count() > 100 {
            return Err(AdminError::bad_request("别名名称过长"));
        }
        if !seen.insert(upstream.to_string()) {
            return Err(AdminError::bad_request(format!(
                "上游模型 {upstream} 出现了重复别名"
            )));
        }
        aliases.push(crate::storage::store::AccountAliasRow {
            upstream_model: upstream.to_string(),
            public_name: public.to_string(),
        });
    }
    state
        .store
        .replace_account_aliases(&id, &aliases)
        .await
        .map_err(AdminError::internal)?;
    audit(&state, &admin, "update_aliases", &id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// 一键独立复制账号（§6.4）。
///
/// 直接创建一个停用状态的"名称 - 副本"：复制静态配置、模型选择集、模型
/// 别名与调度目标，Key 在内存解密后用新 nonce 重新加密（两条记录绝不引用
/// 同一个密文），健康、粘性、性能与自动倍率状态因新 ID 自然归零。
pub async fn copy_account(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
) -> AdminResult<(StatusCode, Json<AccountDto>)> {
    let account = find_account(&state, &id).await?;
    let sealed_key = state
        .store
        .account_sealed_key(&id)
        .await
        .map_err(AdminError::internal)?
        .ok_or_else(|| AdminError::bad_request("账号没有已保存的 API Key，无法复制"))?;
    let sealed_token = state
        .store
        .account_sealed_new_api_token(&id)
        .await
        .map_err(AdminError::internal)?;

    let copy = Account {
        id: ids::account(),
        group_id: account.group_id.clone(),
        name: format!("{} - 副本", account.name),
        // 静态配置原样复制；停用状态让管理员检查完再启用（§6.4）。
        upstream_type: account.upstream_type,
        base_url: account.base_url.clone(),
        preferred_protocol: account.preferred_protocol,
        adaptive_protocol: account.adaptive_protocol,
        default_priority: account.default_priority,
        calibration: account.calibration,
        multiplier_mode: account.multiplier_mode,
        manual_multiplier: account.manual_multiplier,
        new_api_user_id: account.new_api_user_id.clone(),
        new_api_group: account.new_api_group.clone(),
        limits: account.limits,
        allow_private_network: account.allow_private_network,
        enabled: false,
        auto_sync: account.auto_sync,
        model_synced_at: None,
        created_at: OffsetDateTime::now_utc(),
    };
    // 重新加密：`Cipher::open` 每次返回独立的明文，`seal` 每次取新 nonce。
    // 重新加密：`Cipher::open` 每次返回独立的明文，`seal` 每次取新 nonce，
    // 两条记录绝不共享密文（§20.4）。
    let key = state
        .cipher
        .open(&sealed_key)
        .map_err(AdminError::internal)?;
    let token = match sealed_token {
        Some(sealed) => Some(state.cipher.open(&sealed).map_err(AdminError::internal)?),
        None => None,
    };
    let has_token = token.is_some();
    let secrets = AccountSecrets::new(
        state
            .cipher
            .seal(key.as_ref())
            .map_err(AdminError::internal)?,
        token
            .map(|t| state.cipher.seal(t.as_ref()))
            .transpose()
            .map_err(AdminError::internal)?,
    );

    state
        .store
        .insert_account(&copy, &secrets)
        .await
        .map_err(|e| conflict_or_internal(e, "同一分组内账号名称已存在"))?;

    // 选择集、别名与调度目标随账号独立一份（§6.4：所有配置独立，Key 和状态
    // 不共享）。
    let catalog = state
        .store
        .list_account_models(&id)
        .await
        .map_err(AdminError::internal)?;
    if !catalog.is_empty() {
        state
            .store
            .replace_account_models(&copy.id, &catalog)
            .await
            .map_err(AdminError::internal)?;
    }
    let aliases = state
        .store
        .list_account_aliases(&id)
        .await
        .map_err(AdminError::internal)?;
    if !aliases.is_empty() {
        state
            .store
            .replace_account_aliases(&copy.id, &aliases)
            .await
            .map_err(AdminError::internal)?;
    }
    let targets: Vec<DispatchTarget> = state
        .store
        .list_targets()
        .await
        .map_err(AdminError::internal)?
        .into_iter()
        .filter(|t| t.account_id == id)
        .map(|t| DispatchTarget {
            id: ids::target(),
            account_id: copy.id.clone(),
            ..t
        })
        .collect();
    for target in &targets {
        state
            .store
            .insert_target(target)
            .await
            .map_err(AdminError::internal)?;
    }

    reload(&state).await?;
    audit(&state, &admin, "copy_account", &copy.id).await;
    Ok((
        StatusCode::CREATED,
        Json(account_dto(&state, &copy, has_token).await),
    ))
}

#[derive(Deserialize)]
pub struct TestAccountPayload {
    /// 用哪个上游模型发测试请求。留空时取选择集里第一个已选模型。
    pub model: Option<String>,
}

/// 测试连接（§6.4、§7）：发送一次真实的 `hi`。
///
/// 测试数据不参与自适应、粘性与正常重试统计：走独立的请求路径，不写
/// 端点证据、健康状态或评分，只报告能不能通。
pub async fn test_account(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
    Json(payload): Json<TestAccountPayload>,
) -> AdminResult<Json<Value>> {
    let account = find_account(&state, &id).await?;
    // 模型名：显式指定优先；否则取选择集里第一个已选且未消失的模型。
    let model = match payload
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        Some(model) => model.to_string(),
        None => {
            state
                .store
                .list_account_models(&id)
                .await
                .map_err(AdminError::internal)?
                .into_iter()
                .find(|row| row.selected && !row.missing)
                .ok_or_else(|| {
                    AdminError::bad_request(
                        "账号还没有已选模型；先用「获取模型」勾选，或在请求里指定 model",
                    )
                })?
                .upstream_model
        }
    };

    let sealed = state
        .store
        .account_sealed_key(&id)
        .await
        .map_err(AdminError::internal)?
        .ok_or_else(|| AdminError::bad_request("账号没有已保存的 API Key"))?;
    let key = state.cipher.open(&sealed).map_err(AdminError::internal)?;
    let api_key = String::from_utf8_lossy(&key).into_owned();

    let endpoint = crate::upstream::Endpoint::native(account.preferred_protocol);
    let url = crate::upstream::build_url(&account.base_url, endpoint)
        .map_err(|error| AdminError::bad_request(format!("Base URL 无法构造端点：{error}")))?;
    // 三个协议的最小合法请求体恰好同形：一句话加 max_tokens。
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 8,
    });

    let headers = crate::upstream::headers_for_protocol(account.preferred_protocol, &api_key)
        .map_err(|error| AdminError::bad_request(format!("请求头构造失败：{error}")))?;
    let started = std::time::Instant::now();
    let outcome = state
        .upstream
        .http_for(account.allow_private_network)
        .post(url)
        .headers(headers)
        .timeout(std::time::Duration::from_secs(20))
        .json(&body)
        .send()
        .await;
    let latency_ms = started.elapsed().as_millis() as i64;

    // 网络层失败也返回结构化结果，让前端能展示而不是抛 500。
    let result = match outcome {
        Ok(response) => {
            let status = response.status().as_u16();
            let ok = response.status().is_success();
            let text = response.text().await.unwrap_or_default();
            let message = if ok {
                format!("测试通过（{latency_ms} ms）")
            } else {
                format!(
                    "上游返回 {status}：{}",
                    crate::gateway::passthrough::upstream_error_message(text.as_bytes())
                        .unwrap_or_else(|| format!("状态码 {status}"))
                )
            };
            json!({
                "ok": ok,
                "status": status,
                "latency_ms": latency_ms,
                "model": model,
                "message": message,
            })
        }
        Err(error) => json!({
            "ok": false,
            "status": 0,
            "latency_ms": latency_ms,
            "model": model,
            "message": format!("连接失败：{error}"),
        }),
    };

    audit(&state, &admin, "test_account", &id).await;
    Ok(Json(result))
}

// ------------------------------------------------- 成本页与校准（§6.8）

/// 成本统计区间（§6.7 设置页口径之外的简单约定）：
/// `day` 当天 / `month` 本月。成本页不提供更细的自定义区间。
#[derive(Deserialize)]
pub struct CostQuery {
    pub period: Option<String>,
}

/// 成本页数据（§6.8）：以逻辑模型为单位组织，绝不跨模型加总。
///
/// 倍率是折扣不是价格：`倍率 × token` 只在同一逻辑模型内部与真实花销成正比。
/// 全局区域只显示不失真的量——请求总数与各账号占比。
pub async fn cost(
    State(state): State<SharedState>,
    _: Admin,
    Query(query): Query<CostQuery>,
) -> AdminResult<Json<Value>> {
    let now = crate::storage::now_unix();
    let since = match query.period.as_deref() {
        Some("month") => {
            // 本月起点：以 UTC 0 点为界，粗粒度即可，成本页是诊断视图不是账单。
            let days = OffsetDateTime::from_unix_timestamp(now)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH)
                .day();
            now - (i64::from(days) - 1) * 86_400
        }
        // 默认按天。
        _ => now - 86_400,
    };

    let usage = state
        .store
        .cost_usage(since)
        .await
        .map_err(AdminError::internal)?;
    let samples = state
        .store
        .cost_multiplier_samples(since)
        .await
        .map_err(AdminError::internal)?;
    let config = state.config.current();

    // 账号名与有效倍率全部从当前配置解析；已删除的账号行保留计数但标 unknown。
    let mut accounts: HashMap<&str, &Account> = HashMap::new();
    for group in &config.groups {
        for model in group.models.values() {
            for target in &model.targets {
                accounts
                    .entry(target.account.id.as_str())
                    .or_insert_with(|| target.account.as_ref());
            }
        }
    }

    // (group, model) → per-model 视图。请求级倍率样本先折叠成"按请求次数
    // 加权的平均倍率"，这是 §6.8 要求的口径。
    #[derive(Default)]
    struct ModelCost {
        requests: i64,
        tokens: i64,
        per_account: Vec<(String, String, i64, i64, Option<Multiplier>)>,
        weighted_sum: i128,
        weighted_count: i64,
        cheapest: Option<Multiplier>,
        dearest: Option<Multiplier>,
    }
    let mut models: std::collections::BTreeMap<(String, String), ModelCost> =
        std::collections::BTreeMap::new();

    for row in &usage {
        let entry = models
            .entry((row.group_id.clone(), row.logical_model.clone()))
            .or_default();
        entry.requests += row.requests;
        entry.tokens += row.tokens;
        let (name, multiplier) = match accounts.get(row.account_id.as_str()) {
            Some(account) => {
                let limit = config
                    .group_by_id(&row.group_id)
                    .map(|g| g.group.multiplier_limit)
                    .unwrap_or(Multiplier::ONE);
                let effective = state
                    .runtime
                    .multipliers
                    .view()
                    .effective(account, limit, now)
                    .value;
                (account.name.clone(), Some(effective))
            }
            None => ("（已删除账号）".to_string(), None),
        };
        entry.per_account.push((
            name,
            row.account_id.clone(),
            row.requests,
            row.tokens,
            multiplier,
        ));
    }

    for sample in &samples {
        let entry = models
            .entry((sample.group_id.clone(), sample.logical_model.clone()))
            .or_default();
        let weighted = (sample.effective_multiplier.raw() as i128) * (sample.requests as i128);
        entry.weighted_sum += weighted;
        entry.weighted_count += sample.requests;
        let some = Some(sample.effective_multiplier);
        entry.cheapest = entry.cheapest.min(some).or(some);
        entry.dearest = entry.dearest.max(some).or(some);
    }

    let model_views: Vec<Value> = models
        .into_iter()
        .map(|((group_id, name), cost)| {
            let mut cost = cost;
            // 有 Token 就按 Token 排、按 Token 算占比；整段区间都没有 Token
            // 时退化为请求数口径，绝不虚构 token 数（§6.8）。
            let tokens_available = cost.tokens > 0;
            cost.per_account.sort_by_key(
                |entry| {
                    if tokens_available { -entry.3 } else { -entry.2 }
                },
            );
            let total = if tokens_available {
                cost.tokens.max(1)
            } else {
                cost.requests.max(1)
            };
            // 加权均倍率：请求级样本按次数平均，四舍五入回定点域（展示用）。
            let weighted_avg = (cost.weighted_count > 0).then(|| {
                let scaled = (cost.weighted_sum + cost.weighted_count as i128 / 2)
                    / cost.weighted_count as i128;
                Multiplier::from_raw(scaled as i64)
            });
            let saving = match (weighted_avg, cost.cheapest) {
                (Some(avg), Some(cheapest)) if avg.raw() > 0 && avg > cheapest => {
                    // 全用最便宜目标还能再省多少（§6.8：自我恭维的对立面）。
                    Some(round4(1.0 - cheapest.to_f64() / avg.to_f64()))
                }
                _ => None,
            };
            json!({
                "group_id": group_id,
                "logical_model": name,
                "requests": cost.requests,
                "tokens": cost.tokens,
                "share_basis": if tokens_available { "tokens" } else { "requests" },
                "accounts": cost.per_account.iter().map(|(name, id, requests, tokens, multiplier)| {
                    let basis = if tokens_available { *tokens as f64 } else { *requests as f64 };
                    json!({
                        "account_id": id,
                        "name": name,
                        "requests": requests,
                        "tokens": tokens,
                        "share": round4(basis / total as f64),
                        "effective_multiplier": multiplier,
                    })
                }).collect::<Vec<_>>(),
                "weighted_avg_multiplier": weighted_avg,
                "cheapest_multiplier": cost.cheapest,
                "dearest_multiplier": cost.dearest,
                // 只用它会更省，但没有备份——省的比例必须看得见（§6.8）。
                "saving_vs_cheapest": saving,
                "single_target": cost.per_account.len() == 1,
            })
        })
        .collect();

    // 全局区域：只显示不失真的量（§6.8）。没有任何跨模型的"总成本"。
    let total_requests: i64 = usage.iter().map(|r| r.requests).sum();
    let total_tokens: i64 = usage.iter().map(|r| r.tokens).sum();
    let tokens_available = total_tokens > 0;
    let mut by_account: std::collections::BTreeMap<String, (String, i64, i64)> =
        std::collections::BTreeMap::new();
    for row in &usage {
        let name = accounts
            .get(row.account_id.as_str())
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "（已删除账号）".to_string());
        let entry = by_account
            .entry(row.account_id.clone())
            .or_insert((name, 0, 0));
        entry.1 += row.requests;
        entry.2 += row.tokens;
    }
    let account_shares: Vec<Value> = by_account
        .into_iter()
        .map(|(id, (name, requests, tokens))| {
            json!({
                "account_id": id,
                "name": name,
                "requests": requests,
                "tokens": tokens,
                "share": round4(if tokens_available {
                    tokens as f64 / total_tokens.max(1) as f64
                } else {
                    requests as f64 / total_requests.max(1) as f64
                }),
            })
        })
        .collect();

    Ok(Json(json!({
        "period": query.period.as_deref().unwrap_or("day"),
        "since": since,
        "total_requests": total_requests,
        "total_tokens": total_tokens,
        // 占比口径：区间内只要有任何一条记录带 Token 就按 Token，否则按请求数。
        "share_basis": if tokens_available { "tokens" } else { "requests" },
        "account_shares": account_shares,
        "models": model_views,
    })))
}

#[derive(Deserialize)]
pub struct CalibratePayload {
    pub logical_model: String,
    /// 站点后台报的该模型"倍率"或折算账面值，例如站点倍率 0.83。
    pub reported: Multiplier,
    /// 对账区间起点（unix 秒）。留空时取最近 30 天。
    pub period_start: Option<i64>,
}

/// 校准助手（§6.8）：按单模型对账，反算校准系数。
///
/// 公式：`校准系数 = 站点报的倍率 ÷ 网关侧该模型该账号的加权均倍率`。
/// 必须按单个模型对账——总用量对账会随模型组合漂移。
pub async fn calibrate_account(
    State(state): State<SharedState>,
    admin: Admin,
    Path(id): Path<String>,
    Json(payload): Json<CalibratePayload>,
) -> AdminResult<Json<Value>> {
    let account = find_account(&state, &id).await?;
    let now = crate::storage::now_unix();
    let since = payload.period_start.unwrap_or(now - 30 * 86_400).max(0);
    if since >= now {
        return Err(AdminError::bad_request("对账区间起点必须在过去"));
    }

    let samples = state
        .store
        .cost_multiplier_samples(since)
        .await
        .map_err(AdminError::internal)?;
    // 只取该账号所在分组、该逻辑模型的样本；这些样本是"账号全组"的，还要
    // 按该账号的流量占比折算。样本表没有账号维度，所以这里再查 usage。
    let usage = state
        .store
        .cost_usage(since)
        .await
        .map_err(AdminError::internal)?;

    let group_id = &account.group_id;
    let model = &payload.logical_model;
    let model_samples: Vec<_> = samples
        .iter()
        .filter(|s| &s.group_id == group_id && &s.logical_model == model)
        .collect();
    let model_requests: i64 = model_samples.iter().map(|s| s.requests).sum();
    let account_requests: i64 = usage
        .iter()
        .filter(|u| &u.group_id == group_id && &u.logical_model == model && u.account_id == id)
        .map(|u| u.requests)
        .sum();
    let total_requests: i64 = usage
        .iter()
        .filter(|u| &u.group_id == group_id && &u.logical_model == model)
        .map(|u| u.requests)
        .sum();

    if model_requests == 0 {
        return Err(AdminError::bad_request(format!(
            "模型 {model} 在对账区间内没有成功请求，无法校准"
        )));
    }

    // 全组加权均倍率。校准系数是账号级属性，但请求级样本不带账号维度；
    // 单账号独占模型时就是精确值，多账号共享时给出近似并明确提示。
    let weighted_sum: i128 = model_samples
        .iter()
        .map(|s| s.effective_multiplier.raw() as i128 * s.requests as i128)
        .sum();
    let group_avg = Multiplier::from_raw(
        ((weighted_sum + model_requests as i128 / 2) / model_requests as i128) as i64,
    );
    let exclusive = account_requests == total_requests;

    // 校准系数 = 站点报的倍率 ÷ 全组加权均倍率。
    if group_avg.raw() == 0 {
        return Err(AdminError::bad_request("加权均倍率为 0，无法反算"));
    }
    let calibration = Multiplier::from_raw(
        ((payload.reported.raw() as i128 * 1_000_000) / group_avg.raw() as i128)
            .min(i64::MAX as i128) as i64,
    );
    if calibration.raw() <= 0 || calibration.to_f64() > 100.0 {
        return Err(AdminError::bad_request(format!(
            "反算出的校准系数 {} 超出合理范围，请核对站点报的倍率",
            calibration
        )));
    }

    let record = crate::storage::store::CalibrationRecord {
        id: ids::calibration(),
        account_id: id.clone(),
        logical_model: model.clone(),
        period_start: since,
        period_end: now,
        gateway_requests: account_requests,
        reported: payload.reported.to_string(),
        calibration: calibration.to_string(),
        created_at: now,
    };
    state
        .store
        .insert_calibration_record(&record)
        .await
        .map_err(AdminError::internal)?;
    audit(&state, &admin, "calibrate_account", &id).await;

    Ok(Json(json!({
        "calibration": calibration.to_string(),
        "reported": payload.reported.to_string(),
        "group_avg_multiplier": group_avg.to_string(),
        "gateway_requests": account_requests,
        "model_requests": model_requests,
        // 多账号共享模型时全组均倍率 ≠ 本账号真实倍率，提醒人工判断。
        "exclusive": exclusive,
        "notice": if exclusive {
            "该模型对账区间内只有这个账号有流量，系数是精确值"
        } else {
            "该模型对账区间内有多个账号分担流量，系数是按全组均倍率的近似值"
        },
    })))
}

/// 一个账号的最近校准记录。
pub async fn list_calibrations(
    State(state): State<SharedState>,
    _: Admin,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    find_account(&state, &id).await?;
    let records = state
        .store
        .list_calibration_records(&id, 20)
        .await
        .map_err(AdminError::internal)?;
    Ok(Json(json!({
        "data": records.iter().map(|r| json!({
            "id": r.id,
            "logical_model": r.logical_model,
            "period_start": r.period_start,
            "period_end": r.period_end,
            "gateway_requests": r.gateway_requests,
            "reported": r.reported,
            "calibration": r.calibration,
            "created_at": r.created_at,
        })).collect::<Vec<_>>(),
    })))
}

// ------------------------------------------------- 备份与恢复（§23.5）

#[derive(Deserialize)]
pub struct BackupExportPayload {
    /// 备份密码。Argon2id 派生密钥用；绝不写入日志或审计。
    pub password: String,
}

/// 导出加密配置备份。响应体就是备份文件本身。
pub async fn export_backup(
    State(state): State<SharedState>,
    admin: Admin,
    Json(payload): Json<BackupExportPayload>,
) -> AdminResult<Response> {
    let bytes =
        crate::security::backup::export_backup(&state.store, &state.cipher, &payload.password)
            .await
            .map_err(|error| AdminError::bad_request(format!("{error:#}")))?;
    audit(&state, &admin, "backup_export", "config").await;
    let filename = format!(
        "akhub-backup-{}.json",
        time::OffsetDateTime::now_utc().date()
    );
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct BackupImportPayload {
    pub password: String,
    /// 信封 JSON 的内容。前端直接读取用户选择的文件后以字符串传入。
    pub content: String,
}

/// 校验并原子恢复一份配置备份（§23.5）。
///
/// 恢复完成后：配置快照整体重建，动态运行状态（健康、粘性、评分）按新
/// 配置清空重建；倍率状态表用本机数据重播。
pub async fn import_backup(
    State(state): State<SharedState>,
    admin: Admin,
    Json(payload): Json<BackupImportPayload>,
) -> AdminResult<Json<Value>> {
    let data =
        crate::security::backup::decrypt_backup(payload.content.as_bytes(), &payload.password)
            .map_err(|error| AdminError::bad_request(format!("{error:#}")))?;

    // 引用校验（§23.5）：恢复前先验证备份内部一致性，不修改现有配置。
    let account_ids: HashSet<String> = data
        .accounts
        .iter()
        .filter_map(|a| str_of(a, "id"))
        .collect();
    let account_groups: HashMap<String, String> = data
        .accounts
        .iter()
        .filter_map(|a| Some((str_of(a, "id")?, str_of(a, "group_id")?)))
        .collect();
    let group_ids: HashSet<String> = data.groups.iter().filter_map(|g| str_of(g, "id")).collect();
    for account in &data.accounts {
        let group = str_of(account, "group_id").unwrap_or_default();
        if !group_ids.contains(&group) {
            return Err(AdminError::bad_request(format!(
                "备份里的账号 {} 引用了不存在的分组 {group}",
                str_of(account, "name").unwrap_or_default()
            )));
        }
    }
    let model_groups: HashMap<String, String> = data
        .logical_models
        .iter()
        .filter_map(|m| Some((str_of(m, "id")?, str_of(m, "group_id")?)))
        .collect();
    for model in &data.logical_models {
        let group = str_of(model, "group_id").unwrap_or_default();
        if !group_ids.contains(&group) {
            return Err(AdminError::bad_request(format!(
                "备份里的逻辑模型 {} 引用了不存在的分组 {group}",
                str_of(model, "name").unwrap_or_default()
            )));
        }
    }
    for target in &data.dispatch_targets {
        let model = str_of(target, "logical_model_id").unwrap_or_default();
        if !data
            .logical_models
            .iter()
            .any(|m| str_of(m, "id").as_deref() == Some(model.as_str()))
        {
            return Err(AdminError::bad_request(format!(
                "备份里的调度目标引用了不存在的逻辑模型 {model}"
            )));
        }
        let account = str_of(target, "account_id").unwrap_or_default();
        if !account_ids.contains(&account) {
            return Err(AdminError::bad_request(format!(
                "备份里的调度目标引用了不存在的账号 {account}"
            )));
        }
        let model_group = model_groups.get(&model).map(String::as_str);
        let account_group = account_groups.get(&account).map(String::as_str);
        if model_group.is_none() || model_group != account_group {
            return Err(AdminError::bad_request(format!(
                "备份里的调度目标 {model} 与账号 {account} 不属于同一分组"
            )));
        }
    }

    state
        .store
        .import_backup(&data, &state.cipher)
        .await
        .map_err(AdminError::internal)?;
    // 动态状态整体作废：账号协议与模型可能都变了（§16.7）。
    state.runtime.evidence.clear();
    state.runtime.capabilities.clear();
    state.runtime.health.clear_all();
    reload(&state).await?;
    audit(&state, &admin, "backup_import", "config").await;
    Ok(Json(json!({
        "groups": data.groups.len(),
        "accounts": data.accounts.len(),
        "logical_models": data.logical_models.len(),
        "dispatch_targets": data.dispatch_targets.len(),
    })))
}

fn str_of(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

// -------------------------------------------------------------- 请求记录

#[derive(Deserialize)]
pub struct Pagination {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// 请求记录的筛选参数（§6.6）。全部可选，空表示不限。
#[derive(Deserialize)]
pub struct RequestQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// 起始时间（unix 秒，含）。
    pub since: Option<i64>,
    /// 结束时间（unix 秒，含）。
    pub until: Option<i64>,
    pub request_id: Option<String>,
    pub group_id: Option<String>,
    pub logical_model: Option<String>,
    pub target_id: Option<String>,
    pub account_id: Option<String>,
    /// `ok` 只看 2xx；`error` 只看失败。
    pub status: Option<String>,
    pub error_code: Option<String>,
}

fn trimmed_opt(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// 请求元数据分页查询（§6.6）。永远不包含正文；支持按时间/ID/分组/模型/
/// 目标/账号/状态/错误码筛选，并返回命中总数供翻页。
pub async fn list_requests(
    State(state): State<SharedState>,
    _: Admin,
    Query(query): Query<RequestQuery>,
) -> AdminResult<Json<Value>> {
    let filter = crate::storage::store::RequestFilter {
        since: query.since,
        until: query.until,
        request_id: trimmed_opt(&query.request_id).map(str::to_string),
        group_id: trimmed_opt(&query.group_id).map(str::to_string),
        logical_model: trimmed_opt(&query.logical_model).map(str::to_string),
        target_id: trimmed_opt(&query.target_id).map(str::to_string),
        account_id: trimmed_opt(&query.account_id).map(str::to_string),
        status: trimmed_opt(&query.status).map(str::to_string),
        error_code: trimmed_opt(&query.error_code).map(str::to_string),
    };
    let (records, total) = state
        .store
        .list_request_records_filtered(
            &filter,
            query.limit.unwrap_or(50),
            query.offset.unwrap_or(0),
        )
        .await
        .map_err(AdminError::internal)?;

    let items: Vec<Value> = records
        .iter()
        .map(|record| {
            json!({
                "request_id": record.request_id,
                "started_at": record.started_at,
                "duration_ms": record.duration_ms,
                "protocol": record.protocol.as_str(),
                "streaming": record.streaming,
                "group_id": record.group_id,
                "logical_model": record.logical_model,
                "target_id": record.target_id,
                "account_id": record.account_id,
                "upstream_model": record.upstream_model,
                "request_bytes": record.request_bytes,
                "upstream_status": record.upstream_status,
                "http_status": record.http_status,
                "error_code": record.error_code,
                "endpoint": record.endpoint,
                "degraded": record.degraded,
                "effective_multiplier": record.effective_multiplier,
                "cheapest_multiplier": record.cheapest_multiplier,
                "dearest_multiplier": record.dearest_multiplier,
                "attempts": record.attempts,
                "queued_ms": record.queued_ms,
                "sticky_hit": record.sticky_hit,
                // 用量与时机（§6.6）；上游没上报时是 null，不估算。
                "first_token_ms": record.first_token_ms,
                "input_tokens": record.input_tokens,
                "output_tokens": record.output_tokens,
                "config_version": record.config_version,
                // 每次尝试的明细：目标、端点、耗时、失败原因与是否计入预算。
                "attempts_detail": record.attempts_detail.iter().map(|attempt| json!({
                    "seq": attempt.seq,
                    "target_id": attempt.target_id,
                    "account_id": attempt.account_id,
                    "upstream_model": attempt.upstream_model,
                    "endpoint": attempt.endpoint,
                    "started_at": attempt.started_at,
                    "duration_ms": attempt.duration_ms,
                    "outcome": attempt.outcome,
                    "error_code": attempt.error_code,
                    "counts_against_budget": attempt.counts_against_budget,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(json!({ "data": items, "total": total })))
}

// ---------------------------------------------------------------- 辅助

async fn find_group(state: &SharedState, id: &str) -> AdminResult<Group> {
    state
        .store
        .list_groups()
        .await
        .map_err(AdminError::internal)?
        .into_iter()
        .find(|g| g.id == id)
        .ok_or_else(|| AdminError::not_found("分组不存在"))
}

async fn find_account(state: &SharedState, id: &str) -> AdminResult<Account> {
    state
        .store
        .list_accounts()
        .await
        .map_err(AdminError::internal)?
        .into_iter()
        .find(|a| a.id == id)
        .ok_or_else(|| AdminError::not_found("账号不存在"))
}

fn require_name(raw: &str, field: &str) -> AdminResult<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AdminError::bad_request(format!("{field}不能为空")));
    }
    if trimmed.chars().count() > 100 {
        return Err(AdminError::bad_request(format!(
            "{field}不能超过 100 个字符"
        )));
    }
    Ok(trimmed.to_string())
}

/// 调度权重四项之和必须为 100（§6.3）。
fn validate_weights(weights: SchedulingWeights) -> AdminResult<SchedulingWeights> {
    if !weights.is_valid() {
        return Err(AdminError::bad_request(format!(
            "四个调度权重之和必须为 100，当前为 {}",
            weights.sum()
        )));
    }
    Ok(weights)
}

fn validate_priority(priority: i32) -> AdminResult<i32> {
    if !(0..=100).contains(&priority) {
        return Err(AdminError::bad_request("人工优先级必须在 0 到 100 之间"));
    }
    Ok(priority)
}

/// RPM / TPM / 最大并发都必须为正；想"不限"就把字段留空（§17.1）。
///
/// 允许写 0 是个陷阱：它看起来像"不限"，实际会把目标永久锁死。
fn validate_limits(limits: Limits) -> AdminResult<Limits> {
    for (value, field) in [
        (limits.rpm, "RPM"),
        (limits.tpm, "TPM"),
        (limits.max_concurrency, "最大并发"),
    ] {
        if value == Some(0) {
            return Err(AdminError::bad_request(format!(
                "{field}不能为 0；不需要限制时请留空"
            )));
        }
    }
    Ok(limits)
}

/// 自动倍率来源必须配齐它需要的凭据（§11.2）。
///
/// `site_available` 表示该 Base URL 已经配了站点级凭据（§6.4）：账号自己不填
/// 也能工作，此时不再强制要求账号级令牌与用户 ID。
fn validate_multiplier_source(
    mode: MultiplierMode,
    new_api_token: Option<&str>,
    new_api_user_id: Option<&str>,
    site_available: bool,
) -> AdminResult<()> {
    if mode != MultiplierMode::NewApi || site_available {
        return Ok(());
    }
    // New API 的 sk-xxx 不被分组接口接受，必须另外提供访问令牌与用户 ID；
    // 缺任何一个就只能用手动倍率，写进去只会让探针每 5 分钟失败一次。
    if new_api_token.is_none_or(str::is_empty) {
        return Err(AdminError::bad_request(
            "New API 自动倍率需要在个人设置页生成的访问令牌",
        ));
    }
    if new_api_user_id.is_none_or(str::is_empty) {
        return Err(AdminError::bad_request(
            "New API 自动倍率需要用户 ID（New-Api-User 请求头）",
        ));
    }
    Ok(())
}

/// 校验队列最长等待：0（跟随请求总超时）到 1 小时之间。
fn validate_max_wait(value: Option<u32>) -> AdminResult<u32> {
    let value = value.unwrap_or(60);
    if value > 3600 {
        return Err(AdminError::bad_request("队列最长等待不能超过 3600 秒"));
    }
    Ok(value)
}

/// 校验 Base URL；未开启内网访问时同时执行 SSRF 网段检查（§23.3）。
fn validate_base_url(raw: &str, allow_private: bool) -> AdminResult<String> {
    match url_guard::validate_base_url(raw) {
        Ok(url) => Ok(normalize_base_url(&url)),
        // `BlockedAddress` 只在协议、主机等检查全部通过之后才会产生，所以
        // 显式开启内网访问的账号可以安全地放行这一种错误，而不放行其它。
        Err(url_guard::UrlGuardError::BlockedAddress(_)) if allow_private => {
            let url = reqwest::Url::parse(raw.trim())
                .map_err(|e| AdminError::bad_request(format!("Base URL 无法解析：{e}")))?;
            Ok(normalize_base_url(&url))
        }
        Err(error) => Err(AdminError::bad_request(error.to_string())),
    }
}

fn normalize_base_url(url: &reqwest::Url) -> String {
    url.as_str().trim_end_matches('/').to_string()
}

/// 唯一约束冲突返回 409，其余按内部错误处理。
fn conflict_or_internal(error: anyhow::Error, message: &str) -> AdminError {
    let text = format!("{error:#}");
    if text.contains("UNIQUE constraint failed") {
        AdminError::conflict(message)
    } else {
        AdminError::internal(error)
    }
}

/// 写操作完成后原子切换配置快照（§21）。
async fn reload(state: &SharedState) -> AdminResult<()> {
    // 同时刷新动态状态表：删掉的账号/目标不该继续占着熔断与评分条目，
    // 新增的自动倍率账号也要立刻进入刷新轮次（§21）。
    state.reload_config().await.map_err(AdminError::internal)?;
    Ok(())
}

async fn audit(state: &SharedState, admin: &Admin, action: &str, object: &str) {
    if let Err(error) = state
        .store
        .record_audit(&admin.username, action, object, "ok")
        .await
    {
        tracing::warn!(%error, action, "写入审计日志失败");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_must_sum_to_one_hundred() {
        assert!(validate_weights(SchedulingWeights::default()).is_ok());
        let bad = SchedulingWeights {
            multiplier: 50,
            reliability: 25,
            first_token: 20,
            throughput: 15,
        };
        let error = validate_weights(bad).unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("110"));
    }

    #[test]
    fn priority_is_bounded_to_zero_through_one_hundred() {
        assert!(validate_priority(0).is_ok());
        assert!(validate_priority(100).is_ok());
        assert!(validate_priority(-1).is_err());
        assert!(validate_priority(101).is_err());
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(require_name("  主力  ", "分组名称").unwrap(), "主力");
        assert!(require_name("   ", "分组名称").is_err());
        assert!(require_name(&"x".repeat(101), "分组名称").is_err());
    }

    #[test]
    fn private_base_urls_need_the_explicit_opt_in() {
        assert!(validate_base_url("http://127.0.0.1:8080", false).is_err());
        assert!(validate_base_url("http://127.0.0.1:8080", true).is_ok());
        // 开启内网访问也不能绕过协议限制。
        assert!(validate_base_url("file:///etc/passwd", true).is_err());
    }

    #[test]
    fn public_base_urls_are_normalised() {
        assert_eq!(
            validate_base_url("https://api.anthropic.com/", false).unwrap(),
            "https://api.anthropic.com"
        );
    }
}
