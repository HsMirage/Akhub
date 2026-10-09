//! 日志与错误信息脱敏（§23.4、§24.1）。

/// 需要在日志中屏蔽的请求头名称（小写匹配）。
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "api-key",
    "cookie",
    "set-cookie",
    "proxy-authorization",
    "new-api-user",
    "x-akhub-session",
];

/// 判断某个请求头是否必须脱敏后才能进入日志。
pub fn is_sensitive_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_HEADERS.contains(&lower.as_str())
}

/// 把凭据压缩成"前缀 + 长度"形式，既可用于排查又不泄漏密钥。
///
/// **幂等**：对已经脱敏过的文本再跑一次不会变成"脱敏的脱敏"。全局日志层会对
/// 所有输出再过一遍，而调用点自己也常常先脱敏一次，两次都得安全。
pub fn secret(value: &str) -> String {
    if value.contains('…') {
        return value.to_string();
    }
    let visible: String = value.chars().take(6).collect();
    format!("{visible}…({} 字符)", value.chars().count())
}

/// 脱敏单个请求头值。
pub fn header_value(name: &str, value: &str) -> String {
    if is_sensitive_header(name) {
        secret(value)
    } else {
        value.to_string()
    }
}

/// 密钥前缀。上游把 Key 回显在错误正文里的情况并不少见，转发这类文本前
/// 必须先过一遍脱敏。
const KEY_PREFIXES: &[&str] = &["sk-", "sk_", "akh-"];
/// 没有前缀但足够长的连续 Token 字符也按密钥处理。
const OPAQUE_LEN: usize = 32;
/// 词两侧允许剥掉的标点。
const PUNCTUATION: &str = "\"'`,;()[]{}<>";

/// 只有经过当前账号上下文脱敏的原因，才允许覆盖通用客户提示。
#[derive(Debug, Clone)]
pub(crate) struct PublicMessage(String);

impl PublicMessage {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone)]
pub(crate) struct ErrorRedactor {
    private_values: Vec<zeroize::Zeroizing<String>>,
}

impl ErrorRedactor {
    pub(crate) fn new(account: &str, base_url: &str, key: &str) -> Self {
        let host = reqwest::Url::parse(base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_default();
        let mut values: Vec<_> = [account, base_url, &host, key]
            .into_iter()
            .filter(|value| !value.is_empty())
            .map(|value| zeroize::Zeroizing::new(value.to_owned()))
            .collect();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self {
            private_values: values,
        }
    }

    pub(crate) fn message(&self, raw: &str) -> PublicMessage {
        let mut result = raw.to_owned();
        for value in &self.private_values {
            let lower = result.to_ascii_lowercase();
            let needle = value.to_ascii_lowercase();
            let mut masked = String::new();
            let mut previous = 0;
            for (offset, _) in lower.match_indices(&needle) {
                // 短账号名 A / 10 不应误伤 auto 或 1024x1024。
                if value.is_ascii() && value.len() <= 2 {
                    let word = |byte: &u8| byte.is_ascii_alphanumeric() || *byte == b'_';
                    if offset
                        .checked_sub(1)
                        .and_then(|i| result.as_bytes().get(i))
                        .is_some_and(word)
                        || result
                            .as_bytes()
                            .get(offset + value.len())
                            .is_some_and(word)
                    {
                        continue;
                    }
                }
                masked.push_str(&result[previous..offset]);
                masked.push_str("[已隐藏]");
                previous = offset + value.len();
            }
            masked.push_str(&result[previous..]);
            result = masked;
        }
        // 完整 URL 可能带凭据、私有地址或查询令牌，不应进入客户消息。
        let lower = result.to_ascii_lowercase();
        let mut masked = String::new();
        let mut previous = 0;
        for (start, _) in lower.match_indices("http") {
            if start < previous
                || !(lower[start..].starts_with("https://")
                    || lower[start..].starts_with("http://"))
            {
                continue;
            }
            let end = result[start..]
                .find(|c: char| c.is_whitespace() || "\"'<>),;".contains(c))
                .map(|length| start + length)
                .unwrap_or(result.len());
            masked.push_str(&result[previous..start]);
            masked.push_str("[地址已隐藏]");
            previous = end;
        }
        masked.push_str(&result[previous..]);
        result = masked;
        let result: String = text(&result)
            .chars()
            .filter(|c| !c.is_control() || c.is_whitespace())
            .take(1200)
            .collect();
        PublicMessage(if result.trim().is_empty() {
            "服务未返回具体错误原因，请提供请求编号联系管理员".into()
        } else {
            result
        })
    }

    pub(crate) fn payload(&self, value: &serde_json::Value) -> PublicMessage {
        let message = [
            "/error/message",
            "/response/error/message",
            "/message",
            "/error",
            "/detail",
        ]
        .iter()
        .find_map(|path| {
            value
                .pointer(path)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or("");
        self.message(message)
    }
}

/// 脱敏一段自由文本：把看起来像密钥的片段替换成"前缀 + 长度"。
///
/// 宁可多脱一点也不能漏：误伤一个长哈希只是日志难看一点，漏掉一把 Key 是
/// 事故。
pub fn text(raw: &str) -> String {
    raw.split_inclusive(char::is_whitespace)
        .map(redact_word)
        .collect()
}

/// 处理一个以空白结尾的词。
///
/// 依次剥掉尾随空白、两侧标点、`key=` / `Authorization:` 这类前导，只对真正
/// 的值做判断；替换时把剥掉的部分原样拼回去，保证句子其余部分不受影响。
fn redact_word(chunk: &str) -> String {
    let word = chunk.trim_end();
    let tail = &chunk[word.len()..];

    let is_punctuation = |c: char| PUNCTUATION.contains(c);
    let start = word.len() - word.trim_start_matches(is_punctuation).len();
    let end = word.trim_end_matches(is_punctuation).len();
    if start >= end {
        return chunk.to_string();
    }
    let (head, rest) = word.split_at(start);
    let (core, punctuation_tail) = rest.split_at(end - start);
    let (lead, value) = match core.rfind(['=', ':']) {
        Some(index) => core.split_at(index + 1),
        None => ("", core),
    };

    if looks_like_key(value) {
        format!("{head}{lead}{}{punctuation_tail}{tail}", secret(value))
    } else {
        chunk.to_string()
    }
}

fn looks_like_key(value: &str) -> bool {
    KEY_PREFIXES
        .iter()
        .any(|prefix| value.len() > prefix.len() + 4 && value.starts_with(prefix))
        || (value.len() >= OPAQUE_LEN
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_error_keeps_parameters_but_removes_private_identity_and_credentials() {
        let redactor = ErrorRedactor::new(
            "私人渠道-xkiro",
            "https://upstream.example/api",
            "tiny-secret",
        );
        let message = "私人渠道-xkiro: Unsupported size \"auto\". Supported: 1024x1024, 1024x1536. upstream.example key=tiny-secret https://another.example/debug?token=foo";
        let public = redactor.message(message);
        for private in [
            "私人渠道-xkiro",
            "upstream.example",
            "tiny-secret",
            "another.example",
            "token=foo",
        ] {
            assert!(!public.as_str().contains(private), "{public:?}");
        }
        assert!(public.as_str().contains("Unsupported size \"auto\""));
        assert!(public.as_str().contains("1024x1024, 1024x1536"));
        assert_eq!(redactor.message(public.as_str()).as_str(), public.as_str());
    }

    #[test]
    fn structured_error_only_exposes_message_not_debug_fields() {
        let redactor =
            ErrorRedactor::new("Private-Channel", "https://upstream.example", "short-key");
        let public = redactor.payload(&serde_json::json!({"error":{"message":null},"message":"PRIVATE-CHANNEL: invalid size auto","debug":"never exposed"}));
        assert!(public.as_str().contains("invalid size auto"));
        assert!(!public.as_str().contains("PRIVATE-CHANNEL"));
        assert!(!public.as_str().contains("never exposed"));
    }

    #[test]
    fn short_account_names_do_not_destroy_parameter_values() {
        let redactor = ErrorRedactor::new("A", "", "");
        assert_eq!(
            redactor.message("A: size auto; 1024x1024").as_str(),
            "[已隐藏]: size auto; 1024x1024"
        );
        let redactor = ErrorRedactor::new("10", "", "");
        assert_eq!(
            redactor.message("10: size 1024x1024").as_str(),
            "[已隐藏]: size 1024x1024"
        );
    }

    /// 脱敏必须幂等：全局日志层会对所有输出再过一遍，而调用点自己也常常
    /// 先脱敏一次。不幂等的话日志里会出现"脱敏的脱敏"。
    #[test]
    fn redaction_is_idempotent() {
        let once = text("上游拒绝 sk-abcdef1234567890 这个 Key");
        let twice = text(&once);
        assert_eq!(once, twice, "二次脱敏不该改变结果");
        assert!(!twice.contains("abcdef1234567890"), "密钥不能残留：{twice}");
    }

    /// 已知的密钥形态都要被替换掉（§20.2、§23.4）。
    #[test]
    fn every_known_key_shape_is_replaced() {
        for raw in [
            "sk-proj-abcdefghijklmnopqrstuvwxyz012345",
            "akh-abcdefghijklmnopqrstuvwxyz012345",
            "Authorization: Bearer sk-abcdefghijklmnop",
            "api_key=sk_abcdefghijklmnopqrst",
        ] {
            let out = text(raw);
            assert!(out.contains('…'), "这个形态没被脱敏：{raw:?} → {out:?}");
        }
    }

    #[test]
    fn sensitive_headers_are_matched_case_insensitively() {
        assert!(is_sensitive_header("Authorization"));
        assert!(is_sensitive_header("X-Api-Key"));
        assert!(!is_sensitive_header("content-type"));
    }

    #[test]
    fn secret_never_reveals_full_value() {
        let masked = secret("sk-abcdef0123456789");
        assert!(!masked.contains("0123456789"));
        assert!(masked.starts_with("sk-abc"));
    }

    #[test]
    fn non_sensitive_header_passes_through() {
        assert_eq!(
            header_value("content-type", "application/json"),
            "application/json"
        );
        assert_ne!(
            header_value("authorization", "Bearer sk-xyz-secret"),
            "Bearer sk-xyz-secret"
        );
    }

    #[test]
    fn free_text_loses_anything_that_looks_like_a_key() {
        let masked = text("上游返回 invalid key sk-abcdef0123456789xyz, 请检查");
        assert!(!masked.contains("0123456789xyz"), "{masked}");
        assert!(masked.contains("上游返回"), "非密钥文字必须原样保留");
        assert!(masked.contains("请检查"));
        assert!(masked.contains("…(22 字符),"), "标点要拼回原位：{masked}");

        // 没有前缀但足够长的不透明串同样按密钥处理，`key=value` 形式也要看值。
        let opaque = text("token=aaaaaaaabbbbbbbbccccccccdddddddd 之后");
        assert!(!opaque.contains("dddddddd"), "{opaque}");
        assert!(opaque.starts_with("token="), "{opaque}");

        let header = text("Authorization:sk-live-0123456789 已拒绝");
        assert!(!header.contains("0123456789"), "{header}");
        assert!(header.starts_with("Authorization:sk-liv"), "{header}");
    }

    #[test]
    fn ordinary_words_and_numbers_survive_redaction() {
        let message = text("探针返回 HTTP 503，Content-Type 不是 JSON");
        assert_eq!(message, "探针返回 HTTP 503，Content-Type 不是 JSON");
        // 时间与 URL 里的冒号不能触发误判。
        assert_eq!(text("10:00:01 https://host/v1"), "10:00:01 https://host/v1");
    }
}
