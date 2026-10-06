//! 稳定网关错误码、HTTP 状态码映射与按下游协议的错误体（§18）。
//!
//! 状态码不是装饰：它直接决定客户端是否重试，而客户端重试是"调用不中断"
//! 的最后一道防线（§18.3）。

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::domain::Protocol;

/// 所有供应商错误统一生成自己的响应，保留状态，不保留供应商正文或字段。
pub(crate) fn upstream_response(
    status: StatusCode,
    protocol: Protocol,
    request_id: &str,
) -> Response {
    let code = if status.is_server_error() {
        ErrorCode::UpstreamProtocolError
    } else {
        ErrorCode::UnsupportedParameter
    };
    let mut response = GatewayError::new(code, "请求未被接受，请检查模型、接口及请求参数")
        .with_protocol(protocol)
        .with_request_id(request_id)
        .into_response();
    *response.status_mut() = status;
    response
}

/// 完整错误帧不能透传；正常帧逐字保留，跨网络分块的半帧有界缓冲。
pub(crate) fn private_stream(
    body: axum::body::Body,
    protocol: Protocol,
    request_id: &str,
) -> axum::body::Body {
    use futures::StreamExt as _;
    let request_id = request_id.to_owned();
    axum::body::Body::from_stream(async_stream::stream! {
        let mut input = body.into_data_stream();
        let mut buffer = Vec::new();
        while let Some(chunk) = input.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    yield Ok::<_, std::io::Error>(crate::protocol::stream_error(protocol,
                        ErrorCode::UpstreamProtocolError, "", Some(&request_id)));
                    return;
                }
            };
            for segment in chunk.split_inclusive(|b| *b == b'\n') {
                if buffer.len().saturating_add(segment.len()) > 64 * 1024 * 1024 {
                    yield Ok(crate::protocol::stream_error(protocol,
                        ErrorCode::UpstreamProtocolError, "", Some(&request_id)));
                    return;
                }
                buffer.extend_from_slice(segment);
                if crate::protocol::sse::find_frame_end(&buffer, buffer.len().saturating_sub(4)).is_some() {
                    if is_error_frame(&buffer) {
                        tracing::warn!(request_id, "供应商流式错误已对外隐藏");
                        yield Ok(private_error_frame(&buffer, protocol, &request_id));
                        return;
                    }
                    yield Ok(axum::body::Bytes::from(std::mem::take(&mut buffer)));
                }
            }
        }
        if !buffer.is_empty() {
            if is_error_frame(&buffer) {
                yield Ok(private_error_frame(&buffer, protocol, &request_id));
            } else {
                yield Ok(axum::body::Bytes::from(buffer));
            }
        }
    })
}

fn is_error_frame(raw: &[u8]) -> bool {
    let frame = crate::protocol::sse::parse_frame(&String::from_utf8_lossy(raw));
    let payload = serde_json::from_str::<serde_json::Value>(&frame.data).ok();
    if frame.event.as_deref() == Some("error")
        || payload.as_ref().is_some_and(|p| {
            p.get("type").and_then(|v| v.as_str()) == Some("error")
                || p.get("error").is_some_and(|e| !e.is_null())
        })
    {
        return true;
    }
    matches!(
        frame.event.as_deref(),
        Some("response.failed" | "response.error")
    ) || payload.as_ref().is_some_and(|p| {
        matches!(
            p.get("type").and_then(|v| v.as_str()),
            Some("response.failed" | "response.error")
        ) || p.pointer("/response/status").and_then(|v| v.as_str()) == Some("failed")
    })
}

fn private_error_frame(raw: &[u8], protocol: Protocol, request_id: &str) -> axum::body::Bytes {
    let frame = crate::protocol::sse::parse_frame(&String::from_utf8_lossy(raw));
    let payload = serde_json::from_str::<serde_json::Value>(&frame.data).ok();
    if protocol == Protocol::OpenAiResponses
        && let Some(mut response) =
            payload.and_then(|mut p| p.get_mut("response").map(serde_json::Value::take))
        && sanitize_failure(&mut response)
    {
        return crate::protocol::sse::format_frame(
            Some("response.failed"),
            &json!({"type":"response.failed", "response":response, "request_id":request_id})
                .to_string(),
        );
    }
    crate::protocol::stream_error(
        protocol,
        ErrorCode::UpstreamProtocolError,
        "",
        Some(request_id),
    )
}

/// HTTP 200 的异步失败也不能携带供应商诊断字段。
pub(crate) fn sanitize_failure(value: &mut serde_json::Value) -> bool {
    let failed = matches!(
        value.get("status").and_then(|v| v.as_str()),
        Some("failed" | "error" | "blocked")
    ) || value.get("error").is_some_and(|v| !v.is_null());
    if failed && let Some(object) = value.as_object_mut() {
        object.retain(|key, _| {
            matches!(
                key.as_str(),
                "id" | "task_id"
                    | "object"
                    | "status"
                    | "created_at"
                    | "expires_at"
                    | "completed_at"
                    | "usage"
            )
        });
        object.insert(
            "error".into(),
            json!({"code":"upstream_protocol_error", "message":"服务响应异常，请稍后重试"}),
        );
    }
    failed
}

/// 第一期定义的全部网关错误码（§18.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    AuthInvalid,
    ModelNotFound,
    /// 异步生图任务不存在、已过期，或不属于当前分组（§14.9）。
    ImageTaskNotFound,
    NoEligibleTarget,
    MultiplierUnknown,
    MultiplierExceeded,
    QueueFull,
    QueueTimeout,
    RequestTooLarge,
    UnsupportedParameter,
    UpstreamTimeout,
    UpstreamExhausted,
    ResponseStateExpired,
    RateLimited,
    UpstreamProtocolError,
    InternalError,
}

/// 客户端在流结束前断开连接（§18.1、§24.1）。
///
/// 与 New API / sub2api 的 `end_reason=client_gone` 同义。它只会写进请求记录，
/// 不会作为响应返回给任何客户端——下游已经走了，没有收件人。
pub const CLIENT_GONE: &str = "client_gone";

impl ErrorCode {
    /// 服务端诊断与客户提示分离，不从任意供应商文本猜测渠道名称。
    pub fn public_message(self, detail: &str) -> &str {
        match self {
            Self::UpstreamTimeout => "请求处理超时，请稍后重试",
            Self::UpstreamExhausted | Self::NoEligibleTarget => "服务暂时不可用，请稍后重试",
            Self::UpstreamProtocolError => "服务响应异常，请稍后重试",
            Self::InternalError => "服务内部错误，请联系管理员并提供请求编号",
            Self::RateLimited => "请求频率超限，请稍后重试",
            Self::QueueFull => "请求队列已满，请稍后重试",
            Self::QueueTimeout => "请求排队超时，请稍后重试",
            Self::MultiplierUnknown | Self::MultiplierExceeded => "当前模型暂不可用，请联系管理员",
            _ => detail,
        }
    }
    /// 全部稳定错误码。
    ///
    /// 后台按它校验请求记录的 `error_code` 筛选参数：有了这份清单，填错一个码
    /// 会拿到明确的 400 与可选值列表，而不是一个看起来"就是没有记录"的空列表。
    pub const ALL: &'static [Self] = &[
        Self::AuthInvalid,
        Self::ModelNotFound,
        Self::ImageTaskNotFound,
        Self::NoEligibleTarget,
        Self::MultiplierUnknown,
        Self::MultiplierExceeded,
        Self::QueueFull,
        Self::QueueTimeout,
        Self::RequestTooLarge,
        Self::UnsupportedParameter,
        Self::UpstreamTimeout,
        Self::UpstreamExhausted,
        Self::ResponseStateExpired,
        Self::RateLimited,
        Self::UpstreamProtocolError,
        Self::InternalError,
    ];

    /// 只会出现在请求记录里、不会作为响应返回的结局标识（§24.1）。
    pub const RECORD_ONLY: &'static [&'static str] = &[CLIENT_GONE];

    /// 请求记录 `error_code` 列的全部合法取值（稳定错误码 + 只进记录的标识）。
    pub fn record_codes() -> Vec<&'static str> {
        ErrorCode::ALL
            .iter()
            .map(|code| code.as_str())
            .chain(ErrorCode::RECORD_ONLY.iter().copied())
            .collect()
    }

    /// 这个字符串是不是一个合法的记录错误码。
    pub fn is_record_code(code: &str) -> bool {
        ErrorCode::record_codes().contains(&code)
    }

    /// 对外稳定的错误码字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthInvalid => "auth_invalid",
            Self::ModelNotFound => "model_not_found",
            Self::ImageTaskNotFound => "image_task_not_found",
            Self::NoEligibleTarget => "no_eligible_target",
            Self::MultiplierUnknown => "multiplier_unknown",
            Self::MultiplierExceeded => "multiplier_exceeded",
            Self::QueueFull => "queue_full",
            Self::QueueTimeout => "queue_timeout",
            Self::RequestTooLarge => "request_too_large",
            Self::UnsupportedParameter => "unsupported_parameter",
            Self::UpstreamTimeout => "upstream_timeout",
            Self::UpstreamExhausted => "upstream_exhausted",
            Self::ResponseStateExpired => "response_state_expired",
            Self::RateLimited => "rate_limited",
            Self::UpstreamProtocolError => "upstream_protocol_error",
            Self::InternalError => "internal_error",
        }
    }

    /// §18.3 的映射表。可重试类返回 429/5xx，其余快速失败。
    pub fn status(self) -> StatusCode {
        match self {
            Self::QueueFull | Self::QueueTimeout | Self::RateLimited => {
                StatusCode::TOO_MANY_REQUESTS
            }
            Self::NoEligibleTarget | Self::UpstreamExhausted => StatusCode::SERVICE_UNAVAILABLE,
            Self::UpstreamTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::UpstreamProtocolError => StatusCode::BAD_GATEWAY,
            Self::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
            Self::MultiplierUnknown | Self::MultiplierExceeded => StatusCode::FORBIDDEN,
            Self::AuthInvalid => StatusCode::UNAUTHORIZED,
            Self::ModelNotFound | Self::ResponseStateExpired | Self::ImageTaskNotFound => {
                StatusCode::NOT_FOUND
            }
            Self::UnsupportedParameter => StatusCode::BAD_REQUEST,
            Self::RequestTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        }
    }

    /// 客户端是否应当退避后重试。
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::QueueFull
                | Self::QueueTimeout
                | Self::RateLimited
                | Self::NoEligibleTarget
                | Self::UpstreamExhausted
                | Self::UpstreamTimeout
                | Self::UpstreamProtocolError
                | Self::InternalError
        )
    }

    /// OpenAI 错误对象的 `type` 字段。
    fn openai_type(self) -> &'static str {
        match self.status() {
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::NOT_FOUND => "invalid_request_error",
            StatusCode::BAD_REQUEST => "invalid_request_error",
            StatusCode::PAYLOAD_TOO_LARGE => "invalid_request_error",
            StatusCode::FORBIDDEN => "permission_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            _ => "api_error",
        }
    }

    /// 流内错误帧用的 OpenAI `type`（§18.2）。与完整响应同口径。
    pub fn openai_type_for_stream(self) -> &'static str {
        self.openai_type()
    }

    /// 流内错误帧用的 Anthropic `error.type`（§18.2）。
    pub fn anthropic_type_for_stream(self) -> &'static str {
        self.anthropic_type()
    }

    /// Anthropic 错误对象的 `error.type` 字段。
    fn anthropic_type(self) -> &'static str {
        match self.status() {
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::FORBIDDEN => "permission_error",
            StatusCode::NOT_FOUND => "not_found_error",
            StatusCode::BAD_REQUEST => "invalid_request_error",
            StatusCode::PAYLOAD_TOO_LARGE => "request_too_large",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            StatusCode::GATEWAY_TIMEOUT | StatusCode::SERVICE_UNAVAILABLE => "overloaded_error",
            _ => "api_error",
        }
    }
}

/// 一个可直接返回给下游的网关错误。
#[derive(Debug, Clone)]
pub struct GatewayError {
    pub code: ErrorCode,
    pub message: String,
    /// 按下游协议决定错误体形状；未知入口时按 OpenAI 形状返回。
    pub protocol: Protocol,
    pub request_id: Option<String>,
    /// 可重试错误附带的建议等待秒数。
    pub retry_after: Option<u64>,
}

impl GatewayError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            protocol: Protocol::OpenAiChat,
            request_id: None,
            retry_after: None,
        }
    }

    pub fn with_protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = protocol;
        self
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    /// 按下游协议构造错误体（§18.2）。
    pub fn body(&self) -> serde_json::Value {
        let message = self.code.public_message(&self.message);
        match self.protocol {
            Protocol::AnthropicMessages => json!({
                "type": "error",
                "error": {
                    "type": self.code.anthropic_type(),
                    "message": message,
                },
                "akhub_error_code": self.code.as_str(),
                "request_id": self.request_id,
            }),
            Protocol::OpenAiChat | Protocol::OpenAiResponses => json!({
                "error": {
                    "message": message,
                    "type": self.code.openai_type(),
                    "code": self.code.as_str(),
                    "param": serde_json::Value::Null,
                },
                "request_id": self.request_id,
            }),
        }
    }

    /// SSE 已经开始后使用的错误事件。此时状态码已经发出，只能在流内报错。
    pub fn sse_event(&self) -> String {
        match self.protocol {
            Protocol::AnthropicMessages => {
                format!("event: error\ndata: {}\n\n", self.body())
            }
            Protocol::OpenAiChat | Protocol::OpenAiResponses => {
                format!("data: {}\n\n", self.body())
            }
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        tracing::debug!(request_id = self.request_id, code = self.code.as_str(),
            detail = %crate::security::redact::text(&self.message), "网关错误诊断");
        let mut response = (self.code.status(), axum::Json(self.body())).into_response();
        let headers = response.headers_mut();
        if let Some(request_id) = self
            .request_id
            .as_deref()
            .and_then(|v| HeaderValue::from_str(v).ok())
        {
            headers.insert("x-akhub-request-id", request_id);
        }
        // 可重试错误必须带 Retry-After，否则客户端只能盲目立即重试。
        if self.code.is_retryable()
            && let Some(seconds) = self.retry_after.or(Some(default_retry_after(self.code)))
        {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

/// 未显式给出等待时间时的保守默认值。
fn default_retry_after(code: ErrorCode) -> u64 {
    match code {
        ErrorCode::QueueFull | ErrorCode::QueueTimeout | ErrorCode::RateLimited => 5,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fragmented_native_error_frames_are_replaced_without_touching_content() {
        use axum::body::{Body, Bytes, to_bytes};
        let normal = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\r\n\r\n";
        for error in [
            "event: error\ndata: {\"error\":{\"message\":\"private-channel\"},\"provider\":\"secret\"}\n\n",
            "event: response.failed\ndata: {\"response\":{\"status\":\"failed\",\"error\":{\"message\":\"private-channel\"}}}\n\n",
            "data: {\"error\":{\"message\":\"private-channel\"}}",
        ] {
            let input = format!("{normal}{error}");
            let chunks: Vec<Result<Bytes, std::io::Error>> = input
                .as_bytes()
                .chunks(3)
                .map(|b| Ok(Bytes::copy_from_slice(b)))
                .collect();
            let body = private_stream(
                Body::from_stream(futures::stream::iter(chunks)),
                Protocol::OpenAiChat,
                "req_private",
            );
            let output = to_bytes(body, 10000).await.unwrap();
            let text = String::from_utf8(output.to_vec()).unwrap();
            assert!(text.starts_with(normal));
            assert!(text.contains("req_private"));
            assert!(text.contains("upstream_protocol_error"));
            assert!(!text.contains("private-channel"));
            assert!(!text.contains("secret"));
        }
    }

    #[test]
    fn asynchronous_failure_drops_vendor_fields() {
        let mut value = json!({"id":"task_1","status":"failed","message":"private-channel",
            "debug":"secret","error":{"provider":"private-channel"}});
        assert!(sanitize_failure(&mut value));
        assert_eq!(value["id"], "task_1");
        assert!(!value.to_string().contains("private-channel"));
        assert!(!value.to_string().contains("secret"));
    }

    #[test]
    fn upstream_details_never_reach_clients() {
        for protocol in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ] {
            for code in [
                ErrorCode::UpstreamExhausted,
                ErrorCode::UpstreamTimeout,
                ErrorCode::UpstreamProtocolError,
                ErrorCode::InternalError,
                ErrorCode::NoEligibleTarget,
                ErrorCode::RateLimited,
            ] {
                let error = GatewayError::new(code, "账号「私人渠道」 upstream.example secret")
                    .with_protocol(protocol)
                    .with_request_id("req_test");
                assert!(!error.body().to_string().contains("私人渠道"));
                assert!(!error.sse_event().contains("upstream.example"));
                assert!(error.sse_event().contains("req_test"));
            }
        }
    }

    /// 每个错误码都必须登记进 [`ErrorCode::ALL`]。
    ///
    /// 后台的 `error_code` 筛选按 `ALL` 校验，漏登记会让一个合法的筛选值被
    /// 400 拒掉。这里的 match 是穷尽的：新增错误码时它会编译失败，提醒同步。
    #[test]
    fn every_error_code_is_registered_for_the_record_filter() {
        for code in ErrorCode::ALL {
            let registered: &[ErrorCode] = match *code {
                ErrorCode::AuthInvalid
                | ErrorCode::ModelNotFound
                | ErrorCode::ImageTaskNotFound
                | ErrorCode::NoEligibleTarget
                | ErrorCode::MultiplierUnknown
                | ErrorCode::MultiplierExceeded
                | ErrorCode::QueueFull
                | ErrorCode::QueueTimeout
                | ErrorCode::RequestTooLarge
                | ErrorCode::UnsupportedParameter
                | ErrorCode::UpstreamTimeout
                | ErrorCode::UpstreamExhausted
                | ErrorCode::ResponseStateExpired
                | ErrorCode::RateLimited
                | ErrorCode::UpstreamProtocolError
                | ErrorCode::InternalError => ErrorCode::ALL,
            };
            assert!(registered.contains(code), "{code:?} 未登记进 ALL");
        }
    }

    /// 记录错误码清单必须包含流式结算写入的 `client_gone`（§24.1）。
    #[test]
    fn the_record_filter_accepts_the_abort_marker() {
        assert!(ErrorCode::is_record_code(CLIENT_GONE));
        assert!(ErrorCode::is_record_code("upstream_timeout"));
        assert!(
            !ErrorCode::is_record_code("upstream_timout"),
            "拼错的码必须被拒"
        );
        assert_eq!(ErrorCode::record_codes().len(), ErrorCode::ALL.len() + 1);
    }

    #[test]
    fn status_mapping_matches_the_specification() {
        // §18.3 可重试
        assert_eq!(ErrorCode::QueueFull.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            ErrorCode::QueueTimeout.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ErrorCode::RateLimited.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ErrorCode::NoEligibleTarget.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ErrorCode::UpstreamExhausted.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ErrorCode::UpstreamTimeout.status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            ErrorCode::UpstreamProtocolError.status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            ErrorCode::InternalError.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        // §18.3 不可重试
        assert_eq!(ErrorCode::MultiplierUnknown.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            ErrorCode::MultiplierExceeded.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(ErrorCode::AuthInvalid.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(ErrorCode::ModelNotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            ErrorCode::UnsupportedParameter.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ErrorCode::RequestTooLarge.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            ErrorCode::ResponseStateExpired.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn multiplier_errors_are_not_retryable() {
        // 走到 multiplier_unknown 说明宽限期已用完，重试是白搭（§18.3）。
        assert!(!ErrorCode::MultiplierUnknown.is_retryable());
        assert!(!ErrorCode::MultiplierExceeded.is_retryable());
        assert!(ErrorCode::NoEligibleTarget.is_retryable());
    }

    #[test]
    fn anthropic_body_uses_anthropic_shape() {
        let error = GatewayError::new(ErrorCode::ModelNotFound, "未知逻辑模型")
            .with_protocol(Protocol::AnthropicMessages);
        let body = error.body();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "not_found_error");
        assert_eq!(body["akhub_error_code"], "model_not_found");
    }

    #[test]
    fn openai_body_uses_openai_shape() {
        let error = GatewayError::new(ErrorCode::AuthInvalid, "Key 无效");
        let body = error.body();
        assert_eq!(body["error"]["type"], "authentication_error");
        assert_eq!(body["error"]["code"], "auth_invalid");
        assert!(body.get("type").is_none());
    }

    #[test]
    fn retryable_errors_carry_retry_after() {
        let response = GatewayError::new(ErrorCode::QueueFull, "队列已满").into_response();
        assert!(response.headers().contains_key(header::RETRY_AFTER));

        let response = GatewayError::new(ErrorCode::AuthInvalid, "Key 无效").into_response();
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
    }

    #[test]
    fn sse_error_events_follow_the_downstream_protocol() {
        let anthropic = GatewayError::new(ErrorCode::UpstreamProtocolError, "上游中断")
            .with_protocol(Protocol::AnthropicMessages)
            .sse_event();
        assert!(anthropic.starts_with("event: error\ndata: "));
        assert!(anthropic.ends_with("\n\n"));

        let openai = GatewayError::new(ErrorCode::UpstreamProtocolError, "上游中断").sse_event();
        assert!(openai.starts_with("data: "));
    }
}
