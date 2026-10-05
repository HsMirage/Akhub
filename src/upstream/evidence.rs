//! 端点成功记录（§14.3）：只参与排序，不排除任何模型的请求。
//!
//! 404/405 可能来自模型、凭据或具体路径，不能证明整个账号没有这个接口。
//! 拒绝的端点只在本次请求内排除；这里只记录真实成功，24 小时后回到未知。

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::upstream::Endpoint;

pub const TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
pub struct Evidence {
    supported: RwLock<HashMap<(String, Endpoint), Instant>>,
}

impl Evidence {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只记录真正成功的调用，不能将收到 HTTP 响应头当成成功。
    pub fn note_supported(&self, account_id: &str, endpoint: Endpoint, now: Instant) {
        if let Ok(mut map) = self.supported.write() {
            map.insert((account_id.to_string(), endpoint), now + TTL);
        }
    }

    pub fn is_supported(&self, account_id: &str, endpoint: Endpoint, now: Instant) -> bool {
        self.supported
            .read()
            .ok()
            .and_then(|map| map.get(&(account_id.to_string(), endpoint)).copied())
            .is_some_and(|expires| expires > now)
    }

    /// 配置变更后旧的成功记录不再参与排序。
    pub fn clear(&self) {
        if let Ok(mut map) = self.supported.write() {
            map.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_scoped_to_one_account_and_endpoint() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_supported("a", Endpoint::Responses, now);
        assert!(evidence.is_supported("a", Endpoint::Responses, now));
        assert!(!evidence.is_supported("a", Endpoint::Messages, now));
        assert!(!evidence.is_supported("b", Endpoint::Responses, now));
    }

    #[test]
    fn success_expires_back_to_unknown() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_supported("a", Endpoint::Responses, now);
        assert!(evidence.is_supported("a", Endpoint::Responses, now + TTL / 2));
        assert!(!evidence.is_supported("a", Endpoint::Responses, now + TTL));
    }

    #[test]
    fn configuration_changes_clear_success_records() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_supported("a", Endpoint::Responses, now);
        evidence.clear();
        assert!(!evidence.is_supported("a", Endpoint::Responses, now));
    }
}
