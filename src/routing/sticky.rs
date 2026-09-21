//! 会话粘性：粘性键推导、绑定表与等待预算（§10）。
//!
//! 第 4 级粘性键**只哈希稳定前缀**，不含任何用户消息。这是与第一版最重要的
//! 区别：上游的 prompt cache 按前缀匹配，缓存亲和的正确单位是"前缀"而不是
//! "会话"。把第一条用户消息塞进哈希会让 `/compact` 必然打断粘性——恰好发生
//! 在上下文最大、缓存最值钱的那一刻。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::http::HeaderMap;
use hmac::{Hmac, KeyInit as _, Mac};
use sha2::Sha256;

use crate::security::KeyDigest;
use crate::storage::store::StickyBindingRow;

/// 普通粘性的滑动过期时间（§10.2）。
pub const TTL: Duration = Duration::from_secs(3600);

/// 显式会话头，按 §10.1 的第 2 级识别。
const SESSION_HEADERS: &[&str] = &[
    "x-session-affinity",
    "x-session-id",
    "session-id",
    "conversation_id",
    "x-conversation-id",
];

/// 一条粘性绑定。
///
/// 绑的是**目标 + Key**：上游的 prompt cache 按凭据隔离，只绑目标会在账号内
/// 换 Key 时把整份前缀缓存作废（§4.2.1 的不变量 B）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub target_id: String,
    /// 绑定的那把 Key 的凭据摘要。`None` 表示绑定时账号没有 Key 池语义
    /// （或来自升级前的快照），下一次成功调用即补齐。
    pub credential_digest: Option<String>,
    pub bound_at: i64,
    /// 上次真正使用该目标的时间，同时用于滑动过期与缓存新鲜度系数。
    pub last_used_at: i64,
}

/// 粘性键：分组 + 逻辑模型 + 推导出的标识摘要。
///
/// 分组和逻辑模型必须进键：不同分组的同一个会话本来就该落到不同账号上。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key(String);

impl Key {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 粘性键的来源，供后台诊断"为什么这两个请求没粘在一起"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Responses 的 `conversation` / `previous_response_id`。
    ResponseChain,
    /// 显式会话头。
    SessionHeader,
    /// `prompt_cache_key` 等协议内稳定字段。
    CacheKey,
    /// 稳定前缀摘要（system prompt + 工具定义）。
    StablePrefix,
}

/// 推导粘性键（§10.1）。按优先级依次尝试，全部落空时返回 `None`。
pub fn derive(
    digest: &KeyDigest,
    group_id: &str,
    logical_model: &str,
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Option<(Key, Origin)> {
    let scope = format!("{group_id}\u{1}{logical_model}");

    // 1. Responses 状态链：这是最强的信号，上游资源本身就绑在一个账号上。
    if let Some(value) = string_field(body, "conversation").or_else(|| {
        body.get("previous_response_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string)
    }) {
        return Some((
            key(&scope, "chain", value.as_bytes()),
            Origin::ResponseChain,
        ));
    }

    // 2. 显式会话头。
    for name in SESSION_HEADERS {
        if let Some(value) = headers.get(*name).and_then(|v| v.to_str().ok())
            && !value.trim().is_empty()
        {
            return Some((
                key(&scope, "session", value.trim().as_bytes()),
                Origin::SessionHeader,
            ));
        }
    }

    // 3. 协议中的稳定用户/会话字段。
    if let Some(value) = body
        .get("prompt_cache_key")
        .and_then(serde_json::Value::as_str)
        .or_else(|| body.get("user").and_then(serde_json::Value::as_str))
        .filter(|value| !value.trim().is_empty())
    {
        return Some((key(&scope, "cache", value.as_bytes()), Origin::CacheKey));
    }

    // 4. 稳定前缀摘要。
    let prefix = stable_prefix(body);
    if prefix.is_empty() {
        // 没有 system prompt 也没有工具定义时前缀是空的，粘上去毫无意义：
        // 一次性的短请求本来就不需要缓存亲和。
        return None;
    }
    Some((
        key_with(digest, &scope, "prefix", &prefix),
        Origin::StablePrefix,
    ))
}

/// 取出参与稳定前缀哈希的部分：system prompt 与工具定义，外加分组与模型。
///
/// 三个协议的字段名不同，但语义一致：
/// * OpenAI Chat —— `messages` 里 role 为 `system` / `developer` 的条目。
/// * Anthropic —— 顶层 `system`。
/// * Responses —— 顶层 `instructions`。
fn stable_prefix(body: &serde_json::Value) -> Vec<u8> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(system) = body.get("system") {
        parts.push(canonical(system));
    }
    if let Some(instructions) = body.get("instructions") {
        parts.push(canonical(instructions));
    }
    if let Some(messages) = body.get("messages").and_then(serde_json::Value::as_array) {
        for message in messages {
            match message.get("role").and_then(serde_json::Value::as_str) {
                Some("system") | Some("developer") => {
                    parts.push(canonical(message.get("content").unwrap_or(message)));
                }
                // 遇到第一条非系统消息就停：后面全是会变的用户内容。
                _ => break,
            }
        }
    }
    if let Some(tools) = body.get("tools") {
        parts.push(canonical(tools));
    }

    if parts.is_empty() {
        return Vec::new();
    }
    parts.join("\u{1}").into_bytes()
}

/// 稳定序列化：`serde_json::Value` 的对象是 `BTreeMap`，键序天然有序，
/// 所以同一份内容永远得到同一段字节。
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn string_field(body: &serde_json::Value, field: &str) -> Option<String> {
    match body.get(field)? {
        serde_json::Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        // Responses 的 `conversation` 也可能是 `{"id": "conv_..."}`。
        serde_json::Value::Object(map) => map
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// 明文标识可以直接进键：它们本来就是客户端给的不敏感 ID。
fn key(scope: &str, kind: &str, value: &[u8]) -> Key {
    let mut hasher =
        <Hmac<Sha256>>::new_from_slice(b"akhub:sticky:v1").expect("HMAC-SHA256 接受任意长度密钥");
    hasher.update(scope.as_bytes());
    hasher.update(&[1]);
    hasher.update(kind.as_bytes());
    hasher.update(&[1]);
    hasher.update(value);
    Key(hex::encode(hasher.finalize().into_bytes()))
}

/// 稳定前缀含 system prompt 与工具定义，必须用主密钥派生的 pepper 哈希，
/// 且只保存摘要——原文绝不落库、绝不进日志（§23.4）。
fn key_with(digest: &KeyDigest, scope: &str, kind: &str, value: &[u8]) -> Key {
    let mut material = Vec::with_capacity(scope.len() + kind.len() + value.len() + 2);
    material.extend_from_slice(scope.as_bytes());
    material.push(1);
    material.extend_from_slice(kind.as_bytes());
    material.push(1);
    material.extend_from_slice(value);
    Key(hex::encode(
        digest.digest(&String::from_utf8_lossy(&material)),
    ))
}

/// 粘性绑定表的内存上限（§19.4）。
///
/// 一个"项目"一行，正常规模是几十到几百。这个上限纯粹是失控保护：配置写错、
/// 上游返回的 system prompt 每次都不同、或者有人拿随机前缀打网关时，表不能
/// 无限涨下去。
const MAX_BINDINGS: usize = 10_000;
/// 触发上限时一次淘汰多少条。
///
/// 按批淘汰而不是只淘汰一条：否则表会长期贴着上限，每次绑定都做一次全表扫描。
const EVICT_BATCH: usize = MAX_BINDINGS / 10;

/// 内存中的粘性绑定表。写盘由 60 秒快照任务批量完成（§19.4）。
#[derive(Default)]
pub struct Bindings {
    inner: RwLock<HashMap<Key, Arc<BindingEntry>>>,
}

#[derive(Debug)]
struct BindingEntry {
    group_id: String,
    logical_model: String,
    state: std::sync::Mutex<Binding>,
}

impl Bindings {
    pub fn new() -> Self {
        Self::default()
    }

    /// 查询绑定。命中时顺带刷新滑动过期时间。
    pub fn get(&self, key: &Key, now: i64) -> Option<Binding> {
        let entry = {
            let guard = self.inner.read().ok()?;
            Arc::clone(guard.get(key)?)
        };
        let mut state = entry.state.lock().ok()?;
        if now - state.last_used_at >= TTL.as_secs() as i64 {
            return None;
        }
        state.last_used_at = now;
        Some(state.clone())
    }

    /// 建立或改写绑定。
    ///
    /// `credential_digest` 是这次真正使用的 Key 的摘要，`None` 表示这次
    /// 调用没有凭据语义。
    pub fn bind(
        &self,
        key: Key,
        group_id: &str,
        logical_model: &str,
        target_id: &str,
        credential_digest: Option<&str>,
        now: i64,
    ) {
        let mut guard = crate::sync::write(&self.inner);
        if guard.len() >= MAX_BINDINGS && !guard.contains_key(&key) {
            evict_oldest(&mut guard);
        }
        guard.insert(
            key,
            Arc::new(BindingEntry {
                group_id: group_id.to_string(),
                logical_model: logical_model.to_string(),
                state: std::sync::Mutex::new(Binding {
                    target_id: target_id.to_string(),
                    credential_digest: credential_digest.map(str::to_string),
                    bound_at: now,
                    last_used_at: now,
                }),
            }),
        );
    }

    /// 只把绑定里的 Key 换成另一把，保留绑定时间。
    ///
    /// 用于"原 Key 不再可用、同账号内换了另一把"的场景：目标没变，所以
    /// 不需要重新抽签，只需要把凭据亲和更新到新的那把（§4.2.1）。
    pub fn rebind_credential(&self, key: &Key, credential_digest: Option<&str>, now: i64) {
        let entry = match self.inner.read() {
            Ok(guard) => guard.get(key).map(Arc::clone),
            Err(_) => None,
        };
        let Some(entry) = entry else {
            return;
        };
        if let Ok(mut state) = entry.state.lock() {
            state.credential_digest = credential_digest.map(str::to_string);
            state.last_used_at = now;
        }
    }

    /// 目标不再合格时清除绑定（§10.2）。
    pub fn clear(&self, key: &Key) {
        if let Ok(mut guard) = self.inner.write() {
            guard.remove(key);
        }
    }

    /// 导出全部绑定，供 60 秒快照任务落盘。
    pub fn export(&self) -> Vec<StickyBindingRow> {
        let guard = crate::sync::read(&self.inner);
        guard
            .iter()
            .filter_map(|(key, entry)| {
                let state = entry.state.lock().ok()?;
                Some(StickyBindingRow {
                    sticky_key: key.0.clone(),
                    group_id: entry.group_id.clone(),
                    logical_model: entry.logical_model.clone(),
                    target_id: state.target_id.clone(),
                    credential_digest: state.credential_digest.clone(),
                    bound_at: state.bound_at,
                    last_used_at: state.last_used_at,
                })
            })
            .collect()
    }

    /// 启动时从快照恢复，让前缀缓存不因重启而丢失（§20.1）。
    pub fn restore(&self, rows: &[StickyBindingRow]) {
        let mut guard = crate::sync::write(&self.inner);
        for row in rows {
            guard.insert(
                Key(row.sticky_key.clone()),
                Arc::new(BindingEntry {
                    group_id: row.group_id.clone(),
                    logical_model: row.logical_model.clone(),
                    state: std::sync::Mutex::new(Binding {
                        target_id: row.target_id.clone(),
                        credential_digest: row.credential_digest.clone(),
                        bound_at: row.bound_at,
                        last_used_at: row.last_used_at,
                    }),
                }),
            );
        }
    }

    /// 清除过期或指向已消失目标的绑定。
    pub fn prune(&self, live_targets: &[String], now: i64) {
        if let Ok(mut guard) = self.inner.write() {
            guard.retain(|_, entry| {
                let Ok(state) = entry.state.lock() else {
                    return false;
                };
                now - state.last_used_at < TTL.as_secs() as i64
                    && live_targets.iter().any(|id| *id == state.target_id)
            });
        }
    }

    pub fn len(&self) -> usize {
        self.inner.read().map(|guard| guard.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 体积档位对应的基础等待预算（§10.3）。
///
/// 以**请求体字节数**为锚点：字节数在缓冲时就已经知道，零额外开销；估算
/// token 反而需要 tokenizer 或另一个粗估。
const SIZE_BUDGET: &[(usize, u64)] = &[
    (8 * 1024, 0),
    (24 * 1024, 5),
    (64 * 1024, 12),
    (160 * 1024, 25),
    (320 * 1024, 40),
    (640 * 1024, 60),
    (1200 * 1024, 75),
];
/// 超过最大档位后的预算。
const MAX_BUDGET: u64 = 90;

/// 淘汰最久未使用的若干条绑定（§19.4）。
///
/// 只在超过上限时调用。代价是一次 O(n log n) 排序，但按批淘汰之后它摊到上千次
/// 绑定上，可以忽略。被淘汰的前缀下一次请求会重新抽签——只损失那一个前缀的
/// 缓存，不会影响其他会话。
fn evict_oldest(guard: &mut HashMap<Key, Arc<BindingEntry>>) {
    let mut ages: Vec<(i64, Key)> = guard
        .iter()
        .map(|(key, entry)| {
            let last = entry
                .state
                .lock()
                .map(|state| state.last_used_at)
                .unwrap_or_default();
            (last, key.clone())
        })
        .collect();
    ages.sort_by_key(|(last, _)| *last);
    for (_, key) in ages.into_iter().take(EVICT_BATCH) {
        guard.remove(&key);
    }
    tracing::warn!(
        limit = MAX_BINDINGS,
        evicted = EVICT_BATCH,
        "粘性绑定表达到上限，已淘汰最久未使用的条目"
    );
}

/// 缓存新鲜度系数（§10.3）。
///
/// 粘性命中不等于缓存命中：Anthropic 的缓存默认 TTL 5 分钟，OpenAI 的自动
/// 缓存也在 5–10 分钟量级。上次命中是 20 分钟前的话，缓存早没了，等待毫无
/// 价值。
fn freshness(since_last_hit: i64) -> f64 {
    match since_last_hit {
        s if s < 4 * 60 => 1.0,
        s if s <= 6 * 60 => 0.5,
        _ => 0.1,
    }
}

/// 暴露给请求记录：这次粘性等待用的新鲜度系数（§24.1）。
pub fn freshness_for(since_last_hit: i64) -> f64 {
    freshness(since_last_hit)
}

/// 粘性请求愿意为"等到原目标空出来"付出的时间（§10.3）。
///
/// 换一次号的代价是一次完整的前缀缓存重建：100K 上下文下大约是缓存读价的
/// 20 倍，所以粘性请求的排队价值远高于普通请求，不能共用同一个等待上限。
pub fn wait_budget(request_bytes: usize, since_last_hit: i64) -> Duration {
    let base = SIZE_BUDGET
        .iter()
        .find(|(limit, _)| request_bytes < *limit)
        .map(|(_, budget)| *budget)
        .unwrap_or(MAX_BUDGET);
    Duration::from_secs_f64(base as f64 * freshness(since_last_hit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest() -> KeyDigest {
        crate::security::KeyDigest::for_tests([9u8; 32])
    }

    fn derive_prefix(body: &serde_json::Value) -> Option<(Key, Origin)> {
        derive(
            &digest(),
            "g1",
            "claude-sonnet-4-5",
            &HeaderMap::new(),
            body,
        )
    }

    #[test]
    fn compacting_the_conversation_does_not_break_the_binding() {
        // agent 客户端 /compact 后第一条用户消息被换成摘要——这恰好发生在
        // 上下文最大、缓存最值钱的时刻，粘性绝不能因此断链（§10.1）。
        let before = json!({
            "system": "你是一个编码助手",
            "tools": [{"name": "bash"}],
            "messages": [{"role": "user", "content": "读一下 main.rs"}],
        });
        let after = json!({
            "system": "你是一个编码助手",
            "tools": [{"name": "bash"}],
            "messages": [{"role": "user", "content": "【上下文摘要】之前我们在读 main.rs…"}],
        });
        assert_eq!(derive_prefix(&before), derive_prefix(&after));
    }

    #[test]
    fn injected_timestamps_in_the_first_user_message_do_not_reset_stickiness() {
        // 客户端常在第一条消息里注入当前时间、git 状态、目录快照。
        let make = |injected: &str| {
            json!({
                "system": "你是一个编码助手",
                "messages": [{"role": "user", "content": format!("当前时间 {injected}")}],
            })
        };
        assert_eq!(
            derive_prefix(&make("10:00:00")),
            derive_prefix(&make("10:00:01"))
        );
    }

    #[test]
    fn a_different_project_naturally_gets_a_different_key() {
        let a = json!({"system": "项目 A 的说明", "messages": []});
        let b = json!({"system": "项目 B 的说明", "messages": []});
        assert_ne!(derive_prefix(&a), derive_prefix(&b));
    }

    #[test]
    fn changing_the_tool_definitions_changes_the_key() {
        let a = json!({"system": "同样的说明", "tools": [{"name": "bash"}]});
        let b = json!({"system": "同样的说明", "tools": [{"name": "bash"}, {"name": "edit"}]});
        assert_ne!(derive_prefix(&a), derive_prefix(&b));
    }

    #[test]
    fn groups_and_models_never_share_a_binding() {
        let body = json!({"system": "同样的说明"});
        let headers = HeaderMap::new();
        let one = derive(&digest(), "g1", "m1", &headers, &body);
        let other_group = derive(&digest(), "g2", "m1", &headers, &body);
        let other_model = derive(&digest(), "g1", "m2", &headers, &body);
        assert_ne!(one, other_group);
        assert_ne!(one, other_model);
    }

    #[test]
    fn the_derivation_order_follows_the_specification() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "sess-1".parse().unwrap());
        let body = json!({
            "previous_response_id": "resp_123",
            "prompt_cache_key": "cache-1",
            "system": "说明",
        });

        // 1. 状态链最优先。
        assert_eq!(
            derive(&digest(), "g", "m", &headers, &body).unwrap().1,
            Origin::ResponseChain
        );

        let mut body = body;
        body.as_object_mut().unwrap().remove("previous_response_id");
        // 2. 显式会话头。
        assert_eq!(
            derive(&digest(), "g", "m", &headers, &body).unwrap().1,
            Origin::SessionHeader
        );

        // 3. prompt_cache_key。
        let empty = HeaderMap::new();
        assert_eq!(
            derive(&digest(), "g", "m", &empty, &body).unwrap().1,
            Origin::CacheKey
        );

        // 4. 稳定前缀。
        body.as_object_mut().unwrap().remove("prompt_cache_key");
        assert_eq!(
            derive(&digest(), "g", "m", &empty, &body).unwrap().1,
            Origin::StablePrefix
        );
    }

    #[test]
    fn a_bare_one_off_request_has_no_sticky_key() {
        // 没有 system prompt 也没有工具定义：粘上去毫无价值。
        let body = json!({"messages": [{"role": "user", "content": "你好"}]});
        assert!(derive_prefix(&body).is_none());
    }

    #[test]
    fn a_conversation_object_is_accepted_as_well_as_a_string() {
        let as_object = json!({"conversation": {"id": "conv_1"}});
        let as_string = json!({"conversation": "conv_1"});
        assert_eq!(derive_prefix(&as_object), derive_prefix(&as_string));
    }

    #[test]
    fn the_key_never_contains_the_prompt_itself() {
        let secret = "这是绝不能落库的系统提示词";
        let body = json!({"system": secret});
        let (key, _) = derive_prefix(&body).unwrap();
        assert!(!key.as_str().contains(secret));
        assert_eq!(key.as_str().len(), 64, "只保存 SHA-256 摘要");
    }

    #[test]
    fn bindings_slide_their_expiry_on_every_hit() {
        let bindings = Bindings::new();
        let (key, _) = derive_prefix(&json!({"system": "s"})).unwrap();
        bindings.bind(key.clone(), "g1", "m1", "tgt-a", None, 1_000);

        // 3000 秒后仍在，且这次命中把过期时间往后推。
        assert_eq!(bindings.get(&key, 4_000).unwrap().target_id, "tgt-a");
        assert!(bindings.get(&key, 4_000 + 3_500).is_some());
        // 从上次命中起超过 1 小时才真正过期。
        assert!(bindings.get(&key, 4_000 + 3_500 + 3_601).is_none());
    }

    #[test]
    fn bindings_survive_a_restart_and_lose_deleted_targets() {
        let bindings = Bindings::new();
        let (key, _) = derive_prefix(&json!({"system": "s"})).unwrap();
        bindings.bind(key.clone(), "g1", "m1", "tgt-a", None, 1_000);

        let exported = bindings.export();
        assert_eq!(exported.len(), 1);

        // §26.7：重启后同一前缀仍打到原目标。
        let restored = Bindings::new();
        restored.restore(&exported);
        assert_eq!(restored.get(&key, 1_100).unwrap().target_id, "tgt-a");

        restored.prune(&[], 1_100);
        assert!(restored.is_empty(), "指向已删除目标的绑定必须一起消失");
    }

    #[test]
    fn the_wait_budget_follows_size_and_freshness() {
        // §10.3 的两个例子：400 KB 请求，2 分钟前刚命中 → 60 秒；
        // 同样大小但 15 分钟前命中 → 6 秒。
        assert_eq!(wait_budget(400 * 1024, 2 * 60), Duration::from_secs(60));
        assert_eq!(wait_budget(400 * 1024, 15 * 60), Duration::from_secs(6));
    }

    #[test]
    fn small_requests_switch_immediately_instead_of_waiting() {
        // 小请求重建缓存几乎不花钱，等待纯属浪费时间。
        assert_eq!(wait_budget(4 * 1024, 0), Duration::ZERO);
        assert_eq!(wait_budget(7 * 1024, 60), Duration::ZERO);
    }

    #[test]
    fn the_budget_grows_monotonically_with_the_body_size() {
        let sizes = [
            10 * 1024,
            30 * 1024,
            100 * 1024,
            200 * 1024,
            400 * 1024,
            800 * 1024,
            2 * 1024 * 1024,
        ];
        let budgets: Vec<_> = sizes.iter().map(|s| wait_budget(*s, 0)).collect();
        assert!(
            budgets.windows(2).all(|pair| pair[1] > pair[0]),
            "{budgets:?}"
        );
        assert_eq!(*budgets.last().unwrap(), Duration::from_secs(MAX_BUDGET));
    }

    #[test]
    fn a_cold_cache_makes_waiting_nearly_worthless() {
        let big = 1024 * 1024;
        assert!(wait_budget(big, 20 * 60) < wait_budget(big, 60) / 5);
    }

    /// 绑定表到上限时淘汰最久未使用的条目，而不是无限增长（§19.4）。
    #[test]
    fn bindings_evict_the_oldest_when_the_table_is_full() {
        let bindings = Bindings::new();
        // 填满到上限，每条的最后使用时间依次递增：最先写入的最旧。
        for i in 0..MAX_BINDINGS {
            let (key, _) = derive_prefix(&json!({"system": format!("项目 {i}")})).unwrap();
            bindings.bind(key, "g1", "m1", "t1", None, i as i64);
        }
        assert_eq!(bindings.len(), MAX_BINDINGS);

        // 再写一条，触发按批淘汰。
        let (fresh, _) = derive_prefix(&json!({"system": "新项目"})).unwrap();
        bindings.bind(fresh.clone(), "g1", "m1", "t2", None, 1_000_000);
        assert!(
            bindings.len() <= MAX_BINDINGS,
            "淘汰后不该超过上限：{}",
            bindings.len()
        );
        assert_eq!(
            bindings.len(),
            MAX_BINDINGS - EVICT_BATCH + 1,
            "一次淘汰一批，加上新写入的一条"
        );
        // 刚写进去的必须在。
        assert!(
            bindings.get(&fresh, 1_000_000).is_some(),
            "新绑定不能被淘汰"
        );

        // 最旧的那些已经被淘汰。
        let (oldest, _) = derive_prefix(&json!({"system": "项目 0"})).unwrap();
        assert!(
            bindings.get(&oldest, 1_000_000).is_none(),
            "最久未使用的绑定应当先被淘汰"
        );
        // 较新的还在。查询时间要贴近它的写入时间，否则会被 1 小时的滑动过期
        // 判成失效——那是 TTL 的行为，不是淘汰的行为。
        let (newer, _) =
            derive_prefix(&json!({"system": format!("项目 {}", MAX_BINDINGS - 1)})).unwrap();
        assert!(
            bindings.get(&newer, MAX_BINDINGS as i64 - 1).is_some(),
            "较新的绑定不该被淘汰"
        );
    }
}
