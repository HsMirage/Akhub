//! 端点能力证据（§14.2、§16.7）。
//!
//! 只记录**明确的"不支持"**：某个账号的某个端点返回了"这条路由不存在"。普通
//! 400、5xx、超时和网络错误都不能证明端点不存在，绝不能写进这里——否则一次
//! 上游抖动就会永久关闭一条本来可用的原生通路。
//!
//! 证据 24 小时过期，配置变更时立即清空（账号的协议设置可能已经改了）。不做
//! 持久化：重启后第一个请求用一次 404 重新学会，代价是一次廉价失败，而把它
//! 写进数据库要多一张表和一条恢复路径。

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::upstream::Endpoint;

/// 限制默认 24 小时过期（§16.7）。
pub const TTL: Duration = Duration::from_secs(24 * 3600);

/// 账号 → 已证实不存在的端点及其过期时刻。
#[derive(Default)]
pub struct Evidence {
    unsupported: RwLock<HashMap<(String, Endpoint), Instant>>,
}

impl Evidence {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次"这个端点不存在"。
    pub fn note_unsupported(&self, account_id: &str, endpoint: Endpoint, now: Instant) {
        if let Ok(mut map) = self.unsupported.write() {
            map.insert((account_id.to_string(), endpoint), now + TTL);
        }
    }

    /// 该端点此刻是否已被证实不存在。
    pub fn is_unsupported(&self, account_id: &str, endpoint: Endpoint, now: Instant) -> bool {
        self.unsupported
            .read()
            .ok()
            .and_then(|map| map.get(&(account_id.to_string(), endpoint)).copied())
            .is_some_and(|expires| expires > now)
    }

    /// 清空全部证据。配置一旦变化就调用：账号的协议设置可能刚被改过，旧证据
    /// 的前提已经不成立（§16.7）。
    pub fn clear(&self) {
        if let Ok(mut map) = self.unsupported.write() {
            map.clear();
        }
    }

    /// 当前有效的证据条数，供后台展示。
    pub fn len(&self, now: Instant) -> usize {
        self.unsupported
            .read()
            .map(|map| map.values().filter(|expires| **expires > now).count())
            .unwrap_or(0)
    }

    pub fn is_empty(&self, now: Instant) -> bool {
        self.len(now) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_is_scoped_to_one_account_and_one_endpoint() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_unsupported("acc1", Endpoint::Messages, now);

        assert!(evidence.is_unsupported("acc1", Endpoint::Messages, now));
        assert!(!evidence.is_unsupported("acc1", Endpoint::ChatCompletions, now));
        assert!(!evidence.is_unsupported("acc2", Endpoint::Messages, now));
    }

    #[test]
    fn evidence_expires_after_a_day() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_unsupported("acc", Endpoint::Responses, now);

        assert!(evidence.is_unsupported("acc", Endpoint::Responses, now + TTL / 2));
        assert!(!evidence.is_unsupported(
            "acc",
            Endpoint::Responses,
            now + TTL + Duration::from_secs(1)
        ));
        assert_eq!(evidence.len(now + TTL), 0);
    }

    #[test]
    fn a_configuration_change_invalidates_everything() {
        let evidence = Evidence::new();
        let now = Instant::now();
        evidence.note_unsupported("acc", Endpoint::Responses, now);
        evidence.clear();
        assert!(evidence.is_empty(now));
    }
}
