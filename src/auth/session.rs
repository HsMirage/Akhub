//! 管理员会话：Argon2id 密码校验、内存会话表与登录限速（§23.2）。

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

/// 用 Argon2id 哈希管理员密码。
pub fn hash_password(password: &str) -> Result<String> {
    // 盐由 password-hash 自动生成，长度取该库的推荐值。
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| anyhow!("密码哈希失败：{e}"))
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

    /// 该用户当前是否处于登录锁定中。
    pub fn is_locked(&self, username: &str) -> bool {
        let mut failures = crate::sync::lock(&self.failures);
        match failures.get(username).and_then(|f| f.locked_until) {
            Some(until) if until > Instant::now() => true,
            Some(_) => {
                // 锁定已过期，顺手清空计数，给一次干净的重试机会。
                failures.remove(username);
                false
            }
            None => false,
        }
    }

    /// 记录一次登录失败，必要时触发锁定。
    pub fn record_failure(&self, username: &str) {
        let mut failures = crate::sync::lock(&self.failures);
        let entry = failures.entry(username.to_string()).or_default();
        entry.count += 1;
        if entry.count >= MAX_FAILURES {
            entry.locked_until = Some(Instant::now() + LOCKOUT);
            entry.count = 0;
        }
    }

    /// 登录成功：清空失败计数并签发会话令牌。
    pub fn create(&self, username: &str) -> Result<String> {
        crate::sync::lock(&self.failures).remove(username);
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
    fn repeated_failures_lock_the_account() {
        let store = SessionStore::new();
        for _ in 0..MAX_FAILURES - 1 {
            store.record_failure("admin");
        }
        assert!(!store.is_locked("admin"));
        store.record_failure("admin");
        assert!(store.is_locked("admin"));
    }

    #[test]
    fn a_successful_login_clears_the_failure_counter() {
        let store = SessionStore::new();
        for _ in 0..MAX_FAILURES - 1 {
            store.record_failure("admin");
        }
        store.create("admin").unwrap();
        store.record_failure("admin");
        assert!(!store.is_locked("admin"), "成功登录后计数应当归零");
    }
}
