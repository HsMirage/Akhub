//! `/v1/models` 与 `/v1/models/{model}`：响应形状按鉴权头判定（§7.3）。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::app::SharedState;
use crate::auth::{self, Credential};
use crate::config::GroupView;
use crate::gateway::error::{ErrorCode, GatewayError};

/// 逻辑模型列表。使用 `x-api-key` 的客户端拿到 Anthropic 形状，
/// 使用 `Authorization: Bearer` 的客户端拿到 OpenAI 形状。
pub async fn list(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    let (group, credential) = match resolve(&state, &headers) {
        Ok(pair) => pair,
        Err(error) => return error.into_response(),
    };

    let mut names = listable_models(&group);
    names.sort();

    let body = if credential.anthropic_style {
        anthropic_list(&names, &group)
    } else {
        openai_list(&names, &group)
    };
    Json(body).into_response()
}

/// 单个逻辑模型，形状规则与列表一致。
pub async fn get(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(model): Path<String>,
) -> Response {
    let (group, credential) = match resolve(&state, &headers) {
        Ok(pair) => pair,
        Err(error) => return error.into_response(),
    };

    if !listable_models(&group).contains(&model) {
        // 未知逻辑模型直接报错，绝不跨组搜索（§7.3）。
        return GatewayError::new(
            ErrorCode::ModelNotFound,
            format!("分组「{}」中不存在逻辑模型 {model}", group.group.name),
        )
        .with_protocol(auth::error_protocol(Some(&credential)))
        .into_response();
    }

    let created = group
        .find_model(&model)
        .map(|m| m.model.created_at.unix_timestamp())
        .unwrap_or_default();

    let body = if credential.anthropic_style {
        anthropic_entry(&model, created)
    } else {
        openai_entry(&model, created)
    };
    Json(body).into_response()
}

/// 鉴权并取得分组视图。
fn resolve(
    state: &SharedState,
    headers: &HeaderMap,
) -> Result<(std::sync::Arc<GroupView>, Credential), GatewayError> {
    let credential = auth::extract_credential(headers)?;
    let config = state.config.current();
    let group = auth::authenticate(&config, &state.key_digest, &credential)
        .map_err(|e| e.with_protocol(auth::error_protocol(Some(&credential))))?;
    Ok((std::sync::Arc::clone(group), credential))
}

/// 该分组中应当对外可见的逻辑模型名。
///
/// 目标全部不健康时模型仍然保留：否则客户端会因为一次瞬时故障缓存下一份
/// 缺模型的列表（§7.3）。真正被排除的只有"零目标"。
fn listable_models(group: &GroupView) -> Vec<String> {
    let mut names: Vec<String> = group
        .models
        .values()
        .filter(|model| model.is_listable())
        .flat_map(|model| model.exposed_names())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn openai_entry(name: &str, created: i64) -> Value {
    json!({
        "id": name,
        "object": "model",
        "created": created,
        // 绝不暴露真实上游身份（§7.3）。
        "owned_by": "akhub",
    })
}

fn openai_list(names: &[String], group: &GroupView) -> Value {
    let data: Vec<Value> = names
        .iter()
        .map(|name| openai_entry(name, created_at(group, name)))
        .collect();
    json!({ "object": "list", "data": data })
}

fn anthropic_entry(name: &str, created: i64) -> Value {
    json!({
        "type": "model",
        "id": name,
        "display_name": name,
        "created_at": rfc3339(created),
    })
}

fn anthropic_list(names: &[String], group: &GroupView) -> Value {
    let data: Vec<Value> = names
        .iter()
        .map(|name| anthropic_entry(name, created_at(group, name)))
        .collect();
    json!({
        "data": data,
        "has_more": false,
        "first_id": names.first().cloned(),
        "last_id": names.last().cloned(),
    })
}

fn created_at(group: &GroupView, name: &str) -> i64 {
    group
        .find_model(name)
        .map(|m| m.model.created_at.unix_timestamp())
        .unwrap_or_default()
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use time::OffsetDateTime;

    use super::*;
    use crate::config::{LogicalModelView, TargetView};
    use crate::domain::{
        Account, DispatchTarget, Group, LogicalModel, ModelOrigin, Multiplier, Protocol,
        SchedulingWeights,
    };

    fn model_view(name: &str, enabled: bool, target_count: usize) -> Arc<LogicalModelView> {
        let account = Arc::new(Account {
            id: "a1".into(),
            group_id: Some("g1".into()),
            name: "账号A".into(),
            base_url: "https://api.example.com".into(),
            preferred_protocol: Protocol::OpenAiChat,
            adaptive_protocol: true,
            default_priority: 50,
            calibration: Multiplier::ONE,
            multiplier_mode: crate::domain::MultiplierMode::Manual,
            manual_multiplier: Multiplier::ONE,
            new_api_user_id: None,
            new_api_group: None,
            limits: crate::domain::Limits::default(),
            allow_private_network: false,
            enabled: true,
            hide_original: false,
            auto_sync: false,
            model_synced_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        });
        let targets = (0..target_count)
            .map(|i| {
                Arc::new(TargetView {
                    target: DispatchTarget {
                        id: format!("t{i}"),
                        logical_model_id: name.into(),
                        account_id: "a1".into(),
                        upstream_model: format!("{name}-真名"),
                        hide_original: false,
                        priority_override: None,
                        limits: crate::domain::Limits::default(),
                        enabled: true,
                        created_at: OffsetDateTime::UNIX_EPOCH,
                    },
                    account: Arc::clone(&account),
                    priority: 50,
                })
            })
            .collect();
        Arc::new(LogicalModelView {
            model: LogicalModel {
                id: name.into(),
                group_id: "g1".into(),
                name: name.into(),
                origin: ModelOrigin::Auto,
                enabled,
                created_at: OffsetDateTime::UNIX_EPOCH,
            },
            targets,
            exposed: true,
            aliases: Vec::new(),
        })
    }

    fn group(models: Vec<Arc<LogicalModelView>>) -> GroupView {
        GroupView {
            group: Group {
                id: "g1".into(),
                name: "主力".into(),
                key_prefix: "akh-000000".into(),
                key_digest_hex: "d1".into(),
                multiplier_limit: Multiplier::ONE,
                weights: SchedulingWeights::default(),
                queue_capacity: 100,
                max_wait_secs: 60,
                allow_managed_background: false,
                allow_degrade: true,
                created_at: OffsetDateTime::UNIX_EPOCH,
            },
            models: models
                .into_iter()
                .map(|m| (m.model.name.clone(), m))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn zero_target_models_are_hidden_but_disabled_targets_are_not() {
        let group = group(vec![
            model_view("有目标", true, 2),
            model_view("零目标", true, 0),
            model_view("已停用", false, 3),
        ]);
        let listed = listable_models(&group);
        assert_eq!(listed, vec!["有目标".to_string()]);
    }

    #[test]
    fn openai_shape_matches_the_specification() {
        let group = group(vec![model_view("glm-4.6", true, 1)]);
        let body = openai_list(&["glm-4.6".to_string()], &group);
        assert_eq!(body["object"], "list");
        assert_eq!(body["data"][0]["id"], "glm-4.6");
        assert_eq!(body["data"][0]["object"], "model");
        assert_eq!(body["data"][0]["owned_by"], "akhub");
    }

    #[test]
    fn anthropic_shape_matches_the_specification() {
        let group = group(vec![model_view("claude-sonnet-4-5", true, 1)]);
        let names = vec!["claude-sonnet-4-5".to_string()];
        let body = anthropic_list(&names, &group);
        assert_eq!(body["data"][0]["type"], "model");
        assert_eq!(body["data"][0]["id"], "claude-sonnet-4-5");
        assert_eq!(body["data"][0]["display_name"], "claude-sonnet-4-5");
        assert_eq!(body["has_more"], false);
        assert_eq!(body["first_id"], "claude-sonnet-4-5");
        assert_eq!(body["last_id"], "claude-sonnet-4-5");
        assert!(
            body["data"][0]["created_at"]
                .as_str()
                .unwrap()
                .ends_with("Z")
        );
    }

    #[test]
    fn upstream_model_names_never_appear_in_either_shape() {
        let group = group(vec![model_view("glm-4.6", true, 1)]);
        let names = vec!["glm-4.6".to_string()];
        for body in [openai_list(&names, &group), anthropic_list(&names, &group)] {
            assert!(
                !body.to_string().contains("真名"),
                "真实上游模型名泄漏了：{body}"
            );
        }
    }
}
