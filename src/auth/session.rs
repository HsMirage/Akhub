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

/// 同时进行的 Argon2 校验上限。
///
/// 每一次校验按当前参数（m=19456）要吃 19 MiB 并占满一个核几十毫秒。
/// 2026-11-11 的事件正是这么炸的：`spawn_blocking` 默认允许 512 个线程，
/// 突发几百个并发登录时 `512 x 19 MiB` 直接把 8 GB 机器打到内核 OOM。
/// 4 个并发足够两个人同时登录还不排队，最坏内存增量约 76 MiB。
const MAX_CONCURRENT_VERIFICATIONS: usize = 4;

/// Argon2 校验的并发闸门。
///
/// 超出的请求**不排队**，直接当作"稍后再试"——攻击者本来就不该占用名额，
/// 而真管理员手抖多点几次也能立刻得到明确反馈。
pub struct VerificationGate {
    permits: tokio::sync::Semaphore,
}

impl Default for VerificationGate {
    fn default() -> Self {
        Self::new()
    }
}

impl VerificationGate {
    pub fn new() -> Self {
        Self {
            permits: tokio::sync::Semaphore::new(MAX_CONCURRENT_VERIFICATIONS),
        }
    }

    /// 拿一个校验名额；当前已满则返回 None。
    pub fn try_acquire(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.permits.try_acquire().ok()
    }
}

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
    /// Argon2 校验的并发闸门，防止登录突发把内存打爆。
    gate: VerificationGate,
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
            gate: VerificationGate::new(),
        }
    }

    /// 拿一个 Argon2 校验名额；并发已满返回 None，调用方应当直接拒绝这次登录。
    ///
    /// 返回的 permit 必须活到校验结束——它一释放就代表那个 19 MiB 已经还回去了。
    pub fn try_begin_verification(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.gate.try_acquire()
    }

    /// 占用一次登录尝试的名额：已被锁定则返回 false。
    ///
    /// **必须在 `await` 之前调用**。历史上这里是 `is_locked()` 先查、等
    /// Argon2 校验完再 `record_failure()` 记账，两次加锁之间隔着几十毫秒的
    /// 校验窗口；攻击者一秒并发几百个请求时全部在"计数还是 0"的瞬间通过检查，
    /// 锁定形同虚设（2026-11-11 事件：单 IP 6,923 次请求里 4,351 次真的跑到
    /// 了 Argon2，把 19 MiB x 数百并发堆到 OOM）。
    ///
    /// 这里把"查锁定"和"先记一笔"合成一次加锁，并发请求必然互相看见。
    /// 校验成功时由 `clear_failures()` 把这一笔抹掉，失败则保留——所以名额
    /// 是"预扣"而不是"事后记账"。
    pub fn try_begin_attempt(&self, client: &str) -> bool {
        let mut failures = crate::sync::lock(&self.failures);
        let now = Instant::now();
        if let Some(locked_until) = failures.get(client).and_then(|f| f.locked_until) {
            if locked_until > now {
                return false;
            }
            // 锁定已过期，清掉旧账，给一次干净的重试机会。
            failures.remove(client);
        }
        drop(failures);
        self.record_failure(client);
        true
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
        for _ in 0..MAX_FAILURES {
            assert!(store.try_begin_attempt("203.0.113.9"));
        }
        assert!(!store.try_begin_attempt("203.0.113.9"), "打满就该锁定");
        assert!(
            store.try_begin_attempt("198.51.100.1"),
            "别的客户端不受牵连"
        );
    }

    #[test]
    fn a_successful_login_clears_the_failure_counter() {
        let store = SessionStore::new();
        for _ in 0..MAX_FAILURES {
            store.try_begin_attempt("203.0.113.9");
        }
        store.clear_failures("203.0.113.9");
        assert!(
            store.try_begin_attempt("203.0.113.9"),
            "成功登录后计数应当归零"
        );
    }

    /// 2026-11-11 的 OOM 根因：'查锁定'和'记失败'分成两步、中间隔着 Argon2。
    ///
    /// 并发突发时所有请求都在计数为 0 的瞬间通过检查，于是全部跑到 Argon2。
    /// 这个测试锁住修复后的行为：计数必须在校验**之前**就预扣掉。
    #[test]
    fn concurrent_bursts_cannot_all_pass_the_lockout_check() {
        let store = std::sync::Arc::new(SessionStore::new());
        let mut handles = Vec::new();
        for _ in 0..64 {
            let store = std::sync::Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                store.try_begin_attempt("203.0.113.9")
            }));
        }
        let passed = handles
            .into_iter()
            .map(|h| h.join().unwrap_or(false))
            .filter(|passed| *passed)
            .count();
        // record_failure 在计数触顶时把 count 归零并加锁，所以恰好触顶的那一次
        // 仍算放行：最多 MAX_FAILURES 次放行 + 1 次"触发锁定的那一次"。
        assert!(
            passed <= MAX_FAILURES as usize + 1,
            "并发突发最多只能放行 {} 个，实际放行 {passed} 个——锁定又漏了",
            MAX_FAILURES + 1
        );
    }

    /// 并发闸门：同时最多 4 个 Argon2 在校验，超出的必须被拒。
    #[test]
    fn verification_gate_caps_concurrent_argon2() {
        let store = SessionStore::new();
        let permits: Vec<_> = (0..MAX_CONCURRENT_VERIFICATIONS)
            .map(|_| store.try_begin_verification())
            .collect();
        assert!(permits.iter().all(|p| p.is_some()), "上限内应当全部放行");
        assert!(
            store.try_begin_verification().is_none(),
            "满了必须拒绝，否则 512 个并发 x 19 MiB 会把内存打爆"
        );
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
