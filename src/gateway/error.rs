//! 稳定网关错误码、HTTP 状态码映射与按下游协议的错误体（§18）。
//!
//! 状态码不是装饰：它直接决定客户端是否重试，而客户端重试是"调用不中断"
//! 的最后一道防线（§18.3）。

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::domain::Protocol;

/// 第一期定义的全部网关错误码（§18.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    AuthInvalid,
    ModelNotFound,
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

impl ErrorCode {
    /// 对外稳定的错误码字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthInvalid => "auth_invalid",
            Self::ModelNotFound => "model_not_found",
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
            Self::ModelNotFound | Self::ResponseStateExpired => StatusCode::NOT_FOUND,
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
        match self.protocol {
            Protocol::AnthropicMessages => json!({
                "type": "error",
                "error": {
                    "type": self.code.anthropic_type(),
                    "message": self.message,
                },
                "akhub_error_code": self.code.as_str(),
                "request_id": self.request_id,
            }),
            Protocol::OpenAiChat | Protocol::OpenAiResponses => json!({
                "error": {
                    "message": self.message,
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
