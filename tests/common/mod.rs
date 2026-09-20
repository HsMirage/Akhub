//! 端到端测试共用的夹具：可编排的假上游与一台完整的 Akhub。
//!
//! 假上游不 mock 网关内部，只扮演站点：按脚本返回成功、指定状态码、挂起、
//! 流式中断或错误事件，并把收到的每个请求记录下来供断言。

#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use akhub::app::{AppState, Settings, SharedState};
use akhub::domain::{
    Account, DispatchTarget, Group, Limits, LogicalModel, ModelOrigin, Multiplier, MultiplierMode,
    Protocol, SchedulingWeights, UpstreamType,
};
use akhub::storage::store::{AccountSecrets, ids};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::watch;

/// 假上游对一次推理请求的反应。
#[derive(Debug, Clone)]
pub enum Behavior {
    /// 按协议返回一段正常响应（流式或非流式由请求体决定）。
    Ok,
    /// 返回固定状态码，可带 `Retry-After`（秒）。
    Status(u16, Option<u64>),
    /// 返回固定状态码与自定义 JSON 体：模拟上游显式拒绝某能力的错误形状。
    Json(u16, Value),
    /// 挂起到 [`FakeUpstream::release`] 被调用，模拟慢请求或满载。
    Hang,
    /// 流式：先送一个有语义的增量，然后连接中断。
    StreamThenAbort,
    /// 流式：先发送语义增量，等待 release 后再发剩余内容。
    StreamThenHang,
    /// 流式：只送协议开始标记与一个错误事件，然后结束。
    StreamErrorEvent,
    /// 按协议返回一次工具调用，用于验证跨协议的工具往返。
    ToolCall,
}

/// 假上游收到的一次请求。
#[derive(Debug, Clone)]
pub struct Seen {
    pub path: String,
    pub body: Value,
    pub headers: HeaderMap,
}

#[derive(Clone)]
pub struct FakeUpstream {
    pub base_url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    script: Arc<Mutex<VecDeque<Behavior>>>,
    fallback: Arc<Mutex<Behavior>>,
    release: watch::Sender<bool>,
    /// `/v1/sub2api/billing` 的响应体；`None` 时返回 500。
    billing: Arc<Mutex<Option<Value>>>,
    /// `/api/user/self/groups` 的响应体；`None` 时返回 500。
    groups: Arc<Mutex<Option<Value>>>,
    /// `/v1/models` 的响应体；`None` 时返回 500（§16.1）。
    models: Arc<Mutex<Option<Value>>>,
}

impl FakeUpstream {
    /// 启动一台默认一切正常的假上游。
    pub async fn spawn() -> Self {
        let (release, _) = watch::channel(false);
        let upstream = Self {
            base_url: String::new(),
            seen: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(VecDeque::new())),
            fallback: Arc::new(Mutex::new(Behavior::Ok)),
            release,
            billing: Arc::new(Mutex::new(None)),
            groups: Arc::new(Mutex::new(None)),
            models: Arc::new(Mutex::new(None)),
        };
        let app = Router::new()
            .route("/v1/messages", post(inference))
            .route("/v1/messages/count_tokens", post(inference))
            .route("/v1/chat/completions", post(inference))
            .route("/v1/responses", post(inference))
            .route("/v1/models", get(models))
            .route("/v1/sub2api/billing", get(billing))
            .route("/api/user/self/groups", get(groups))
            .with_state(upstream.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base_url: format!("http://{addr}"),
            ..upstream
        }
    }

    /// 接下来的请求依次按这些行为处理，用完后回到默认行为。
    pub fn script(&self, behaviors: impl IntoIterator<Item = Behavior>) -> &Self {
        self.script.lock().unwrap().extend(behaviors);
        self
    }

    /// 脚本用尽后的默认行为。
    pub fn fallback(&self, behavior: Behavior) -> &Self {
        *self.fallback.lock().unwrap() = behavior;
        self
    }

    /// 放行所有挂起中的请求。
    pub fn release(&self) {
        let _ = self.release.send(true);
    }

    pub fn set_billing(&self, body: Option<Value>) {
        *self.billing.lock().unwrap() = body;
    }

    pub fn set_groups(&self, body: Option<Value>) {
        *self.groups.lock().unwrap() = body;
    }

    pub fn set_models(&self, body: Option<Value>) {
        *self.models.lock().unwrap() = body;
    }

    pub fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// 等到假上游收到第 `count` 个请求，用于同步"请求已经挂起"这类状态。
    pub async fn wait_for_requests(&self, count: usize) {
        for _ in 0..200 {
            if self.requests() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("假上游在预期时间内没有收到第 {count} 个请求");
    }

    fn next_behavior(&self) -> Behavior {
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.fallback.lock().unwrap().clone())
    }
}

async fn inference(
    State(upstream): State<FakeUpstream>,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    upstream.seen.lock().unwrap().push(Seen {
        path: uri.path().to_string(),
        body: parsed.clone(),
        headers,
    });
    let streaming = parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = uri.path().to_string();

    match upstream.next_behavior() {
        Behavior::Ok => {
            if streaming {
                sse(&path, &ok_frames(&path))
            } else {
                axum::Json(ok_body(&path, &parsed)).into_response()
            }
        }
        Behavior::Status(code, retry_after) => {
            let status = StatusCode::from_u16(code).unwrap();
            let body = json!({"error": {"message": format!("fake upstream {code}")}});
            let mut response = (status, axum::Json(body)).into_response();
            if let Some(seconds) = retry_after {
                response
                    .headers_mut()
                    .insert("retry-after", seconds.to_string().parse().unwrap());
            }
            response
        }
        Behavior::Json(code, body) => {
            (StatusCode::from_u16(code).unwrap(), axum::Json(body)).into_response()
        }
        Behavior::Hang => {
            let mut released = upstream.release.subscribe();
            while !*released.borrow() {
                if released.changed().await.is_err() {
                    break;
                }
            }
            if streaming {
                sse(&path, &ok_frames(&path))
            } else {
                axum::Json(ok_body(&path, &parsed)).into_response()
            }
        }
        Behavior::StreamThenHang => {
            let frames = ok_frames(&path);
            let first = axum::body::Bytes::from(frames[..2].concat());
            let rest = axum::body::Bytes::from(frames[2..].concat());
            let mut released = upstream.release.subscribe();
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(first);
                while !*released.borrow() {
                    if released.changed().await.is_err() {
                        return;
                    }
                }
                yield Ok(rest);
            };
            (
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        Behavior::StreamThenAbort => {
            let frames = ok_frames(&path);
            let first = axum::body::Bytes::from(frames[..2].concat());
            // 先让第一块真正刷到网络上，再断开：紧挨着报错会被 hyper 整体丢弃，
            // 对端看到的就只是一次连接失败，而不是"内容之后中断"。
            let stream = futures::stream::unfold(0u8, move |step| {
                let first = first.clone();
                async move {
                    match step {
                        0 => Some((Ok::<_, std::io::Error>(first), 1)),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Some((Err(std::io::Error::other("connection reset")), 2))
                        }
                        _ => None,
                    }
                }
            });
            (
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        Behavior::StreamErrorEvent => {
            let frames = [
                ok_frames(&path)[0].clone(),
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n".to_string(),
            ];
            sse(&path, &frames)
        }
        Behavior::ToolCall => {
            if streaming {
                sse(&path, &tool_frames(&path))
            } else {
                axum::Json(tool_body(&path, &parsed)).into_response()
            }
        }
    }
}

async fn billing(State(upstream): State<FakeUpstream>, headers: HeaderMap) -> Response {
    upstream.seen.lock().unwrap().push(Seen {
        path: "/v1/sub2api/billing".into(),
        body: Value::Null,
        headers,
    });
    match upstream.billing.lock().unwrap().clone() {
        Some(body) => axum::Json(body).into_response(),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn groups(State(upstream): State<FakeUpstream>, headers: HeaderMap) -> Response {
    upstream.seen.lock().unwrap().push(Seen {
        path: "/api/user/self/groups".into(),
        body: Value::Null,
        headers,
    });
    match upstream.groups.lock().unwrap().clone() {
        Some(body) => axum::Json(body).into_response(),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn models(State(upstream): State<FakeUpstream>, headers: HeaderMap) -> Response {
    upstream.seen.lock().unwrap().push(Seen {
        path: "/v1/models".into(),
        body: Value::Null,
        headers,
    });
    match upstream.models.lock().unwrap().clone() {
        Some(body) => axum::Json(body).into_response(),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn sse(_path: &str, frames: &[String]) -> Response {
    ([("content-type", "text/event-stream")], frames.concat()).into_response()
}

/// 按协议给出一段最小但形状正确的流：开始标记、一个语义增量、结束。
fn ok_frames(path: &str) -> Vec<String> {
    if path.starts_with("/v1/chat/completions") {
        vec![
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n".into(),
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\n".into(),
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".into(),
            "data: [DONE]\n\n".into(),
        ]
    } else if path.starts_with("/v1/responses") {
        vec![
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n".into(),
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\n\n".into(),
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"output_tokens\":2}}}\n\n".into(),
        ]
    } else {
        vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n".into(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n".into(),
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n".into(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into(),
        ]
    }
}

/// 一次工具调用的流式形状：调用 ID、工具名与分片的参数。
fn tool_frames(path: &str) -> Vec<String> {
    if path.starts_with("/v1/chat/completions") {
        vec![
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n".into(),
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"weather\",\"arguments\":\"{\\\"city\\\"\"}}]}}]}\n\n".into(),
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"北京\\\"}\"}}]}}]}\n\n".into(),
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".into(),
            "data: [DONE]\n\n".into(),
        ]
    } else if path.starts_with("/v1/responses") {
        vec![
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n\n".into(),
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"weather\"}}\n\n".into(),
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"city\\\"\"}\n\n".into(),
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\":\\\"北京\\\"}\"}\n\n".into(),
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0}\n\n".into(),
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"function_call\"}],\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n".into(),
        ]
    } else {
        vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\"}}\n\n".into(),
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"weather\"}}\n\n".into(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\"\"}}\n\n".into(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"北京\\\"}\"}}\n\n".into(),
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".into(),
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":2}}\n\n".into(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into(),
        ]
    }
}

/// 一次工具调用的非流式形状。
fn tool_body(path: &str, request: &Value) -> Value {
    let model = request.get("model").cloned().unwrap_or(Value::Null);
    if path.starts_with("/v1/chat/completions") {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": model,
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": Value::Null,
                "tool_calls": [{"id": "call_1", "type": "function", "function": {
                    "name": "weather", "arguments": "{\"city\":\"北京\"}"
                }}]
            }}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2},
        })
    } else if path.starts_with("/v1/responses") {
        json!({
            "id": "resp_1",
            "object": "response",
            "model": model,
            "status": "completed",
            "output": [{"type": "function_call", "call_id": "call_1",
                        "name": "weather", "arguments": "{\"city\":\"北京\"}"}],
            "usage": {"input_tokens": 10, "output_tokens": 2},
        })
    } else {
        json!({
            "id": "msg_1",
            "type": "message",
            "model": model,
            "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "call_1", "name": "weather",
                         "input": {"city": "北京"}}],
            "usage": {"input_tokens": 10, "output_tokens": 2},
        })
    }
}

fn ok_body(path: &str, request: &Value) -> Value {
    let model = request.get("model").cloned().unwrap_or(Value::Null);
    if path.starts_with("/v1/chat/completions") {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "你好"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12},
        })
    } else if path.starts_with("/v1/responses") {
        json!({
            "id": "resp_1",
            "object": "response",
            "model": model,
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "你好"}]}],
            "usage": {"input_tokens": 10, "output_tokens": 2},
        })
    } else {
        json!({
            "id": "msg_1",
            "type": "message",
            "model": model,
            "content": [{"type": "text", "text": "你好"}],
            "usage": {"input_tokens": 10, "output_tokens": 2},
        })
    }
}

// ------------------------------------------------------------------ Akhub

/// 一台跑起来的 Akhub。
pub struct Akhub {
    pub base_url: String,
    /// 分组的下游 Key 明文。
    pub key: String,
    pub group_id: String,
    pub state: SharedState,
    dir: tempfile::TempDir,
}

impl Akhub {
    /// 数据目录，供"重启"测试在同一目录上再次启动。
    pub fn data_dir(&self) -> &std::path::Path {
        self.dir.path()
    }
}

/// 用已有的状态再起一台 HTTP 服务，模拟重启后的进程。
pub async fn serve(state: SharedState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let router = akhub::server::router(state);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// 用默认设置启动 Akhub，建一个倍率上限为 1 的分组。
pub async fn spawn_akhub() -> Akhub {
    spawn_akhub_with(Settings::default(), |_| {}).await
}

/// 自定义系统设置与分组参数。
pub async fn spawn_akhub_with(settings: Settings, configure: impl FnOnce(&mut Group)) -> Akhub {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap(dir.path(), settings).await.unwrap();

    let (key, prefix) = akhub::security::generate_group_key().unwrap();
    let mut group = Group {
        id: ids::group(),
        name: "主力".into(),
        key_prefix: prefix,
        key_digest_hex: state.key_digest.digest_hex(&key),
        multiplier_limit: Multiplier::ONE,
        weights: SchedulingWeights::default(),
        queue_capacity: 100,
        max_wait_secs: 60,
        allow_managed_background: false,
        allow_degrade: true,
        created_at: OffsetDateTime::now_utc(),
    };
    configure(&mut group);
    state.store.insert_group(&group).await.unwrap();
    state.reload_config().await.unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let router = akhub::server::router(Arc::clone(&state));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    Akhub {
        base_url: format!("http://{addr}"),
        key: key.to_string(),
        group_id: group.id,
        state,
        dir,
    }
}

/// 一个"账号 + 调度目标"的描述。
pub struct TargetSpec<'a> {
    pub name: &'a str,
    pub base_url: &'a str,
    pub protocol: Protocol,
    pub logical_model: &'a str,
    pub upstream_model: &'a str,
    pub priority: i32,
    pub limits: Limits,
    pub multiplier_mode: MultiplierMode,
    pub manual_multiplier: &'a str,
    /// 是否允许运行时选择更合适的原生端点（§14.2）。
    pub adaptive: bool,
    /// New API 探针凭据：（访问令牌，用户 ID，分组名）。
    pub new_api: Option<(&'a str, &'a str, Option<&'a str>)>,
}

impl<'a> TargetSpec<'a> {
    pub fn new(
        name: &'a str,
        base_url: &'a str,
        protocol: Protocol,
        logical_model: &'a str,
        upstream_model: &'a str,
        priority: i32,
    ) -> Self {
        Self {
            name,
            base_url,
            protocol,
            logical_model,
            upstream_model,
            priority,
            limits: Limits::default(),
            multiplier_mode: MultiplierMode::Manual,
            manual_multiplier: "1",
            adaptive: true,
            new_api: None,
        }
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// 关掉运行时适配：只用首选端点，不去猜别的（§14.2）。
    pub fn pinned(mut self) -> Self {
        self.adaptive = false;
        self
    }

    pub fn multiplier(mut self, mode: MultiplierMode, manual: &'a str) -> Self {
        self.multiplier_mode = mode;
        self.manual_multiplier = manual;
        self
    }

    pub fn new_api(mut self, token: &'a str, user_id: &'a str, group: Option<&'a str>) -> Self {
        self.new_api = Some((token, user_id, group));
        self
    }
}

/// 已接线的账号与目标。
pub struct Wired {
    pub account_id: String,
    pub target_id: String,
    /// 该账号的上游 API Key 明文，形如 `key-<账号名>`，供假上游按请求头归因。
    pub api_key: String,
}

/// 在分组下建一个账号，并把它接到一个逻辑模型上。
pub async fn wire_target(akhub: &Akhub, spec: TargetSpec<'_>) -> Wired {
    let state = &akhub.state;
    let api_key = format!("key-{}", spec.name);
    let account = Account {
        id: ids::account(),
        group_id: akhub.group_id.clone(),
        name: spec.name.into(),
        upstream_type: UpstreamType::OpenAiCompatible,
        base_url: spec.base_url.into(),
        preferred_protocol: spec.protocol,
        adaptive_protocol: spec.adaptive,
        default_priority: spec.priority,
        calibration: Multiplier::ONE,
        multiplier_mode: spec.multiplier_mode,
        manual_multiplier: Multiplier::parse(spec.manual_multiplier).unwrap(),
        new_api_user_id: spec.new_api.map(|(_, user, _)| user.to_string()),
        new_api_group: spec
            .new_api
            .and_then(|(_, _, group)| group.map(str::to_string)),
        limits: spec.limits,
        // 假上游监听在 127.0.0.1，必须显式开启内网访问才能通过 SSRF 检查。
        allow_private_network: true,
        enabled: true,
        hide_original: false,
        auto_sync: false,
        model_synced_at: None,
        created_at: OffsetDateTime::now_utc(),
    };
    let secrets = AccountSecrets::new(
        state.cipher.seal(api_key.as_bytes()).unwrap(),
        spec.new_api
            .map(|(token, _, _)| state.cipher.seal(token.as_bytes()).unwrap()),
    );
    state
        .store
        .insert_account(&account, &secrets)
        .await
        .unwrap();

    let target_id =
        wire_extra_target(akhub, &account.id, spec.logical_model, spec.upstream_model).await;
    Wired {
        account_id: account.id,
        target_id,
        api_key,
    }
}

/// 给已有账号再加一个调度目标（同一把 Key 映射多个模型的场景）。返回目标 ID。
pub async fn wire_extra_target(
    akhub: &Akhub,
    account_id: &str,
    logical_model: &str,
    upstream_model: &str,
) -> String {
    let state = &akhub.state;
    let existing = state
        .store
        .list_logical_models()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.name == logical_model);
    let model_id = match existing {
        Some(model) => model.id,
        None => {
            let model = LogicalModel {
                id: ids::logical_model(),
                group_id: akhub.group_id.clone(),
                name: logical_model.into(),
                origin: ModelOrigin::Manual,
                enabled: true,
                created_at: OffsetDateTime::now_utc(),
            };
            state.store.insert_logical_model(&model).await.unwrap();
            model.id
        }
    };

    let target = DispatchTarget {
        id: ids::target(),
        logical_model_id: model_id,
        account_id: account_id.into(),
        upstream_model: upstream_model.into(),
        hide_original: false,
        priority_override: None,
        limits: Limits::default(),
        enabled: true,
        created_at: OffsetDateTime::now_utc(),
    };
    state.store.insert_target(&target).await.unwrap();
    state.reload_config().await.unwrap();
    target.id
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder().build().unwrap()
}

/// 发一个 OpenAI Chat 请求。返回的 future 不借用 `akhub`，可以直接 `spawn`。
pub fn chat(
    akhub: &Akhub,
    body: Value,
) -> impl std::future::Future<Output = reqwest::Response> + Send + 'static {
    chat_at(&akhub.base_url, &akhub.key, body)
}

/// 向指定地址发一个 OpenAI Chat 请求。
pub fn chat_at(
    base_url: &str,
    key: &str,
    body: Value,
) -> impl std::future::Future<Output = reqwest::Response> + Send + 'static {
    let url = format!("{base_url}/v1/chat/completions");
    let key = key.to_string();
    async move {
        client()
            .post(url)
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

/// 发一个 Anthropic Messages 请求。
pub fn messages(
    akhub: &Akhub,
    body: Value,
) -> impl std::future::Future<Output = reqwest::Response> + Send + 'static {
    let url = format!("{}/v1/messages", akhub.base_url);
    let key = akhub.key.clone();
    async move {
        client()
            .post(url)
            .header("x-api-key", key)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

/// 发一个 OpenAI Responses 请求。
pub fn responses(
    akhub: &Akhub,
    body: Value,
) -> impl std::future::Future<Output = reqwest::Response> + Send + 'static {
    let url = format!("{}/v1/responses", akhub.base_url);
    let key = akhub.key.clone();
    async move {
        client()
            .post(url)
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

/// 按下游协议发一个请求，供跨协议矩阵测试逐个方向调用。
pub fn request(
    akhub: &Akhub,
    protocol: Protocol,
    body: Value,
) -> impl std::future::Future<Output = reqwest::Response> + Send + 'static {
    match protocol {
        Protocol::OpenAiChat => Box::pin(chat(akhub, body))
            as std::pin::Pin<Box<dyn std::future::Future<Output = reqwest::Response> + Send>>,
        Protocol::OpenAiResponses => Box::pin(responses(akhub, body)),
        Protocol::AnthropicMessages => Box::pin(messages(akhub, body)),
    }
}

/// 一个足够大的请求体，让粘性等待预算落到 12 秒档（24–64 KB）。
pub fn bulky_chat_body(system: &str, user: &str) -> Value {
    json!({
        "model": "glm-4.6",
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": format!("{user}{}", "字".repeat(12 * 1024))},
        ],
    })
}

// ------------------------------------------------------------------ 假 GitHub Release

/// 假的 GitHub Release API：给版本检查与自更新测试用。
///
/// 只实现真实存在的三个路由：`/releases/latest`、`/releases/tags/{tag}` 与资产下载。
/// 资产 URL 指向这台假服务器，被测代码不会真的去打扰 github.com。
pub struct FakeGithub {
    pub base_url: String,
    /// `/releases/latest` 被请求的次数：用来验证缓存确实生效。
    hits: Arc<Mutex<usize>>,
}

#[derive(Clone)]
struct GithubState {
    tag: String,
    base: String,
    archive: Arc<Vec<u8>>,
    checksums: String,
    hits: Arc<Mutex<usize>>,
}

impl FakeGithub {
    /// 起一台假 GitHub。archive 是 Release 资产字节，checksums.txt 按它现算。
    pub async fn spawn(tag: &str, archive: Vec<u8>) -> Self {
        Self::spawn_inner(tag, archive, None).await
    }

    /// 指定 checksums.txt 里那个哈希：传一个错的来验证「校验不过就拒绝更新」。
    pub async fn spawn_with_checksum(tag: &str, archive: Vec<u8>, checksum: &str) -> Self {
        Self::spawn_inner(tag, archive, Some(checksum.to_string())).await
    }

    async fn spawn_inner(tag: &str, archive: Vec<u8>, checksum: Option<String>) -> Self {
        async fn latest(State(state): State<GithubState>) -> axum::Json<Value> {
            *state.hits.lock().unwrap() += 1;
            axum::Json(release_json(&state.tag, &state.base))
        }
        async fn by_tag(State(state): State<GithubState>) -> axum::Json<Value> {
            axum::Json(release_json(&state.tag, &state.base))
        }
        async fn asset(State(state): State<GithubState>) -> Response {
            (
                [(axum::http::header::CONTENT_TYPE, "application/gzip")],
                state.archive.as_ref().clone(),
            )
                .into_response()
        }
        async fn checksums(State(state): State<GithubState>) -> Response {
            (
                [(axum::http::header::CONTENT_TYPE, "text/plain")],
                state.checksums.clone(),
            )
                .into_response()
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let version = tag.trim_start_matches('v');
        let suffix = akhub::update::asset_suffix().expect("测试平台必须在发行矩阵内");
        let checksums = {
            let hash = checksum.unwrap_or_else(|| {
                use sha2::Digest as _;
                hex::encode(sha2::Sha256::digest(&archive))
            });
            format!("{hash}  akhub-v{version}-{suffix}.tar.gz\n")
        };

        let hits = Arc::new(Mutex::new(0));
        let state = GithubState {
            tag: tag.to_string(),
            base,
            archive: Arc::new(archive),
            checksums: checksums.clone(),
            hits: Arc::clone(&hits),
        };
        let app = Router::new()
            .route("/releases/latest", get(latest))
            .route("/releases/tags/{tag}", get(by_tag))
            .route("/asset", get(asset))
            .route("/checksums.txt", get(checksums))
            .with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            base_url: format!("http://{addr}"),
            hits,
        }
    }

    /// `/releases/latest` 被请求过几次。
    pub fn hits(&self) -> usize {
        *self.hits.lock().unwrap()
    }
}

/// 一台 Release 的 JSON：资产覆盖全部发行平台，下载地址都指向假服务器。
fn release_json(tag: &str, base: &str) -> Value {
    let version = tag.trim_start_matches('v');
    let mut assets: Vec<Value> = [
        "linux-x86_64-musl",
        "linux-x86_64",
        "linux-aarch64",
        "macos-aarch64",
        "macos-x86_64",
    ]
    .iter()
    .map(|suffix| {
        json!({
            "name": format!("akhub-v{version}-{suffix}.tar.gz"),
            "browser_download_url": format!("{base}/asset"),
        })
    })
    .collect();
    assets.push(json!({
        "name": "checksums.txt",
        "browser_download_url": format!("{base}/checksums.txt"),
    }));
    json!({
        "tag_name": tag,
        "name": tag,
        "html_url": format!("https://example.invalid/releases/{tag}"),
        "published_at": "2026-09-20T11:10:55Z",
        "body": "本轮发布说明",
        "assets": assets,
    })
}

/// 造一个与真实发行包同构的归档：内层目录 == 资产名去掉扩展名，里面是 `akhub`。
pub fn make_release_archive(version: &str, binary: &[u8]) -> Vec<u8> {
    let suffix = akhub::update::asset_suffix().expect("测试平台必须在发行矩阵内");
    let stem = format!("akhub-v{version}-{suffix}");
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    builder
        .append_data(&mut header, format!("{stem}/akhub"), binary)
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}
