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
