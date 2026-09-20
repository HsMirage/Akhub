//! 端点能力证据（§14.2、§14.3、§16.7）。
//!
//! 记两类证据，都是**明确**的才记：
//!
//! - **不支持**：某个账号的某个端点返回了"这条路由不存在"。普通 400、5xx、
//!   超时和网络错误都不能证明端点不存在，绝不能写进这里——否则一次上游抖动
//!   就会永久关闭一条本来可用的原生通路。
//! - **已确认支持**：某个账号的某个端点真的成功服务过一次请求。这是 §14.3
//!   第 1 档与第 4 档里的"已确认支持"，也是它和"能力未知"的**唯一**区别。
//!
//! 证据 24 小时过期，配置变更时立即清空（账号的协议设置可能已经改了）。不做
//! 持久化：重启后第一个请求重新学会，代价是一次尝试，而把它写进数据库要多
//! 一张表和一条恢复路径。

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::upstream::Endpoint;

/// 限制默认 24 小时过期（§16.7）。
pub const TTL: Duration = Duration::from_secs(24 * 3600);

/// 账号 → 端点证据及其过期时刻。
#[derive(Default)]
pub struct Evidence {
    unsupported: RwLock<HashMap<(String, Endpoint), Instant>>,
    /// 已确认能用的端点（§14.3 的"已确认支持"）。
    ///
    /// 与"能力未知"分开是为了排序：两个都能表达这次请求的端点里，已经成功过
    /// 的那个应当先试。证据过期后自动退回"未知"。
    supported: RwLock<HashMap<(String, Endpoint), Instant>>,
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

    /// 记录一次"这个端点确实能用"：它成功服务过一次请求。
    ///
    /// 只该在拿到成功响应之后调用。审计要点是"**真的服务过**"，不是"我们试着
    /// 发了"——发出去但被拒绝的请求不构成支持证据。
    pub fn note_supported(&self, account_id: &str, endpoint: Endpoint, now: Instant) {
        if let Ok(mut map) = self.supported.write() {
            map.insert((account_id.to_string(), endpoint), now + TTL);
        }
    }

    /// 该端点此刻是否已被证实可用（§14.3 的"已确认支持"）。
    pub fn is_supported(&self, account_id: &str, endpoint: Endpoint, now: Instant) -> bool {
        self.supported
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
        if let Ok(mut map) = self.supported.write() {
            map.clear();
        }
    }

    /// 当前有效的证据条数（两类合计）。
    pub fn len(&self, now: Instant) -> usize {
        self.unsupported_len(now) + self.supported_len(now)
    }

    /// 「已证实不存在」的条数。
    ///
    /// 概览页的告警只该看这个数：supported 是"真的成功过一次"的记录，把它
    /// 一起算进来，一台健康运转的网关会一直挂着"上游没有这个端点"的提示——
    /// 那是把正常工作的证据当成了故障。
    pub fn unsupported_len(&self, now: Instant) -> usize {
        count_live(&self.unsupported, now)
    }

    /// 「已确认可用」的条数。
    pub fn supported_len(&self, now: Instant) -> usize {
        count_live(&self.supported, now)
    }

    pub fn is_empty(&self, now: Instant) -> bool {
        self.len(now) == 0
    }
}

/// 数一个表里还没过期的条目。
fn count_live(map: &RwLock<HashMap<(String, Endpoint), Instant>>, now: Instant) -> usize {
    map.read()
        .map(|map| map.values().filter(|expires| **expires > now).count())
        .unwrap_or(0)
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

    /// 概览页的"上游没有这个端点"只能数不支持的证据。
    ///
    /// 支持证据是成功过的记录；混在一起会让健康部署一直显示告警。
    #[test]
    fn only_unsupported_evidence_counts_as_a_missing_endpoint() {
        let now = Instant::now();
        let evidence = Evidence::new();
        evidence.note_supported("a", Endpoint::ChatCompletions, now);
        evidence.note_supported("b", Endpoint::Messages, now);
        assert_eq!(evidence.unsupported_len(now), 0, "成功不算缺失");
        assert_eq!(evidence.supported_len(now), 2);

        evidence.note_unsupported("c", Endpoint::Responses, now);
        assert_eq!(evidence.unsupported_len(now), 1);
        assert_eq!(evidence.len(now), 3, "合计仍然是两类之和");
    }

    /// 配置变化要同时清掉支持与不支持的证据（§16.7）。
    #[test]
    fn clearing_evidence_drops_both_kinds() {
        let now = Instant::now();
        let evidence = Evidence::new();
        evidence.note_supported("a", Endpoint::Messages, now);
        evidence.note_unsupported("a", Endpoint::Responses, now);
        assert_eq!(evidence.len(now), 2);
        evidence.clear();
        assert!(evidence.is_empty(now));
        assert!(!evidence.is_supported("a", Endpoint::Messages, now));
        assert!(!evidence.is_unsupported("a", Endpoint::Responses, now));
    }
}
