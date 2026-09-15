//! 上游 HTTP 客户端、端点 URL 与请求头构造（§14.7、§19.4）。

pub mod endpoints;
pub mod evidence;

use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Url;
use thiserror::Error;

use crate::domain::Protocol;

/// Anthropic 未显式指定版本时使用的默认值。
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// 允许按白名单转发给上游的供应商协议头（§14.7）。
const ANTHROPIC_PASSTHROUGH_HEADERS: &[&str] = &["anthropic-beta"];
const OPENAI_PASSTHROUGH_HEADERS: &[&str] =
    &["openai-beta", "openai-organization", "openai-project"];

#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("Base URL 非法：{0}")]
    InvalidBaseUrl(String),
    #[error("凭据中包含无法作为请求头发送的字符")]
    InvalidCredential,
    #[error("上游返回状态码 {0}")]
    BadStatus(u16),
    #[error("上游模型列表格式异常：{0}")]
    BadResponse(String),
    #[error("上游模型列表超过大小上限")]
    TooLarge,
}

/// 共享的上游 HTTP 客户端。
///
/// `reqwest::Client` 内部按 Origin 维护连接池且克隆开销极低，所以整个进程
/// 只需要两个实例：默认拒绝环回/内网/云元数据网段，账号显式开启
/// `allow_private_network` 时使用另一个。
///
/// 关键在于 SSRF 校验发生在**解析器内部**：连接实际使用的地址与校验过的
/// 地址是同一批，中间没有第二次 DNS 解析，因此不存在 DNS Rebinding 的窗口
/// （§23.3、§26.8）。字面量 IP 不经过解析器，由保存时与发请求前的地址检查
/// 覆盖。
#[derive(Clone)]
pub struct UpstreamClient {
    deny_private: reqwest::Client,
    allow_private: reqwest::Client,
}

impl UpstreamClient {
    /// 构造两个客户端。禁止重定向——带 Key 的请求跟随重定向会把凭据泄漏到
    /// 另一个 Origin（§23.3）。
    pub fn new() -> reqwest::Result<Self> {
        Ok(Self {
            deny_private: build_client(GuardedResolver {
                allow_private: false,
            })?,
            allow_private: build_client(GuardedResolver {
                allow_private: true,
            })?,
        })
    }

    /// 按账号配置选择客户端。
    pub fn http_for(&self, allow_private: bool) -> &reqwest::Client {
        if allow_private {
            &self.allow_private
        } else {
            &self.deny_private
        }
    }

    /// 默认客户端：用于探针等不带账号语义、必须禁内网的调用。
    pub fn http(&self) -> &reqwest::Client {
        &self.deny_private
    }
}

/// 带统一配置的客户端构造。
fn build_client(resolver: GuardedResolver) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("akhub/", env!("CARGO_PKG_VERSION")))
        .dns_resolver(resolver)
        .build()
}

/// 解析阶段即完成地址校验的 DNS 解析器（§23.3）。
struct GuardedResolver {
    allow_private: bool,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let allow_private = self.allow_private;
        Box::pin(async move {
            let resolved = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
            let mut addresses = Vec::new();
            for address in resolved {
                if !allow_private && crate::security::url_guard::is_blocked(address.ip()) {
                    return Err(
                        format!("目标地址 {} 属于环回、内网或云元数据网段", address.ip()).into(),
                    );
                }
                addresses.push(address);
            }
            if addresses.is_empty() {
                return Err("主机没有解析出任何地址".into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// 上游端点的标准路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
    ChatCompletions,
    Responses,
    Messages,
    CountTokens,
    /// `POST /v1/responses/compact`（§15.4）：只能原生转发，没有跨协议等价物。
    ResponsesCompact,
    /// `POST /v1/responses/input_tokens`（§15.4）：同上。
    ResponsesInputTokens,
    /// `POST /v1/images/generations`：只能原生转发，没有跨协议等价物。
    ImagesGenerations,
    /// `POST /v1/images/edits`：只能原生转发，没有跨协议等价物。
    ImagesEdits,
}

impl Endpoint {
    /// 三个推理端点。辅助端点（计数、Responses 辅助操作与图片接口）不在其中：
    /// 它们没有跨协议等价物（§15.4、§15.5）。
    pub const INFERENCE: [Endpoint; 3] = [Self::ChatCompletions, Self::Responses, Self::Messages];

    /// 某个协议的原生推理端点。
    pub fn native(protocol: Protocol) -> Self {
        match protocol {
            Protocol::OpenAiChat => Self::ChatCompletions,
            Protocol::OpenAiResponses => Self::Responses,
            Protocol::AnthropicMessages => Self::Messages,
        }
    }

    /// 只能原生转发、没有跨协议等价物的辅助端点。
    pub fn is_native_only(self) -> bool {
        matches!(
            self,
            Self::CountTokens
                | Self::ResponsesCompact
                | Self::ResponsesInputTokens
                | Self::ImagesGenerations
                | Self::ImagesEdits
        )
    }

    /// 该端点所属的协议。
    pub fn protocol(self) -> Protocol {
        match self {
            Self::ChatCompletions => Protocol::OpenAiChat,
            Self::Responses | Self::ResponsesCompact | Self::ResponsesInputTokens => {
                Protocol::OpenAiResponses
            }
            Self::Messages | Self::CountTokens => Protocol::AnthropicMessages,
            Self::ImagesGenerations | Self::ImagesEdits => Protocol::OpenAiChat,
        }
    }

    /// 相对于 API 根的标准路径。
    pub fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "v1/chat/completions",
            Self::Responses => "v1/responses",
            Self::Messages => "v1/messages",
            Self::CountTokens => "v1/messages/count_tokens",
            Self::ResponsesCompact => "v1/responses/compact",
            Self::ResponsesInputTokens => "v1/responses/input_tokens",
            Self::ImagesGenerations => "v1/images/generations",
            Self::ImagesEdits => "v1/images/edits",
        }
    }

    /// 后台展示与请求记录里用的稳定标识。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
            Self::CountTokens => "count_tokens",
            Self::ResponsesCompact => "responses_compact",
            Self::ResponsesInputTokens => "responses_input_tokens",
            Self::ImagesGenerations => "images_generations",
            Self::ImagesEdits => "images_edits",
        }
    }
}

/// 按账号协议构造请求头（见 [`build_headers`]），供不带端点语义的调用
/// （模型列表拉取等）使用。
pub fn headers_for_protocol(protocol: Protocol, api_key: &str) -> Result<HeaderMap, UpstreamError> {
    let endpoint = match protocol {
        Protocol::OpenAiChat | Protocol::OpenAiResponses => Endpoint::ChatCompletions,
        Protocol::AnthropicMessages => Endpoint::Messages,
    };
    build_headers(endpoint, api_key, &HeaderMap::new())
}

/// 模型列表接口的 URL（§16.1）。三个协议都使用 `GET /v1/models`。
pub fn models_url(base_url: &str) -> Result<Url, UpstreamError> {
    let endpoint = Endpoint::ChatCompletions; // 借用它的版本段处理逻辑
    let mut url = build_url(base_url, endpoint)?;
    let path = url.path().trim_end_matches("/chat/completions").to_string();
    url.set_path(&format!("{path}/models"));
    Ok(url)
}

/// 模型列表响应体大小上限（§16.1 第 2 步：限制响应体大小）。
const MODELS_MAX_BYTES: usize = 8 * 1024 * 1024;

/// 拉取并解析上游模型列表（§16.1 第 1–3 步）。
///
/// 校验 HTTP 状态与 JSON 结构，去空、去重后保留真实名称的大小写。顺序保持
/// 上游返回次序去重前的相对位置，供前端稳定展示。
pub async fn fetch_model_list(
    http: &reqwest::Client,
    base_url: &str,
    protocol: Protocol,
    api_key: &str,
) -> std::result::Result<Vec<String>, UpstreamError> {
    let url = models_url(base_url)?;
    let response = http
        .get(url)
        .headers(headers_for_protocol(protocol, api_key)?)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| UpstreamError::BadStatus(0))?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(UpstreamError::BadStatus(status));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| UpstreamError::TooLarge)?
    {
        if bytes.len().saturating_add(chunk.len()) > MODELS_MAX_BYTES {
            return Err(UpstreamError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_model_list(&bytes)
}

/// 从响应体提取模型 ID 列表：OpenAI `{data:[{id}]}` 与 Anthropic
/// `{data:[{id}]}` 同形，直接按同一规则解析。
fn parse_model_list(bytes: &[u8]) -> std::result::Result<Vec<String>, UpstreamError> {
    let body: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| UpstreamError::BadResponse(e.to_string()))?;
    let data = body
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| UpstreamError::BadResponse("缺少 data 数组".into()))?;
    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::with_capacity(data.len());
    for entry in data {
        let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let trimmed = id.trim();
        // 去空并拒绝含空白或过长的 ID：它们既无法对应到真实模型，也可能
        // 只是上游把别的东西塞进了列表。
        if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) || trimmed.len() > 256 {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            names.push(trimmed.to_string());
        }
    }
    Ok(names)
}

/// 把账号 Base URL 与端点路径拼成最终 URL。
///
/// 管理员填写的 Base URL 既可能是 `https://host`，也可能是 `https://host/v1`
/// 或 `https://host/api/v1`。以 `/v1` 结尾时视为已经包含版本段，避免拼出
/// `https://host/v1/v1/messages` 这种必然 404 的地址。
pub fn build_url(base_url: &str, endpoint: Endpoint) -> Result<Url, UpstreamError> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let mut url = Url::parse(trimmed).map_err(|e| UpstreamError::InvalidBaseUrl(e.to_string()))?;

    let base_path = url.path().trim_end_matches('/').to_string();
    let mut suffix = endpoint.path();
    if base_path.ends_with("/v1") || base_path == "/v1" {
        suffix = suffix.strip_prefix("v1/").unwrap_or(suffix);
    }
    url.set_path(&format!("{base_path}/{suffix}"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// 构造发往上游的请求头。
///
/// 下游的鉴权头绝不转发；上游鉴权完全由账号配置生成（§14.7）。
pub fn build_headers(
    endpoint: Endpoint,
    api_key: &str,
    downstream: &HeaderMap,
) -> Result<HeaderMap, UpstreamError> {
    build_headers_with_content_type(
        endpoint,
        api_key,
        downstream,
        HeaderValue::from_static("application/json"),
    )
}

/// 构造发往上游的协议鉴权头，并使用调用方指定的正文类型。
///
/// multipart 请求必须把客户端的 boundary 一并保留下来；因此原始正文路径
/// 使用这个变体，而普通 JSON 请求继续通过 [`build_headers`] 走固定类型。
pub fn build_headers_with_content_type(
    endpoint: Endpoint,
    api_key: &str,
    downstream: &HeaderMap,
    content_type: HeaderValue,
) -> Result<HeaderMap, UpstreamError> {
    let mut headers = HeaderMap::new();
    headers.insert(axum::http::header::CONTENT_TYPE, content_type);

    match endpoint.protocol() {
        Protocol::AnthropicMessages => {
            headers.insert(
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(api_key).map_err(|_| UpstreamError::InvalidCredential)?,
            );
            let version = downstream
                .get("anthropic-version")
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static(DEFAULT_ANTHROPIC_VERSION));
            headers.insert(HeaderName::from_static("anthropic-version"), version);
            copy_allowed(downstream, &mut headers, ANTHROPIC_PASSTHROUGH_HEADERS);
        }
        Protocol::OpenAiChat | Protocol::OpenAiResponses => {
            let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|_| UpstreamError::InvalidCredential)?;
            headers.insert(axum::http::header::AUTHORIZATION, value);
            copy_allowed(downstream, &mut headers, OPENAI_PASSTHROUGH_HEADERS);
        }
    }

    Ok(headers)
}

/// 按白名单从下游请求头复制供应商协议头。
fn copy_allowed(from: &HeaderMap, to: &mut HeaderMap, allowed: &[&'static str]) {
    for name in allowed {
        if let Some(value) = from.get(*name)
            && let Ok(header) = HeaderName::try_from(*name)
        {
            to.insert(header, value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_urls_from_a_bare_host() {
        let url = build_url("https://api.anthropic.com", Endpoint::Messages).unwrap();
        assert_eq!(url.as_str(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn does_not_duplicate_an_existing_version_segment() {
        for base in ["https://host/v1", "https://host/v1/", "https://host/api/v1"] {
            let url = build_url(base, Endpoint::ChatCompletions).unwrap();
            assert!(
                !url.path().contains("/v1/v1/"),
                "{base} 拼出了重复的版本段：{url}"
            );
            assert!(url.path().ends_with("/chat/completions"), "{url}");
        }
    }

    #[test]
    fn preserves_a_subpath_prefix() {
        let url = build_url("https://host/proxy", Endpoint::Messages).unwrap();
        assert_eq!(url.as_str(), "https://host/proxy/v1/messages");
    }

    #[test]
    fn drops_query_and_fragment_from_the_base_url() {
        let url = build_url("https://host/?token=leak#frag", Endpoint::Responses).unwrap();
        assert_eq!(url.as_str(), "https://host/v1/responses");
    }

    #[test]
    fn count_tokens_shares_the_messages_protocol() {
        assert_eq!(
            Endpoint::CountTokens.protocol(),
            Protocol::AnthropicMessages
        );
        let url = build_url("https://host", Endpoint::CountTokens).unwrap();
        assert_eq!(url.path(), "/v1/messages/count_tokens");
    }

    #[test]
    fn downstream_credentials_are_never_forwarded() {
        let mut downstream = HeaderMap::new();
        downstream.insert(
            "authorization",
            HeaderValue::from_static("Bearer downstream-key"),
        );
        downstream.insert("x-api-key", HeaderValue::from_static("downstream-key"));
        downstream.insert("cookie", HeaderValue::from_static("session=abc"));

        let headers = build_headers(Endpoint::Messages, "upstream-real-key", &downstream).unwrap();
        assert_eq!(headers.get("x-api-key").unwrap(), "upstream-real-key");
        assert!(headers.get("authorization").is_none());
        assert!(headers.get("cookie").is_none());
    }

    #[test]
    fn anthropic_version_defaults_but_honours_the_client() {
        let headers = build_headers(Endpoint::Messages, "k", &HeaderMap::new()).unwrap();
        assert_eq!(
            headers.get("anthropic-version").unwrap(),
            DEFAULT_ANTHROPIC_VERSION
        );

        let mut downstream = HeaderMap::new();
        downstream.insert("anthropic-version", HeaderValue::from_static("2024-10-22"));
        let headers = build_headers(Endpoint::Messages, "k", &downstream).unwrap();
        assert_eq!(headers.get("anthropic-version").unwrap(), "2024-10-22");
    }

    #[test]
    fn vendor_beta_headers_only_cross_to_the_matching_vendor() {
        let mut downstream = HeaderMap::new();
        downstream.insert("anthropic-beta", HeaderValue::from_static("tools-2024"));
        downstream.insert("openai-beta", HeaderValue::from_static("assistants=v2"));

        let anthropic = build_headers(Endpoint::Messages, "k", &downstream).unwrap();
        assert_eq!(anthropic.get("anthropic-beta").unwrap(), "tools-2024");
        assert!(anthropic.get("openai-beta").is_none());

        let openai = build_headers(Endpoint::ChatCompletions, "k", &downstream).unwrap();
        assert_eq!(openai.get("openai-beta").unwrap(), "assistants=v2");
        assert!(openai.get("anthropic-beta").is_none());
    }

    #[test]
    fn openai_uses_bearer_authorization() {
        let headers =
            build_headers(Endpoint::ChatCompletions, "sk-abc", &HeaderMap::new()).unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-abc");
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn raw_content_type_is_kept_for_multipart_requests() {
        let content_type = HeaderValue::from_static("multipart/form-data; boundary=abc");
        let headers = build_headers_with_content_type(
            Endpoint::ImagesEdits,
            "sk-abc",
            &HeaderMap::new(),
            content_type.clone(),
        )
        .unwrap();
        assert_eq!(headers.get("content-type"), Some(&content_type));
        assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-abc");
    }

    #[test]
    fn model_list_parses_data_ids_and_dedupes() {
        let body = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "gpt-4o"},
                {"id": " gpt-4o "},
                {"id": "  "},
                {"id": "a b"},
                {"object": "model"},
                {"id": "GPT-4o"},
            ],
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let names = parse_model_list(&bytes).unwrap();
        assert_eq!(names, vec!["gpt-4o".to_string(), "GPT-4o".to_string()]);
    }

    #[test]
    fn model_list_rejects_structurally_invalid_bodies() {
        for body in [
            serde_json::json!([]),
            serde_json::json!({"models": []}),
            serde_json::json!("not json"),
        ] {
            let bytes = serde_json::to_vec(&body).unwrap();
            assert!(
                matches!(parse_model_list(&bytes), Err(UpstreamError::BadResponse(_))),
                "结构异常必须报错：{body}"
            );
        }
    }

    #[test]
    fn models_url_keeps_the_version_segment_once() {
        let url = models_url("https://host/v1").unwrap();
        assert_eq!(url.as_str(), "https://host/v1/models");
        let url = models_url("https://host").unwrap();
        assert_eq!(url.as_str(), "https://host/v1/models");
    }

    use reqwest::dns::Resolve as _;

    /// 解析器必须在解析阶段就把内网地址拦下：连接用的地址只能是校验过的
    /// 那一批，不能等 assert_resolvable 之后再解析第二次（§23.3、§26.8）。
    #[tokio::test]
    async fn the_guarded_resolver_blocks_private_addresses_unless_allowed() {
        let name: reqwest::dns::Name = "localhost".parse().unwrap();

        let denied = GuardedResolver {
            allow_private: false,
        };
        let error = denied
            .resolve(name)
            .await
            .err()
            .expect("默认必须拒绝解析到环回地址的主机");
        assert!(error.to_string().contains("环回"), "{error}");

        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        let allowed = GuardedResolver {
            allow_private: true,
        };
        assert!(
            allowed.resolve(name).await.is_ok(),
            "显式开启内网访问的账号必须能解析"
        );
    }
}
