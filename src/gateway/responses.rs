//! Responses 状态链与网关响应 ID（§15.1、§15.2）。
//!
//! 上游 A 生成的响应 ID，上游 B 一概不认识；客户端拿着 `previous_response_id`
//! 再来时，如果调度换了个号，唯一无损的办法是 Akhub 自己保存可重放状态。
//! 所以对外 ID 是网关的（`resp_akh_*`），本地保存"网关 ID → 上游 ID/账号/
//! 端点"的定位映射，以及 store 未关闭时的加密可重放正文。
//!
//! 三种保存模式：
//! 1. `store` 未设或为 true 且保留期 > 0：保存正文，可跨上游重建。
//! 2. `store: false`：只保存最小定位映射，原生上游仍可续链。
//! 3. 保留期为 0：同上，且没有跨上游重建能力——这是管理员的显式选择。

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::domain::Protocol;
use crate::gateway::error::{ErrorCode, GatewayError};
use crate::protocol::canonical::{Part, Request, Role};
use crate::storage::store::ResponseStateRow;

/// 网关响应 ID 的前缀，与上游 ID 划清界限（§15.1）。
const GATEWAY_PREFIX: &str = "resp_akh_";

/// 入口是 Responses 时解析请求里的状态链引用。
///
/// 引用一律是网关 ID；上游 ID 不在映射里时按过期处理，绝不把缺失历史的
/// 请求当作新对话发送（§15.2）。
pub fn referenced_state(body: &Value) -> Option<&str> {
    body.get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .or_else(|| {
            body.get("conversation")
                .and_then(|c| c.as_str().or_else(|| c.get("id").and_then(Value::as_str)))
                .filter(|id| !id.trim().is_empty())
        })
}

/// 按请求参数决定这次响应的可重放正文是否要保存。
pub fn should_store(body: &Value, retention_days: u32) -> bool {
    if retention_days == 0 {
        return false;
    }
    body.get("store").and_then(Value::as_bool).unwrap_or(true)
}

/// 生成网关响应 ID。
pub fn gateway_id() -> String {
    format!("{GATEWAY_PREFIX}{}", ulid::Ulid::generate())
}

/// 客户端引用了一个我们不认识或已过期的 ID。
pub fn expired_error(protocol: Protocol, id: &str) -> GatewayError {
    GatewayError::new(
        ErrorCode::ResponseStateExpired,
        format!("响应状态 {id} 不存在或已过期，无法继续会话"),
    )
    .with_protocol(protocol)
}

/// 查询一次引用：命中返回记录，未命中或已过期返回错误。
pub async fn lookup(
    state: &crate::app::SharedState,
    group_id: &str,
    reference: &str,
    protocol: Protocol,
) -> Result<ResponseStateRow, GatewayError> {
    let record = state
        .store
        .response_state(reference, group_id)
        .await
        .map_err(AdminInternal)?;
    let now = crate::storage::now_unix();
    match record {
        Some(row) if row.expires_at >= now => Ok(row),
        _ => Err(expired_error(protocol, reference)),
    }
}

/// 把 `previous_response_id` / `conversation` 替换成上游的真 ID（原生续链）。
///
/// 网关 ID 绝不下发到上游：上游不认识它，而它本身就是我们内部状态的地址。
pub fn rewrite_reference(body: &mut Value, upstream_id: &str) {
    if let Some(object) = body.as_object_mut() {
        if object.contains_key("previous_response_id") {
            object.insert("previous_response_id".into(), json!(upstream_id));
        }
        if let Some(conversation) = object.get_mut("conversation") {
            match conversation {
                Value::String(_) => {
                    object.insert("conversation".into(), json!(upstream_id));
                }
                Value::Object(map) => {
                    map.insert("id".into(), json!(upstream_id));
                }
                _ => {}
            }
        }
    }
}

/// 去掉状态链引用：保存正文之后，重建输入时引用本身就是多余的。
pub fn strip_reference(body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        object.remove("previous_response_id");
        object.remove("conversation");
    }
}

/// 用保存的可重放正文重建一次请求的输入历史（§15.2 跨上游重建）。
///
/// 保存的正文是**入口协议**的完整请求体（去掉引用）。换上游时用它解析出
/// 中间格式，再发射到目标协议；同一协议时直接原样返回。
pub fn rebuild_body(
    stored: &Value,
    entry_protocol: Protocol,
    target_protocol: Protocol,
    model: &str,
) -> Result<Value, String> {
    // 查询回放用的最终响应对象绝不是请求的一部分，重建时必须先剥掉。
    let mut stored = stored.clone();
    strip_replay_object(&mut stored);
    let mut request: Request =
        crate::protocol::parse_request(entry_protocol, &stored).map_err(|e| e.to_string())?;
    // 历史属于旧请求；模型、流式开关与参数以本次请求为准的部分在调用方合并。
    request.model = model.to_string();

    if entry_protocol == target_protocol {
        let mut body = stored;
        if let Some(object) = body.as_object_mut() {
            object.insert("model".into(), json!(model));
        }
        return Ok(body);
    }

    let emitted =
        crate::protocol::emit_request(target_protocol, &request).map_err(|e| e.to_string())?;
    Ok(emitted.body)
}

/// 查询回放用的最终响应对象在密封正文里的保留键（§15.1、§15.3）。
///
/// 它只服务于 `GET /v1/responses/{id}`：让查询返回真实的输出项、usage 与状态，
/// 而不是从请求体拼一个"看起来像响应"的对象。续链重建前必须剥掉。
pub const REPLAY_RESPONSE_KEY: &str = "__akhub_response";

fn strip_replay_object(body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        object.remove(REPLAY_RESPONSE_KEY);
    }
}

/// 把一次响应的输入与输出项合并成可重放正文。
///
/// 正文保存**入口协议的原始请求体**（去掉引用、去掉 store 字段），这是唯一
/// 无损的形状：换成保存"中间格式"会丢掉未知字段，换成保存"输出项"则丢掉
/// 采样参数。重建时再按目标协议重新解析与发射。
pub fn stored_body(
    entry_body: &Value,
    output_items: Option<&Value>,
    final_response: Option<&Value>,
) -> Value {
    let mut body = entry_body.clone();
    strip_reference(&mut body);
    let needs_messages_append = {
        let Some(object) = body.as_object_mut() else {
            return body;
        };
        object.remove("store");
        // 流式与否不影响历史形状，统一按非流式保存。
        object.remove("stream");
        object.remove("stream_options");
        // 字符串输入先归一成标准项数组，输出项才有的放矢。
        if let Some(Value::String(text)) = object.get("input").cloned() {
            object.insert(
                "input".into(),
                json!([{
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                }]),
            );
        }
        if let (Some(items), Some(Value::Array(input))) = (output_items, object.get_mut("input")) {
            input.extend(items.as_array().cloned().unwrap_or_default());
        }
        !object.contains_key("input") && output_items.is_some()
    };
    // Chat / Messages 的历史在 messages / 顶层，输出项作为助手轮次追加。
    if needs_messages_append && let Some(items) = output_items {
        append_output_to_messages(&mut body, items);
    }
    if let (Some(object), Some(response)) = (body.as_object_mut(), final_response) {
        // 最终响应对象留一份给查询接口；续链重建前会被剥掉。
        object.insert(REPLAY_RESPONSE_KEY.into(), response.clone());
    }
    body
}

/// 把输出项追加到 Chat / Messages 形状的 messages 数组里。
fn append_output_to_messages(body: &mut Value, output_items: &Value) {
    let Some(items) = output_items.as_array() else {
        return;
    };
    for item in items {
        let Some(message) = item.as_object() else {
            continue;
        };
        // 上游响应项统一转成助手轮次：Responses 项已经是 message 形状。
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("assistant");
        let content = message.get("content").cloned().unwrap_or(Value::Null);
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            messages.push(json!({"role": role, "content": content}));
        }
    }
}

/// 上游 Responses 响应里的输出项，供保存可重放正文用。
pub fn output_items_of(body: &Value) -> Option<Value> {
    body.get("output").cloned()
}

/// 一个内部错误的包装，统一转 500。
struct AdminInternal(anyhow::Error);

impl From<AdminInternal> for GatewayError {
    fn from(error: AdminInternal) -> Self {
        tracing::error!(error = %error.0, "Responses 状态读写失败");
        GatewayError::new(ErrorCode::InternalError, "内部错误，详见服务端日志")
    }
}

// -------------------------------------------------------- 原生上游生命周期代理

/// 状态链记录指向的原生 Responses 上游（§15.3）。
struct NativeTarget {
    account: crate::domain::Account,
    api_key: String,
    upstream_id: String,
}

/// 记录里有原生 Responses 映射时解析出目标；否则返回 `None`。
///
/// 判定依据是记录里的端点证据：只有真正把请求发到了上游 `/v1/responses`
/// 的记录才谈得上原生查询与取消。跨协议转出来的响应没有原生生命周期。
async fn native_target(
    state: &crate::app::SharedState,
    record: &ResponseStateRow,
) -> Option<NativeTarget> {
    if record.endpoint.as_deref() != Some(crate::upstream::Endpoint::Responses.as_str()) {
        return None;
    }
    let upstream_id = record.upstream_id.clone()?;
    let account_id = record.account_id.clone()?;
    let account = state
        .store
        .list_accounts()
        .await
        .ok()?
        .into_iter()
        .find(|account| account.id == account_id)?;
    let sealed = state.store.account_sealed_key(&account.id).await.ok()??;
    let plaintext = state.cipher.open(&sealed).ok()?;
    let api_key = String::from_utf8(plaintext.to_vec()).ok()?;
    Some(NativeTarget {
        account,
        api_key,
        upstream_id,
    })
}

/// 一次需要转发的原生生命周期动作。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Retrieve,
    InputItems,
    Cancel,
    Delete,
}

/// 把生命周期调用转发到原账号。
///
/// 与推理路径使用同一个 HTTP 客户端与同一套地址校验；失败时返回明确错误，
/// 绝不把本地状态冒充成上游的真实状态。
async fn proxy_lifecycle(
    state: &crate::app::SharedState,
    target: &NativeTarget,
    action: Lifecycle,
    gateway_id: &str,
) -> Result<Value, GatewayError> {
    let protocol = Protocol::OpenAiResponses;
    let internal = || {
        GatewayError::new(ErrorCode::InternalError, "内部错误，详见服务端日志")
            .with_protocol(protocol)
    };
    let mut url = crate::upstream::build_url(
        &target.account.base_url,
        crate::upstream::Endpoint::Responses,
    )
    .map_err(|error| {
        tracing::warn!(%error, "构造上游生命周期 URL 失败");
        internal()
    })?;
    {
        let mut segments = url.path_segments_mut().map_err(|_| internal())?;
        segments.push(&target.upstream_id);
        match action {
            Lifecycle::InputItems => {
                segments.push("input_items");
            }
            Lifecycle::Cancel => {
                segments.push("cancel");
            }
            Lifecycle::Retrieve | Lifecycle::Delete => {}
        }
    }

    crate::security::url_guard::assert_resolvable(&url, target.account.allow_private_network)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "生命周期调用的目标地址被拒绝");
            GatewayError::new(ErrorCode::UpstreamExhausted, "上游地址不可用")
                .with_protocol(protocol)
        })?;

    let mut headers =
        crate::upstream::headers_for_protocol(Protocol::OpenAiResponses, &target.api_key).map_err(
            |error| {
                tracing::warn!(%error, "构造上游生命周期请求头失败");
                internal()
            },
        )?;
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );

    let client = state
        .upstream
        .http_for(target.account.allow_private_network);
    let request = match action {
        Lifecycle::Retrieve | Lifecycle::InputItems => client.get(url),
        Lifecycle::Cancel => client.post(url),
        Lifecycle::Delete => client.delete(url),
    };
    let response = request
        .headers(headers)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "上游生命周期调用失败");
            GatewayError::new(ErrorCode::UpstreamExhausted, "上游查询失败，请稍后重试")
                .with_protocol(protocol)
        })?;

    let status = response.status();
    if !status.is_success() {
        let (code, message) = match status.as_u16() {
            404 => (ErrorCode::ResponseStateExpired, "上游已经不存在该响应"),
            400 | 409 => (
                ErrorCode::UnsupportedParameter,
                "上游表示该响应不支持这个操作（例如不是后台任务）",
            ),
            _ => (ErrorCode::UpstreamExhausted, "上游生命周期调用失败"),
        };
        return Err(GatewayError::new(code, message).with_protocol(protocol));
    }

    let mut value: Value = response.json().await.map_err(|error| {
        tracing::warn!(%error, "上游生命周期响应不是合法 JSON");
        GatewayError::new(ErrorCode::UpstreamProtocolError, "上游返回了无法解析的响应")
            .with_protocol(protocol)
    })?;
    // 对外只暴露网关 ID（§15.1）；列表类响应的项 ID 不是响应身份，保持原样。
    if let Some(object) = value.as_object_mut()
        && object.contains_key("id")
    {
        object.insert("id".into(), json!(gateway_id));
    }
    Ok(value)
}

// ------------------------------------------------------------------ 管理路由

/// `GET /v1/responses/{id}`：把保存的响应回放给客户端。
///
/// 没有保存正文（store:false 或已过期）但上游还在时，客户端拿到的是明确的
/// `response_state_expired`，而不是一个貌似成功却缺历史的响应。
pub async fn retrieve(
    State(state): State<crate::app::SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let protocol = Protocol::OpenAiResponses;
    let Some(group) = authenticate(&state, &headers) else {
        return auth_error(protocol);
    };
    // 托管任务（`bg_akh_*`）走自己的表：不问上游，也不看响应状态链（§29.1）。
    if crate::gateway::background::is_managed(&id) {
        return match crate::gateway::background::lookup(&state, &group, &id).await {
            Some(object) => axum::Json(object).into_response(),
            None => expired_error(protocol, &id).into_response(),
        };
    }
    let record = match lookup(&state, &group, &id, protocol).await {
        Ok(record) => record,
        Err(error) => return error.into_response(),
    };
    // 原生 Responses 映射存在时优先问原账号：查询结果、状态与运行中的后台
    // 任务状态才是真的（§15.3）。上游不支持查询（404/405 或网络故障）时，
    // 只要本地保存了真实响应对象就回放它——那仍然是上游给出的事实。
    if let Some(target) = native_target(&state, &record).await {
        match proxy_lifecycle(&state, &target, Lifecycle::Retrieve, &id).await {
            Ok(value) => return axum::Json(value).into_response(),
            Err(error) => {
                if record.sealed_body.is_none() {
                    return error.into_response();
                }
                tracing::debug!(
                    code = error.code.as_str(),
                    "上游查询不可用，回放本地保存的响应对象"
                );
            }
        }
    }
    // 没有原生映射（跨协议或 store:false）：用保存的历史重建一个**诚实的**
    // 对象——只承诺我们真的保存了的东西。新版记录里带的是上游最终响应对象。
    let Some(sealed) = record.sealed_body else {
        return expired_error(protocol, &id).into_response();
    };
    let plaintext = match state.cipher.open(&sealed) {
        Ok(plaintext) => plaintext,
        Err(error) => {
            tracing::error!(%error, "Responses 状态解密失败");
            return GatewayError::new(ErrorCode::InternalError, "内部错误，详见服务端日志")
                .with_protocol(protocol)
                .into_response();
        }
    };
    let body: Value = match serde_json::from_slice(&plaintext) {
        Ok(body) => body,
        Err(_) => {
            return GatewayError::new(ErrorCode::InternalError, "内部错误，详见服务端日志")
                .with_protocol(protocol)
                .into_response();
        }
    };
    axum::Json(replay_response(&body, &id)).into_response()
}

/// `GET /v1/responses/{id}/input_items`：输入项列表（§15）。
///
/// 没有经过等价性验证的本地实现，所以只代理原生上游；拿不到原生映射时返回
/// 明确错误，不拿别的数组冒充输入项。
pub async fn input_items(
    State(state): State<crate::app::SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let protocol = Protocol::OpenAiResponses;
    let Some(group) = authenticate(&state, &headers) else {
        return auth_error(protocol);
    };
    let record = match lookup(&state, &group, &id, protocol).await {
        Ok(record) => record,
        Err(error) => return error.into_response(),
    };
    let Some(target) = native_target(&state, &record).await else {
        return GatewayError::new(
            ErrorCode::UnsupportedParameter,
            "该响应没有可查询的原生输入项（跨协议或 store:false）",
        )
        .with_protocol(protocol)
        .into_response();
    };
    match proxy_lifecycle(&state, &target, Lifecycle::InputItems, &id).await {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `DELETE /v1/responses/{id}`：删除上游（若支持）与本地状态。
///
/// 上游删除是尽力而为：不支持删除的上游不该阻塞本地清理（§15.2）。
pub async fn destroy(
    State(state): State<crate::app::SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let protocol = Protocol::OpenAiResponses;
    let Some(group) = authenticate(&state, &headers) else {
        return auth_error(protocol);
    };
    // 托管任务：先中断在跑的执行，再删记录（§29.1）。
    if crate::gateway::background::is_managed(&id) {
        let deleted = crate::gateway::background::destroy(&state, &group, &id).await;
        return if deleted {
            axum::Json(json!({"id": id, "object": "response", "deleted": true})).into_response()
        } else {
            expired_error(protocol, &id).into_response()
        };
    }
    let record = match lookup(&state, &group, &id, protocol).await {
        Ok(record) => record,
        Err(error) => return error.into_response(),
    };
    if let Some(target) = native_target(&state, &record).await
        && let Err(error) = proxy_lifecycle(&state, &target, Lifecycle::Delete, &id).await
    {
        // 删除失败不影响本地清理，但必须留下可诊断的日志。
        tracing::warn!(
            code = error.code.as_str(),
            message = %error.message,
            "上游删除响应失败，继续清理本地状态"
        );
    }
    match state.store.delete_response_state(&id, &group).await {
        Ok(_) => {
            axum::Json(json!({"id": id, "object": "response", "deleted": true})).into_response()
        }
        Err(error) => GatewayError::from(AdminInternal(error)).into_response(),
    }
}

/// `POST /v1/responses/{id}/cancel`。
///
/// 只有原生 Responses 上游才真的有"取消"语义；其他上游从来没有开始过这个
/// 任务，冒充取消就是伪造（§15.3）。没有原生映射时返回明确的错误，绝不
/// 把本地记录改个状态就说取消成功。
pub async fn cancel(
    State(state): State<crate::app::SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let protocol = Protocol::OpenAiResponses;
    let Some(group) = authenticate(&state, &headers) else {
        return auth_error(protocol);
    };
    // 托管任务：真的中断在跑的任务（断开上游连接），再把状态写进库（§29.1）。
    if crate::gateway::background::is_managed(&id) {
        return match crate::gateway::background::cancel(&state, &group, &id).await {
            None => expired_error(protocol, &id).into_response(),
            Some((mut object, aborted)) => {
                if let Some(map) = object.as_object_mut() {
                    map.insert(
                        "upstream_connection_aborted".into(),
                        serde_json::json!(aborted),
                    );
                    if !aborted {
                        map.insert(
                            "note".into(),
                            serde_json::json!("任务当时已结束，没有正在执行的上游连接需要中断"),
                        );
                    }
                }
                axum::Json(object).into_response()
            }
        };
    }
    let record = match lookup(&state, &group, &id, protocol).await {
        Ok(record) => record,
        Err(error) => return error.into_response(),
    };
    let Some(target) = native_target(&state, &record).await else {
        return GatewayError::new(
            ErrorCode::UnsupportedParameter,
            "该响应不是原生后台任务，Akhub 不会伪报取消成功（§15.3）",
        )
        .with_protocol(protocol)
        .into_response();
    };
    match proxy_lifecycle(&state, &target, Lifecycle::Cancel, &id).await {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

fn authenticate(state: &crate::app::SharedState, headers: &HeaderMap) -> Option<String> {
    let credential = crate::auth::extract_credential(headers).ok()?;
    let config = state.config.current();
    crate::auth::authenticate(&config, &state.key_digest, &credential)
        .ok()
        .map(|group| group.group.id.clone())
}

fn auth_error(protocol: Protocol) -> Response {
    GatewayError::new(ErrorCode::AuthInvalid, "Key 无效")
        .with_protocol(protocol)
        .into_response()
}

/// 把保存的状态回放成响应对象。
///
/// 首选我们真的保存过的最终响应对象（`__akhub_response`）：输出项、usage 与
/// 状态都来自上游，不是拼出来的。只有早期记录或没有最终对象时，才退回
/// "历史 + 网关补齐状态"的重建，并且明确不编造输出与 usage。
fn replay_response(saved: &Value, gateway_id: &str) -> Value {
    if let Some(mut response) = saved.get(REPLAY_RESPONSE_KEY).cloned() {
        if let Some(object) = response.as_object_mut() {
            object.insert("id".into(), json!(gateway_id));
            object.entry("object").or_insert_with(|| json!("response"));
            object.entry("status").or_insert_with(|| json!("completed"));
        }
        return response;
    }
    let model = saved.get("model").cloned().unwrap_or(json!(Value::Null));
    json!({
        "id": gateway_id,
        "object": "response",
        "model": model,
        "status": "completed",
        "input": saved.get("input").or_else(|| saved.get("messages")).cloned().unwrap_or(Value::Null),
        "output": [],
    })
}

/// 一次请求的状态链处理计划。
///
/// 在 `handle()` 里**一次性**决定，之后整个请求生命周期都不再碰状态库：
/// 资格过滤、转换缓存与逐目标准备都建立在这个结果之上。
#[derive(Debug, Clone)]
pub struct ChainPlan {
    pub group_id: String,
    pub logical_model: String,
    /// 客户端发来的引用原值。粘性键以它为锚——同一个会话的每一轮必须
    /// 落在同一个键上（§10.1）。
    pub reference: Option<String>,
    /// 原生续链候选：引用改写成上游 ID 后发回原账号。上游自己掌握服务端
    /// 状态与缓存，这永远是最优路径。
    pub pinned: Option<PinnedRef>,
    /// 可重放的合并请求体：非原生续链的候选（原账号失败或换账号）用它。
    /// 这是"无损切换需要保存可重放状态"的落点（§15.2）。
    pub merged: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct PinnedRef {
    pub account_id: String,
    pub upstream_id: String,
}

impl ChainPlan {
    pub fn new(group_id: impl Into<String>, logical_model: impl Into<String>) -> Self {
        Self {
            group_id: group_id.into(),
            logical_model: logical_model.into(),
            reference: None,
            pinned: None,
            merged: None,
        }
    }

    /// 粘性键的引用锚点：有引用时粘性以引用为准（§10.1 的第 1 级）。
    pub fn sticky_reference(&self) -> Option<&str> {
        self.reference.as_deref()
    }

    /// 资格过滤与转换所依据的请求体：有可重放合并体时用它——引用不在里面，
    /// 跨协议目标因此保持合格；否则用客户端的原始请求体。
    pub fn body_for_translation<'a>(&'a self, original: &'a Value) -> &'a Value {
        self.merged.as_ref().unwrap_or(original)
    }
}

/// 在请求入口处理状态链引用（§15.2）。
///
/// 命中记录时同时准备好两条路：原账号的原生续链与跨上游的可重放正文。
/// 引用从未知或过期的 ID 解析失败时立即报错，绝不按新对话发送。
pub async fn resolve_request(
    state: &crate::app::SharedState,
    plan: &mut ChainPlan,
    body: &mut Value,
    downstream: Protocol,
) -> Result<(), GatewayError> {
    let Some(reference) = referenced_state(body).map(str::to_string) else {
        return Ok(());
    };
    let record = lookup(state, &plan.group_id, &reference, downstream).await?;
    plan.reference = Some(reference.clone());

    // 原生续链候选：原账号 + 上游 ID。
    if let (Some(account), Some(upstream)) = (&record.account_id, &record.upstream_id) {
        plan.pinned = Some(PinnedRef {
            account_id: account.clone(),
            upstream_id: upstream.clone(),
        });
    }

    // 可重放正文：合并历史（保存的请求体 = 全部历史 + 上轮输出）。
    if let Some(sealed) = &record.sealed_body {
        let saved = state
            .cipher
            .open(sealed)
            .ok()
            .and_then(|plaintext| serde_json::from_slice::<Value>(&plaintext).ok());
        if let Some(saved) = saved {
            let rebuilt = rebuild_body(
                &saved,
                record
                    .protocol
                    .as_deref()
                    .and_then(Protocol::parse)
                    .unwrap_or(downstream),
                downstream,
                plan.logical_model.trim(),
            )
            .ok();
            if let Some(rebuilt) = rebuilt {
                plan.merged = Some(merge_history(body.clone(), rebuilt));
            }
        }
    }

    if plan.pinned.is_none() && plan.merged.is_none() {
        return Err(expired_error(downstream, &reference));
    }
    Ok(())
}

/// 把重建出的历史与本次请求合并。
///
/// 重建体提供全部历史；本次请求只贡献"最后一条用户输入"与本次参数。
/// 这样客户端的多轮调用只发增量，Akhub 负责拼全。
fn merge_history(current: Value, rebuilt: Value) -> Value {
    let current_inputs = current_input_items(&current);
    let mut merged = rebuilt;
    if let Some(object) = merged.as_object_mut()
        && !current_inputs.is_empty()
    {
        match object.get_mut("input") {
            Some(Value::Array(items)) => items.extend(current_inputs.clone()),
            // 重建体里是字符串输入：先转成标准消息项，再接本次输入。
            Some(Value::String(text)) if !text.is_empty() => {
                let history = json!({
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": text.clone()}]
                });
                let mut items = vec![history];
                items.extend(current_inputs);
                object.insert("input".into(), json!(items));
            }
            _ => {
                object.insert("input".into(), json!(current_inputs));
            }
        }
    }
    // 本次请求的参数覆盖历史里的旧值：模型名已由调度改写，max_tokens 等以
    // 本次为准。
    if let (Some(current_object), Some(merged_object)) =
        (current.as_object(), merged.as_object_mut())
    {
        for field in [
            "max_output_tokens",
            "temperature",
            "top_p",
            "instructions",
            "stream",
            "tools",
            "tool_choice",
        ] {
            if let Some(value) = current_object.get(field) {
                merged_object.insert(field.to_string(), value.clone());
            }
        }
    }
    merged
}

/// 当前 Responses 请求的输入项必须整体追加，不能只保留最后一个 user 项；
/// function_call_output、tool_result 和并行工具结果都属于续链语义。
fn current_input_items(body: &Value) -> Vec<Value> {
    match body.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) if !text.is_empty() => vec![json!({
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": text}]
        })],
        _ => Vec::new(),
    }
}

/// 保存一次成功响应的状态链记录。
pub struct PendingState {
    pub gateway_id: String,
    pub upstream_id: Option<String>,
    pub account_id: Option<String>,
    pub target_id: Option<String>,
    pub endpoint: Option<String>,
}

/// 把一次成功完成的 Responses 请求写入状态链（§15.2）。
#[allow(clippy::too_many_arguments)]
pub async fn record_state(
    state: &crate::app::SharedState,
    chain: &ChainPlan,
    pending: PendingState,
    entry_body: &Value,
    entry_protocol: Protocol,
    output_items: Option<&Value>,
    final_response: Option<&Value>,
    retention_days: u32,
) {
    let stored = should_store(entry_body, retention_days);
    let sealed_body = stored.then(|| {
        let body = stored_body(entry_body, output_items, final_response);
        state.cipher.seal(body.to_string().as_bytes())
    });
    let now = crate::storage::now_unix();
    let expires_at = now + i64::from(retention_days) * 86_400;
    let record = ResponseStateRow {
        gateway_id: pending.gateway_id,
        group_id: chain.group_id.clone(),
        logical_model: chain.logical_model.clone(),
        account_id: pending.account_id,
        target_id: pending.target_id,
        endpoint: pending.endpoint,
        upstream_id: pending.upstream_id,
        sealed_body: sealed_body.transpose().ok().flatten(),
        protocol: Some(entry_protocol.as_str().to_string()),
        created_at: now,
        expires_at,
    };
    if let Err(error) = state.store.upsert_response_state(&record).await {
        tracing::warn!(%error, "保存 Responses 状态失败");
    }
}

/// 供测试与转发路径使用的辅助：中间格式请求里的最后一条用户消息文本。
pub fn last_user_text(request: &Request) -> Option<&str> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .and_then(|message| message.parts.first())
        .and_then(|part| match part {
            Part::Text(text) => Some(text.as_str()),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn store_defaults_to_true_and_false_means_false() {
        assert!(should_store(&json!({"model": "m"}), 30));
        assert!(!should_store(&json!({"model": "m", "store": false}), 30));
        // 保留期为 0 时不保存正文，无论请求怎么写（§15.2）。
        assert!(!should_store(&json!({"model": "m"}), 0));
    }

    #[test]
    fn references_are_read_from_both_fields() {
        assert_eq!(
            referenced_state(&json!({"previous_response_id": "resp_akh_a"})),
            Some("resp_akh_a")
        );
        assert_eq!(
            referenced_state(&json!({"conversation": {"id": "conv_1"}})),
            Some("conv_1")
        );
        assert_eq!(
            referenced_state(&json!({"conversation": "conv_1"})),
            Some("conv_1")
        );
        // 空引用不算引用。
        assert_eq!(referenced_state(&json!({"previous_response_id": ""})), None);
        assert_eq!(referenced_state(&json!({})), None);
    }

    #[test]
    fn the_reference_is_rewritten_to_the_upstream_id() {
        let mut body = json!({"model": "m", "previous_response_id": "resp_akh_a"});
        rewrite_reference(&mut body, "resp_upstream");
        assert_eq!(body["previous_response_id"], "resp_upstream");
        assert!(
            !body.to_string().contains("resp_akh_a"),
            "网关 ID 绝不能下发给上游"
        );

        let mut body = json!({"conversation": {"id": "resp_akh_a"}});
        rewrite_reference(&mut body, "resp_upstream");
        assert_eq!(body["conversation"]["id"], "resp_upstream");
    }

    #[test]
    fn stored_body_strips_references_and_streaming() {
        let body = json!({
            "model": "m",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "previous_response_id": "resp_akh_a",
            "stream": true,
            "store": true,
            "temperature": 0.7,
        });
        let stored = stored_body(&body, None, None);
        assert!(stored.get("previous_response_id").is_none());
        assert!(stored.get("store").is_none());
        assert!(stored.get("stream").is_none());
        assert_eq!(stored["temperature"], 0.7, "采样参数必须保留");
        assert_eq!(stored["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn output_items_are_appended_to_the_saved_input() {
        let body = json!({
            "model": "m",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
        });
        let output = json!([
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "你好"}]}
        ]);
        let stored = stored_body(&body, Some(&output), None);
        let input = stored["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "你好");
    }

    #[test]
    fn rebuild_keeps_the_same_shape_for_the_same_protocol() {
        let saved = json!({
            "model": "m",
            "input": [{"type": "message", "role": "user", "content": "旧问题"}],
            "temperature": 0.5,
        });
        let rebuilt = rebuild_body(
            &saved,
            Protocol::OpenAiResponses,
            Protocol::OpenAiResponses,
            "m2",
        )
        .unwrap();
        assert_eq!(rebuilt["model"], "m2");
        assert_eq!(rebuilt["temperature"], 0.5);
    }

    #[test]
    fn rebuild_translates_across_protocols() {
        let saved = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": "旧问题"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "旧答案"}]},
            ],
        });
        let rebuilt = rebuild_body(
            &saved,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
            "m2",
        )
        .unwrap();
        let text = rebuilt.to_string();
        assert!(text.contains("旧问题"), "{text}");
        assert!(text.contains("旧答案"), "{text}");
        assert!(
            text.contains("max_tokens"),
            "Messages 必填 max_tokens\n{text}"
        );
    }

    #[test]
    fn merge_history_appends_the_new_user_input() {
        let current = json!({
            "model": "m",
            "previous_response_id": "resp_akh_a",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "新问题"}]}],
            "max_output_tokens": 256,
        });
        let rebuilt = json!({
            "model": "旧模型",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "旧问题"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "旧答案"}]},
            ],
            "max_output_tokens": 1024,
            "temperature": 0.5,
        });
        let merged = merge_history(current, rebuilt);
        let input = merged["input"].as_array().unwrap();
        assert_eq!(input.len(), 3, "旧问题 + 旧答案 + 新问题");
        assert_eq!(merged["max_output_tokens"], 256, "本次参数覆盖历史");
        assert_eq!(merged["temperature"], 0.5, "历史独有的参数保留");
        assert!(!merged.to_string().contains("resp_akh_a"));
    }

    #[test]
    fn merge_history_handles_string_input() {
        let current = json!({"model": "m", "input": "新问题"});
        let rebuilt = json!({"model": "m", "input": "旧问题"});
        let merged = merge_history(current, rebuilt);
        let input = merged["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["content"][0]["text"], "旧问题");
        assert_eq!(input[1]["content"][0]["text"], "新问题");
    }
    #[test]
    fn merge_history_reuses_a_structured_last_item_verbatim() {
        let current = json!({
            "model": "m",
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": "带图"}],
                       "attachments": []}],
        });
        let rebuilt = json!({"model": "m", "input": []});
        let merged = merge_history(current, rebuilt);
        let input = merged["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "带图");
        assert_eq!(input[0]["attachments"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn gateway_ids_are_prefixed_and_unique() {
        let a = gateway_id();
        let b = gateway_id();
        assert!(a.starts_with(GATEWAY_PREFIX));
        assert_ne!(a, b);
    }
}
