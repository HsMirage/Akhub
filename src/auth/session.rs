//! 管理员会话：Argon2id 密码校验、内存会话表与登录限速（§23.2）。
//!
//! 限速按**客户端地址**而不是用户名：按用户名计数时，匿名者每分钟输错 5 次
//! 就能把真正的管理员永远锁在门外。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// 会话有效期。
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// 触发锁定所需的连续失败次数。
const MAX_FAILURES: u32 = 5;
/// 达到失败上限后的锁定时长。
const LOCKOUT: Duration = Duration::from_secs(60);
/// 失败计数表最多跟踪多少个客户端地址。
///
/// 地址来自未认证的连接，不设上限就是一个可以被匿名撑大的内存表。
const MAX_TRACKED_CLIENTS: usize = 4096;

/// 用 Argon2id 哈希管理员密码。
pub fn hash_password(password: &str) -> Result<String> {
    // 盐由 password-hash 自动生成，长度取该库的推荐值。
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| anyhow!("密码哈希失败：{e}"))
}

/// 在阻塞线程池里哈希密码：Argon2id 一次要几十毫秒满载 CPU，不能占着
/// tokio 的工作线程（§19.4）。
pub async fn hash_password_blocking(password: &str) -> Result<String> {
    let password = zeroize::Zeroizing::new(password.to_string());
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| anyhow!("密码哈希任务异常：{e}"))?
}

/// 在阻塞线程池里校验密码。
///
/// 用户不存在时（`encoded` 为 `None`）也照样跑一次 Argon2，对着一个固定的
/// 假哈希：否则"用户名不存在"会比"密码错误"快几十毫秒，用户名可被计时枚举。
pub async fn verify_password_blocking(password: &str, encoded: Option<String>) -> bool {
    let password = zeroize::Zeroizing::new(password.to_string());
    tokio::task::spawn_blocking(move || match encoded {
        Some(encoded) => verify_password(&password, &encoded),
        None => {
            let _ = verify_password(&password, dummy_hash());
            false
        }
    })
    .await
    .unwrap_or(false)
}

/// 用户不存在时用来"陪跑"的假哈希，参数与真实哈希一致。
fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hash_password("akhub-dummy-password").unwrap_or_default())
}

/// 校验密码。哈希本身损坏时按验证失败处理，不 panic。
pub fn verify_password(password: &str, encoded: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(encoded) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

struct Session {
    username: String,
    expires_at: Instant,
}

#[derive(Default)]
struct FailureState {
    count: u32,
    locked_until: Option<Instant>,
}

/// 内存中的管理员会话表。单实例部署下无需持久化：重启后重新登录即可。
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
    failures: Mutex<HashMap<String, FailureState>>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// 该客户端当前是否处于登录锁定中。
    pub fn is_locked(&self, client: &str) -> bool {
        let mut failures = crate::sync::lock(&self.failures);
        match failures.get(client).and_then(|f| f.locked_until) {
            Some(until) if until > Instant::now() => true,
            Some(_) => {
                // 锁定已过期，顺手清空计数，给一次干净的重试机会。
                failures.remove(client);
                false
            }
            None => false,
        }
    }

    /// 记录该客户端的一次登录失败，必要时锁定**这个客户端**。
    ///
    /// 表有上限：满了先清掉**已经解锁**的条目；还不够就整体清空——丢掉的只是
    /// 失败计数（最坏是多给几次重试）。
    pub fn record_failure(&self, client: &str) {
        let mut failures = crate::sync::lock(&self.failures);
        if failures.len() >= MAX_TRACKED_CLIENTS && !failures.contains_key(client) {
            let now = Instant::now();
            failures.retain(|_, entry| entry.locked_until.is_some_and(|until| until > now));
            if failures.len() >= MAX_TRACKED_CLIENTS {
                failures.clear();
            }
        }
        let entry = failures.entry(client.to_string()).or_default();
        entry.count += 1;
        if entry.count >= MAX_FAILURES {
            entry.locked_until = Some(Instant::now() + LOCKOUT);
            entry.count = 0;
        }
    }

    /// 该客户端登录成功：清空它的失败计数。
    pub fn clear_failures(&self, client: &str) {
        crate::sync::lock(&self.failures).remove(client);
    }

    /// 清掉过期会话与已解锁的失败计数。由后台清理任务周期调用：
    /// 只靠"再次使用时顺带清除"的话，没人再用的过期会话会一直留在内存里。
    pub fn purge_expired(&self) {
        let now = Instant::now();
        crate::sync::lock(&self.sessions).retain(|_, session| session.expires_at > now);
        crate::sync::lock(&self.failures)
            .retain(|_, entry| entry.count > 0 || entry.locked_until.is_some_and(|t| t > now));
    }

    /// 签发会话令牌。
    pub fn create(&self, username: &str) -> Result<String> {
        let token = URL_SAFE_NO_PAD.encode(&*crate::security::random_bytes(32)?);
        crate::sync::lock(&self.sessions).insert(
            token.clone(),
            Session {
                username: username.to_string(),
                expires_at: Instant::now() + SESSION_TTL,
            },
        );
        Ok(token)
    }

    /// 校验令牌并返回用户名；过期会话在此顺带清除。
    pub fn resolve(&self, token: &str) -> Option<String> {
        let mut sessions = crate::sync::lock(&self.sessions);
        match sessions.get(token) {
            Some(session) if session.expires_at > Instant::now() => Some(session.username.clone()),
            Some(_) => {
                sessions.remove(token);
                None
            }
            None => None,
        }
    }

    pub fn revoke(&self, token: &str) {
        crate::sync::lock(&self.sessions).remove(token);
    }

    /// 吊销全部会话。改密码之后必须调用：旧浏览器不能继续带着已失效的凭据。
    pub fn revoke_all(&self) {
        crate::sync::lock(&self.sessions).clear();
    }

    /// 当前会话数量，供测试断言。
    pub fn len(&self) -> usize {
        crate::sync::lock(&self.sessions).len()
    }

    /// 没有任何会话时返回 true。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrips_and_rejects_wrong_input() {
        let hash = hash_password("正确的密码").unwrap();
        assert!(verify_password("正确的密码", &hash));
        assert!(!verify_password("错误的密码", &hash));
    }

    #[test]
    fn identical_passwords_get_distinct_hashes() {
        assert_ne!(
            hash_password("same").unwrap(),
            hash_password("same").unwrap()
        );
    }

    /// 失败计数表不能被子虚乌有的用户名撑大。
    ///
    /// `username` 来自未认证的登录请求体：不设上限时，一个循环就能让这张表
    /// 无限增长。
    #[test]
    fn garbage_clients_cannot_grow_the_failure_table_without_bound() {
        let store = SessionStore::new();
        for index in 0..(MAX_TRACKED_CLIENTS * 2) {
            store.record_failure(&format!("ghost-{index}"));
        }
        let tracked = crate::sync::lock(&store.failures).len();
        assert!(
            tracked <= MAX_TRACKED_CLIENTS,
            "失败计数表必须收敛到上限内，实际 {tracked}"
        );
    }

    #[test]
    fn a_corrupt_hash_fails_verification_instead_of_panicking() {
        assert!(!verify_password("任意", "这不是一个 PHC 字符串"));
    }

    #[test]
    fn sessions_resolve_until_revoked() {
        let store = SessionStore::new();
        let token = store.create("admin").unwrap();
        assert_eq!(store.resolve(&token).as_deref(), Some("admin"));
        store.revoke(&token);
        assert!(store.resolve(&token).is_none());
    }

    #[test]
    fn unknown_tokens_never_resolve() {
        let store = SessionStore::new();
        assert!(store.resolve("伪造的令牌").is_none());
    }

    #[test]
    fn repeated_failures_lock_only_that_client() {
        let store = SessionStore::new();
        for _ in 0..MAX_FAILURES - 1 {
            store.record_failure("203.0.113.9");
        }
        assert!(!store.is_locked("203.0.113.9"));
        store.record_failure("203.0.113.9");
        assert!(store.is_locked("203.0.113.9"));
        assert!(!store.is_locked("198.51.100.1"), "别的客户端不受牵连");
    }

    #[test]
    fn a_successful_login_clears_the_failure_counter() {
        let store = SessionStore::new();
        for _ in 0..MAX_FAILURES - 1 {
            store.record_failure("203.0.113.9");
        }
        store.clear_failures("203.0.113.9");
        store.record_failure("203.0.113.9");
        assert!(!store.is_locked("203.0.113.9"), "成功登录后计数应当归零");
    }

    #[test]
    fn expired_sessions_are_purged_without_being_reused() {
        let store = SessionStore::new();
        let token = store.create("admin").unwrap();
        crate::sync::lock(&store.sessions)
            .get_mut(&token)
            .unwrap()
            .expires_at = Instant::now() - Duration::from_secs(1);
        store.create("admin").unwrap();
        store.purge_expired();
        assert_eq!(store.len(), 1, "过期会话必须被清理");
    }

    #[tokio::test]
    async fn unknown_users_still_pay_for_an_argon2_verification() {
        let hash = hash_password("正确的密码").unwrap();
        assert!(verify_password_blocking("正确的密码", Some(hash.clone())).await);
        assert!(!verify_password_blocking("错误", Some(hash)).await);
        assert!(!verify_password_blocking("正确的密码", None).await);
    }
}
