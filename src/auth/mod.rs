//! 下游分组 Key 鉴权与管理员会话。

pub mod session;

use axum::http::HeaderMap;

use crate::config::GroupView;
use crate::domain::Protocol;
use crate::gateway::error::{ErrorCode, GatewayError};
use crate::security::KeyDigest;

/// 下游客户端呈递的凭据，以及它使用的鉴权头形态。
///
/// 头部形态同时决定 `/v1/models` 的响应形状（§7.3），所以必须一路带下去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub key: String,
    /// 客户端使用了 `x-api-key`，据此判定为 Anthropic 客户端。
    pub anthropic_style: bool,
}

/// 从请求头中取出分组 Key（§7.2）。
///
/// 同时接受 `Authorization: Bearer` 与 `x-api-key`；两者都存在且值不同时
/// 直接报错，绝不猜测该用哪一个。
pub fn extract_credential(headers: &HeaderMap) -> Result<Credential, GatewayError> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let trimmed = v.trim();
            trimmed
                .strip_prefix("Bearer ")
                .or_else(|| trimmed.strip_prefix("bearer "))
                .map(str::trim)
        })
        .filter(|v| !v.is_empty());

    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());

    match (bearer, api_key) {
        (Some(bearer), Some(api_key)) if bearer != api_key => Err(GatewayError::new(
            ErrorCode::AuthInvalid,
            "Authorization 与 x-api-key 同时存在且不一致，请只提供一个",
        )),
        // 两者一致时按 x-api-key 判定为 Anthropic 客户端（§7.3）。
        (Some(_), Some(api_key)) => Ok(Credential {
            key: api_key.to_string(),
            anthropic_style: true,
        }),
        (None, Some(api_key)) => Ok(Credential {
            key: api_key.to_string(),
            anthropic_style: true,
        }),
        (Some(bearer), None) => Ok(Credential {
            key: bearer.to_string(),
            anthropic_style: false,
        }),
        (None, None) => Err(GatewayError::new(
            ErrorCode::AuthInvalid,
            "缺少分组 Key，请提供 Authorization: Bearer 或 x-api-key",
        )),
    }
}

/// 校验凭据并定位分组。Key 唯一对应一个分组，因此调度天然被限制在组内。
pub fn authenticate<'a>(
    config: &'a crate::config::RuntimeConfig,
    digest: &KeyDigest,
    credential: &Credential,
) -> Result<&'a std::sync::Arc<GroupView>, GatewayError> {
    config
        .group_by_key_digest(&digest.digest_hex(&credential.key))
        .ok_or_else(|| GatewayError::new(ErrorCode::AuthInvalid, "分组 Key 无效"))
}

/// 客户端偏好的错误体形状。未知入口时按鉴权头推断。
pub fn error_protocol(credential: Option<&Credential>) -> Protocol {
    match credential {
        Some(c) if c.anthropic_style => Protocol::AnthropicMessages,
        _ => Protocol::OpenAiChat,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn accepts_bearer_as_an_openai_style_client() {
        let credential =
            extract_credential(&headers(&[("authorization", "Bearer akh-abc")])).unwrap();
        assert_eq!(credential.key, "akh-abc");
        assert!(!credential.anthropic_style);
    }

    #[test]
    fn accepts_x_api_key_as_an_anthropic_style_client() {
        let credential = extract_credential(&headers(&[("x-api-key", "akh-abc")])).unwrap();
        assert_eq!(credential.key, "akh-abc");
        assert!(credential.anthropic_style);
    }

    #[test]
    fn conflicting_headers_are_rejected_rather_than_guessed() {
        let error = extract_credential(&headers(&[
            ("authorization", "Bearer akh-a"),
            ("x-api-key", "akh-b"),
        ]))
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::AuthInvalid);
    }

    #[test]
    fn identical_headers_resolve_to_the_anthropic_shape() {
        let credential = extract_credential(&headers(&[
            ("authorization", "Bearer akh-a"),
            ("x-api-key", "akh-a"),
        ]))
        .unwrap();
        assert!(
            credential.anthropic_style,
            "两者相同时按 x-api-key 判定（§7.3）"
        );
    }

    #[test]
    fn missing_or_empty_credentials_are_rejected() {
        assert_eq!(
            extract_credential(&HeaderMap::new()).unwrap_err().code,
            ErrorCode::AuthInvalid
        );
        assert_eq!(
            extract_credential(&headers(&[("authorization", "Bearer ")]))
                .unwrap_err()
                .code,
            ErrorCode::AuthInvalid
        );
    }

    #[test]
    fn error_shape_follows_the_authentication_header() {
        let anthropic = Credential {
            key: "k".into(),
            anthropic_style: true,
        };
        assert_eq!(
            error_protocol(Some(&anthropic)),
            Protocol::AnthropicMessages
        );
        assert_eq!(error_protocol(None), Protocol::OpenAiChat);
    }
}
