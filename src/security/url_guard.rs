//! Base URL 校验与 SSRF 防护（§23.3）。
//!
//! 校验发生在两个时刻：保存账号时拒绝明显非法的地址；真正发出上游请求前
//! 再解析一次 DNS 并复查目标 IP，以对抗 DNS Rebinding。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use reqwest::Url;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UrlGuardError {
    #[error("Base URL 无法解析：{0}")]
    Malformed(String),
    #[error("Base URL 只允许 http 或 https，收到 {0}")]
    UnsupportedScheme(String),
    #[error("Base URL 缺少主机名")]
    MissingHost,
    #[error("Base URL 不能包含用户名或密码")]
    EmbeddedCredentials,
    #[error("目标地址 {0} 属于环回、内网或云元数据网段；如确需访问，请为该账号显式开启内网访问")]
    BlockedAddress(IpAddr),
    #[error("解析主机 {host} 失败：{source}")]
    ResolutionFailed {
        host: String,
        source: std::io::Error,
    },
}

/// 校验并标准化账号的 Base URL。
pub fn validate_base_url(raw: &str) -> Result<Url, UrlGuardError> {
    let url = Url::parse(raw.trim()).map_err(|e| UrlGuardError::Malformed(e.to_string()))?;

    match url.scheme() {
        "http" | "https" => {}
        other => return Err(UrlGuardError::UnsupportedScheme(other.to_string())),
    }
    if url.host_str().is_none() {
        return Err(UrlGuardError::MissingHost);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlGuardError::EmbeddedCredentials);
    }
    // 字面量 IP 在保存时即可判定，无需等到发请求。
    guard_literal(&url)?;
    Ok(url)
}

/// 主机是字面量 IP 且落在封禁网段时拒绝；域名一律放行（交给解析器复查）。
///
/// 字面量 IP 不经过 DNS 解析器，解析器层面的 SSRF 校验对它不起作用，所以共享
/// 上游客户端在建立连接前用它补上这一环（见 `upstream::LiteralGuard`）。
pub fn guard_literal(url: &Url) -> Result<(), UrlGuardError> {
    match host_as_ip(url) {
        Some(ip) if is_blocked(ip) => Err(UrlGuardError::BlockedAddress(ip)),
        _ => Ok(()),
    }
}

/// 把主机名解析成字面量 IP。IPv6 在 URL 中带方括号，必须先剥掉再解析，
/// 否则 `http://[::1]` 会被当成普通域名放行。
fn host_as_ip(url: &Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    unbracketed.parse::<IpAddr>().ok()
}

/// 发起上游请求前复查：解析主机并确认所有候选地址都可用。
///
/// 只要有一个地址落在被封禁网段就整体拒绝——DNS 轮询返回的任一地址
/// 都可能被实际连接使用。
pub async fn assert_resolvable(url: &Url, allow_private: bool) -> Result<(), UrlGuardError> {
    if allow_private {
        return Ok(());
    }
    let host = url.host_str().ok_or(UrlGuardError::MissingHost)?;
    if host_as_ip(url).is_some() {
        return guard_literal(url);
    }

    let port = url.port_or_known_default().unwrap_or(443);
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|source| UrlGuardError::ResolutionFailed {
            host: host.to_string(),
            source,
        })?;
    for addr in addrs {
        if is_blocked(addr.ip()) {
            return Err(UrlGuardError::BlockedAddress(addr.ip()));
        }
    }
    Ok(())
}

/// 判断 IP 是否属于必须默认阻止的网段。
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_documentation()
        // 100.64.0.0/10 运营商级 NAT
        || (a == 100 && (64..128).contains(&b))
        // 192.0.0.0/24 IETF 协议分配
        || (a == 192 && b == 0 && ip.octets()[2] == 0)
        // 198.18.0.0/15 基准测试
        || (a == 198 && (18..20).contains(&b))
        // 240.0.0.0/4 保留网段（含 255.255.255.255）
        || a >= 240
        || a == 0
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_v4(v4);
    }
    let segments = ip.segments();
    let first = segments[0];
    // 64:ff9b::/96 NAT64：末 32 位就是被翻译的 IPv4 地址，按它判定。
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        let [hi, lo] = [segments[6], segments[7]];
        return is_blocked_v4(Ipv4Addr::new(
            (hi >> 8) as u8,
            hi as u8,
            (lo >> 8) as u8,
            lo as u8,
        ));
    }
    ip.is_loopback()
        // 2002::/16 6to4：内嵌任意 IPv4，经中继可到达内网，整体阻止。
        || first == 0x2002
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7 唯一本地地址
        || (first & 0xfe00) == 0xfc00
        // fe80::/10 链路本地地址
        || (first & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_schemes() {
        assert!(matches!(
            validate_base_url("file:///etc/passwd"),
            Err(UrlGuardError::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn rejects_embedded_credentials() {
        assert!(matches!(
            validate_base_url("https://user:pass@api.example.com"),
            Err(UrlGuardError::EmbeddedCredentials)
        ));
    }

    #[test]
    fn rejects_loopback_and_metadata_literals() {
        for raw in [
            "http://127.0.0.1:8080",
            "http://169.254.169.254/latest",
            "http://[::1]:9000",
        ] {
            assert!(
                matches!(
                    validate_base_url(raw),
                    Err(UrlGuardError::BlockedAddress(_))
                ),
                "应当阻止 {raw}"
            );
        }
    }

    #[test]
    fn accepts_public_https_endpoint() {
        assert!(validate_base_url("https://api.anthropic.com").is_ok());
    }

    #[test]
    fn blocks_private_ranges_including_cgnat_and_mapped_v6() {
        assert!(is_blocked("10.0.0.5".parse().unwrap()));
        assert!(is_blocked("192.168.1.1".parse().unwrap()));
        assert!(is_blocked("100.64.0.1".parse().unwrap()));
        assert!(is_blocked("fd00::1".parse().unwrap()));
        assert!(is_blocked("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_blocked("1.1.1.1".parse().unwrap()));
        assert!(is_blocked("240.0.0.1".parse().unwrap()));
        assert!(is_blocked("255.255.255.255".parse().unwrap()));
        assert!(
            is_blocked("64:ff9b::7f00:1".parse().unwrap()),
            "NAT64 内嵌 127.0.0.1"
        );
        assert!(
            is_blocked("64:ff9b::a9fe:a9fe".parse().unwrap()),
            "NAT64 内嵌元数据地址"
        );
        assert!(
            !is_blocked("64:ff9b::101:101".parse().unwrap()),
            "NAT64 内嵌 1.1.1.1"
        );
        assert!(is_blocked("2002:7f00:1::1".parse().unwrap()), "6to4");
        assert!(!is_blocked("2606:4700::1111".parse().unwrap()));
    }
}
