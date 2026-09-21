//! 账号内 Key 池的运行时快照（§4.2.1）。
//!
//! 数据库里的每把 Key 都是独立密封的密文。网关的热路径既不该每次尝试都做一次
//! 主键查询 + AEAD 解密，也不该为了"这次用哪把"再去读一遍库：因此这里把 Key
//! 池整体解密成一份**不可变快照**，与 [`crate::config::RuntimeConfig`] 同一套
//! 语义——通过 `ArcSwap` 一次替换，在途请求持有旧 `Arc`，不会读到一半新一半旧。
//!
//! 三条边界：
//!
//! 1. **明文只在内存。** 不进日志、不进备份、不进 API 响应、不实现会打印出
//!    明文的 `Debug`（§23.4）。
//! 2. **摘要用于归类状态。** Key 级熔断、额度与粘性绑定按
//!    [`credential_id`] 归类，而不是按行 ID：改标签、重新粘贴同一把 Key
//!    都不该丢掉这些状态（§4.2.1 的不变量 C）。
//! 3. **空 Key 池是合法状态。** 账号可以一把 Key 都没有；路由层据此把它判为
//!    不合格，而不是让热路径去处理 `Option`。

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::domain::{Account, Limits};
use crate::security::Cipher;

/// 一把可用于推理的 Key。
pub struct Credential {
    /// 行 ID。只用于界面操作与请求记录的定位，不参与状态归类。
    pub id: String,
    pub account_id: String,
    /// 管理员填的标签；留空时界面按序号显示。
    pub label: String,
    pub enabled: bool,
    /// Key 级限额覆盖，逐项盖住账号默认值。
    pub limits: Limits,
    /// 明文凭据。只在内存，绝不外泄。
    pub secret: Arc<str>,
    /// 明文摘要（hex），Key 级动态状态的归类键。
    pub credential_digest: String,
}

impl fmt::Debug for Credential {
    /// 手写 `Debug`：默认派生会把明文打进任何一次 `{:?}`，包括 `tracing`
    /// 的字段输出。这里只打印能安全出现在日志里的部分。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("id", &self.id)
            .field("account_id", &self.account_id)
            .field("label", &self.label)
            .field("enabled", &self.enabled)
            .field("limits", &self.limits)
            .field("credential_digest", &self.credential_digest)
            .finish_non_exhaustive()
    }
}

/// 账号内 Key 池的状态归类键。
///
/// 形如 `账号ID:摘要`。分开账号是必要的：同一个真实 Key 可以出现在两个分组
/// 的两个账号里（§4.2），那时它们的熔断与额度状态必须各自独立。
pub fn credential_id(account_id: &str, credential_digest: &str) -> String {
    format!("{account_id}:{credential_digest}")
}

/// 一份不可变的 Key 池快照。
#[derive(Debug, Default)]
pub struct CredentialPool {
    accounts: HashMap<String, Vec<Arc<Credential>>>,
}

impl CredentialPool {
    /// 从数据库读取全部账号的 Key 池并解密。
    pub async fn load(
        store: &crate::storage::Store,
        cipher: &Cipher,
        accounts: &[Account],
    ) -> Result<Self> {
        let mut pool = Self::default();
        for account in accounts {
            let rows = store
                .list_account_key_rows(&account.id)
                .await
                .with_context(|| format!("读取账号「{}」的 Key 池失败", account.name))?;
            let mut keys = Vec::with_capacity(rows.len());
            for row in rows {
                // 单把 Key 解不开只丢这一把，绝不让整个网关起不来：密文损坏、
                // 主密钥换过、备份跨机恢复不完整都会走到这里。后台会把它显示成
                // "这个账号少了一把 Key"，管理员补一次即可（§4.2.1）。
                let plaintext = match cipher.open(&row.sealed_key) {
                    Ok(plaintext) => plaintext,
                    Err(error) => {
                        tracing::warn!(
                            account = %account.name,
                            key = %row.id,
                            %error,
                            "这把 Key 的密文解不开，已跳过（该账号少一把可用凭据）"
                        );
                        continue;
                    }
                };
                let secret = match String::from_utf8(plaintext.to_vec()) {
                    Ok(secret) => secret,
                    Err(_) => {
                        tracing::warn!(
                            account = %account.name,
                            key = %row.id,
                            "这把 Key 的明文不是合法 UTF-8，已跳过"
                        );
                        continue;
                    }
                };
                keys.push(Arc::new(Credential {
                    id: row.id,
                    account_id: account.id.clone(),
                    label: row.label,
                    enabled: row.enabled,
                    limits: row.limits,
                    // 摘要按明文现算，且用**固定标签的 HMAC**：数据库里的那一列
                    // 只是它的持久化副本，现算保证"状态归类键"永远与真正的凭据
                    // 一致。绝不能掺主密钥——换机恢复、重装之后同一把 Key 必须
                    // 还是同一个摘要，否则熔断状态每次部署都归零（§4.2.1）。
                    credential_digest: crate::security::credential_digest(&secret),
                    secret: Arc::from(secret.as_str()),
                }));
            }
            pool.accounts.insert(account.id.clone(), keys);
        }
        Ok(pool)
    }

    /// 为一个账号登记一组 Key。
    ///
    /// 供测试与手工装配使用：网关自己只通过 [`Self::load`] 从数据库装载。
    pub fn insert(&mut self, account_id: &str, keys: Vec<Arc<Credential>>) {
        self.accounts.insert(account_id.to_string(), keys);
    }

    /// 一个账号的全部 Key，已启用与未启用都在里面，顺序稳定。
    pub fn keys_of(&self, account_id: &str) -> &[Arc<Credential>] {
        self.accounts
            .get(account_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// 按行 ID 找一把 Key。
    pub fn by_id(&self, account_id: &str, key_id: &str) -> Option<&Arc<Credential>> {
        self.keys_of(account_id).iter().find(|key| key.id == key_id)
    }

    /// 按凭据摘要找一把 Key。迁移与备份恢复后行 ID 会变，摘要不会。
    pub fn by_digest(&self, account_id: &str, credential_digest: &str) -> Option<&Arc<Credential>> {
        self.keys_of(account_id)
            .iter()
            .find(|key| key.credential_digest == credential_digest)
    }

    /// 账号是否一把 Key 都没有（从未配置或全部被删）。
    pub fn is_empty(&self, account_id: &str) -> bool {
        self.keys_of(account_id).is_empty()
    }

    /// 当前快照里全部 Key 的归类键，供动态状态表清理已消失的条目。
    pub fn live_credential_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .accounts
            .iter()
            .flat_map(|(account_id, keys)| {
                keys.iter()
                    .map(move |key| credential_id(account_id, &key.credential_digest))
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

/// 造一把测试用的 Key。
///
/// 明文是假的，但**摘要必须由 (账号, 序号) 决定且非空**：粘性绑定、健康状态
/// 与请求记录都按摘要归类，随手写死一个"d1"会让跨账号的测试互相串台。
#[cfg(test)]
pub(crate) fn test_credential(account_id: &str, id: &str) -> Arc<Credential> {
    let digest = crate::security::credential_digest(&format!("{account_id}/{id}"));
    Arc::new(Credential {
        id: id.to_string(),
        account_id: account_id.to_string(),
        label: id.to_string(),
        enabled: true,
        limits: Limits::default(),
        secret: Arc::from(format!("sk-test-{account_id}-{id}").as_str()),
        credential_digest: digest,
    })
}

/// 一次 Key 选择的结果（§4.2.1 的不变量 B）。
pub enum KeyChoice<'a> {
    /// 定了：就是这一把（或这个账号本来就不需要凭据）。
    Ready(Option<&'a Arc<Credential>>),
    /// 所有能用的 Key 都只是"忙"。
    ///
    /// 调用方应当**等待**而不是换账号：忙不等于坏，这条规则与账号层的
    /// §13.6 完全一致——否则一个满负荷的账号会因为"它的 Key 都在忙"而被
    /// 判成不可用，白白让出本可以排队等到的流量。
    Busy,
}

/// 在一组 Key 里挑一把。
///
/// 规则按优先级：
///
/// 1. `pinned`（响应链钉住的凭据）在候选里，直接用——这是"续写必须回到
///    创建它的那把 Key"的唯一实现点。
/// 2. 粘性绑定记着的那把在候选里，直接用——**这一步就是"缓存亲和优先于层内
///    评分"**：已建立的会话不因为另一把 Key 分数更高而迁移。
/// 3. 其余情况才抽签。
///
/// `selectable` 返回 `Ok(false)` 表示这把 Key 不可用（停用、熔断、额度耗尽、
/// 限额满），`Err(())` 表示它只是"忙"。两者必须分开：前者该被跳过，后者该
/// 让整个请求去排队。
/// `excluded` 是本次请求**已经试失败**的那把 Key（§4.2.1）。抽签必须跳过它：
/// 它可能还没被标记成"坏"（例如上游拒绝的方式不构成 KeyInvalid），但只要这一次
/// 调用已经用它打失败过，再抽中它就是纯粹的重复劳动。
pub fn select_key<'a, F>(
    keys: &'a [Arc<Credential>],
    pinned: Option<&str>,
    sticky: Option<&str>,
    excluded: Option<&str>,
    mut selectable: F,
    random: &mut impl FnMut() -> f64,
) -> KeyChoice<'a>
where
    F: FnMut(&Arc<Credential>) -> Result<bool, ()>,
{
    let mut available: Vec<&Arc<Credential>> = Vec::with_capacity(keys.len());
    let mut enabled = 0usize;
    let mut busy = 0usize;
    for key in keys.iter().filter(|key| key.enabled) {
        enabled += 1;
        // 已经试过的这把不参与抽签，也不计入 busy：它是"这次不行"，
        // 不是"整个账号在忙"。
        if excluded.is_some_and(|digest| digest == key.credential_digest) {
            continue;
        }
        match selectable(key) {
            Ok(true) => available.push(key),
            Ok(false) => continue,
            Err(()) => busy += 1,
        }
    }

    // 钉住与粘性都只在"这把 Key 现在能用"时生效。它不可用时才轮到下一把：
    // 这正是"换掉一把坏 Key 就该重新开始"的语义。
    // 钉住与粘性只在"这把 Key 现在能用、且这次还没试过"时生效。
    for preferred in [pinned, sticky] {
        if let Some(id) = preferred
            && !excluded.is_some_and(|digest| digest == id)
            && let Some(found) = available.iter().find(|key| key.credential_digest == id)
        {
            return KeyChoice::Ready(Some(found));
        }
    }

    if available.is_empty() {
        // 没有任何一把在忙 → 全是坏 Key，或者账号根本一把启用的 Key 都没有，
        // 这时应该报"账号不可用"而不是让调用方去排队等一个不会好的账号。
        return if busy > 0 && busy == enabled {
            KeyChoice::Busy
        } else {
            KeyChoice::Ready(None)
        };
    }

    // 加权随机排出一组尝试顺序。权重用目标级性能统计：Key 之间的差异主要
    // 来自凭据配额与限流，而那已经由健康状态精确表达，再叠一层 EWMA 只会
    // 让择时变慢、让样本被稀释（§4.2.1）。
    let order = shuffle_keys(&available, random);
    KeyChoice::Ready(Some(order[0]))
}

/// 对一组候选 Key 做均匀随机排序，返回第一个作为本次选择。
///
/// 用 Fisher–Yates：每个排列等概率，且**只依赖注入的随机源**，测试因此可以
/// 精确断言"同一条前缀永远回到同一把 Key"、"两把 Key 的分配比例随分数变化"。
fn shuffle_keys<'a>(
    candidates: &[&'a Arc<Credential>],
    random: &mut impl FnMut() -> f64,
) -> Vec<&'a Arc<Credential>> {
    let mut pool = candidates.to_vec();
    let mut out = Vec::with_capacity(pool.len());
    while !pool.is_empty() {
        let draw = random().clamp(0.0, 0.999_999);
        let index = (draw * pool.len() as f64) as usize;
        let index = index.min(pool.len() - 1);
        out.push(pool.remove(index));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一把测试用的 Key。明文是假的，摘要由调用方给定。
    pub(crate) fn credential(
        account_id: &str,
        id: &str,
        digest: &str,
        enabled: bool,
    ) -> Arc<Credential> {
        Arc::new(Credential {
            id: id.into(),
            account_id: account_id.into(),
            label: String::new(),
            enabled,
            limits: Limits::default(),
            secret: Arc::from("sk-secret-value"),
            credential_digest: digest.into(),
        })
    }

    fn pool() -> CredentialPool {
        let mut pool = CredentialPool::default();
        pool.accounts.insert(
            "a1".into(),
            vec![
                credential("a1", "k1", "d1", true),
                credential("a1", "k2", "d2", false),
            ],
        );
        pool.accounts.insert("a2".into(), vec![]);
        pool
    }

    /// 摘要必须由固定标签的 HMAC 决定，跨进程、跨机器可复现。
    ///
    /// 值由外部工具算好后钉在这里：一旦有人往 HMAC 里掺主密钥或随机量，
    /// "重新粘贴同一把 Key 就认不出它是同一把"会立刻在这里暴露。
    #[test]
    fn the_credential_digest_is_a_stable_fixed_label_hmac() {
        assert_eq!(
            crate::security::credential_digest("key-A1"),
            "dddd9c8eeac561c16dd17d1007c7f3d5bea8e3fc2c0a6964e2c3bec5cd17cb1d"
        );
    }

    #[test]
    fn keys_are_found_by_row_id_and_by_digest() {
        let pool = pool();
        assert_eq!(pool.by_id("a1", "k2").unwrap().credential_digest, "d2");
        // 行 ID 变了（迁移、备份恢复）之后摘要仍然能对上。
        assert_eq!(pool.by_digest("a1", "d1").unwrap().id, "k1");
        assert!(pool.by_id("a1", "不存在").is_none());
        assert!(pool.by_id("a2", "k1").is_none());
    }

    #[test]
    fn an_account_without_keys_reports_empty() {
        let pool = pool();
        assert!(pool.is_empty("a2"), "零 Key 的账号必须被识别出来");
        assert!(!pool.is_empty("a1"));
        assert!(pool.is_empty("不存在的账号"));
    }

    #[test]
    fn live_ids_are_scoped_per_account() {
        let pool = pool();
        let ids = pool.live_credential_ids();
        assert_eq!(ids, vec!["a1:d1".to_string(), "a1:d2".to_string()]);
    }

    // ------------------------------------------------------ Key 选择（§4.2.1）

    fn pool_of(digests: &[&str]) -> Vec<Arc<Credential>> {
        digests
            .iter()
            .enumerate()
            .map(|(index, digest)| credential("a1", &format!("k{index}"), digest, true))
            .collect()
    }

    /// 注入一个固定序列的随机源，让抽签结果可断言。
    fn fixed(values: &[f64]) -> impl FnMut() -> f64 + '_ {
        let mut index = 0;
        move || {
            let value = values[index % values.len()];
            index += 1;
            value
        }
    }

    #[test]
    fn a_pinned_key_wins_over_everything() {
        let keys = pool_of(&["d1", "d2", "d3"]);
        let choice = select_key(
            &keys,
            Some("d3"),
            Some("d1"),
            None,
            |_| Ok(true),
            &mut fixed(&[0.0]),
        );
        let KeyChoice::Ready(Some(key)) = choice else {
            panic!("必须选中被钉住的那把");
        };
        assert_eq!(key.credential_digest, "d3");
    }

    /// 不变量 B：粘性绑定在可用时优先于重新抽签。
    #[test]
    fn a_sticky_key_is_never_replaced_by_a_fresh_draw() {
        let keys = pool_of(&["d1", "d2"]);
        // 抽签的随机源会说"选第一把"，但粘性绑定指向第二把。
        let choice = select_key(
            &keys,
            None,
            Some("d2"),
            None,
            |_| Ok(true),
            &mut fixed(&[0.0]),
        );
        let KeyChoice::Ready(Some(key)) = choice else {
            panic!("必须命中粘性绑定");
        };
        assert_eq!(key.credential_digest, "d2", "已建立的会话不得迁移");
    }

    /// 钉住与粘性只在"它现在能用"时生效：坏掉的 Key 必须让位。
    #[test]
    fn a_pinned_key_yields_when_it_is_no_longer_usable() {
        let keys = pool_of(&["d1", "d2"]);
        let choice = select_key(
            &keys,
            Some("d1"),
            None,
            None,
            |key| {
                if key.credential_digest == "d1" {
                    // 熔断：不是"忙"，是"坏"。
                    Ok(false)
                } else {
                    Ok(true)
                }
            },
            &mut fixed(&[0.0]),
        );
        let KeyChoice::Ready(Some(key)) = choice else {
            panic!("应当退到另一把可用的 Key");
        };
        assert_eq!(key.credential_digest, "d2");
    }

    /// 全部可用 Key 都在忙时是"等待"而不是"换账号"（与 §13.6 同一条规则）。
    #[test]
    fn all_busy_keys_ask_the_caller_to_wait() {
        let keys = pool_of(&["d1", "d2"]);
        assert!(matches!(
            select_key(&keys, None, None, None, |_| Err(()), &mut fixed(&[0.5])),
            KeyChoice::Busy
        ));
    }

    /// 全坏与全忙是两回事：全坏要报账号不可用，不能去排队。
    #[test]
    fn all_broken_keys_are_unavailable_not_busy() {
        let keys = pool_of(&["d1", "d2"]);
        assert!(matches!(
            select_key(&keys, None, None, None, |_| Ok(false), &mut fixed(&[0.5])),
            KeyChoice::Ready(None)
        ));
    }

    #[test]
    fn disabled_keys_never_take_part() {
        let keys = vec![
            credential("a1", "k1", "d1", false),
            credential("a1", "k2", "d2", true),
        ];
        let choice = select_key(
            &keys,
            Some("d1"),
            None,
            None,
            |_| Ok(true),
            &mut fixed(&[0.0]),
        );
        let KeyChoice::Ready(Some(key)) = choice else {
            panic!("应当选中唯一启用的那把");
        };
        assert_eq!(key.credential_digest, "d2", "停用的 Key 不能被钉住");
    }

    /// 账号一把 Key 都没有时返回"无凭据"，而不是"忙"。
    #[test]
    fn an_empty_pool_is_ready_without_a_credential() {
        assert!(matches!(
            select_key(&[], None, None, None, |_| Ok(true), &mut fixed(&[0.5])),
            KeyChoice::Ready(None)
        ));
    }

    /// 抽签在池内均匀铺开：固定随机源下每把 Key 都会被选中过。
    #[test]
    fn fresh_draws_cover_the_whole_pool() {
        let keys = pool_of(&["d1", "d2", "d3"]);
        let mut seen = std::collections::HashSet::new();
        // 每轮换一个起点，模拟不同的随机序列。
        for start in 0..3 {
            let values: Vec<f64> = (0..3).map(|i| ((start + i) % 3) as f64 / 3.0).collect();
            let choice = select_key(&keys, None, None, None, |_| Ok(true), &mut fixed(&values));
            let KeyChoice::Ready(Some(key)) = choice else {
                panic!("必须选中一把");
            };
            seen.insert(key.credential_digest.clone());
        }
        assert_eq!(seen.len(), 3, "每把 Key 都应该有机会被选中：{seen:?}");
    }

    /// 一把解不开的 Key 只丢它自己，不能让整个快照装载失败。
    ///
    /// 密文损坏、主密钥换过、备份跨机恢复不完整都会走到这条路径；如果这里
    /// 直接报错，等于**一把坏凭据让整台网关起不来**（§4.2.1）。
    #[tokio::test]
    async fn an_undecryptable_key_is_skipped_instead_of_failing_the_pool() {
        let store = crate::storage::Store::new(crate::storage::open_in_memory().await.unwrap());
        let cipher = crate::security::Cipher::for_tests(&[7u8; 32]);
        // 账号有外键指向分组：先建一个最小分组。
        sqlx::query(
            "INSERT INTO groups (id, name, key_prefix, key_digest_hex, multiplier_limit,
                weight_multiplier, weight_reliability, weight_first_token, weight_throughput,
                queue_capacity, allow_degrade, created_at)
             VALUES ('g1', '测试分组', 'akh-x', 'digest-x', 1000000, 40, 25, 20, 15, 10, 1, 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        let account = Account {
            id: "a1".into(),
            group_id: "g1".into(),
            name: "测试账号".into(),

            base_url: "https://api.example.com".into(),
            preferred_protocol: crate::domain::Protocol::OpenAiChat,
            adaptive_protocol: true,
            default_priority: 0,
            calibration: crate::domain::Multiplier::ONE,
            multiplier_mode: crate::domain::MultiplierMode::Manual,
            manual_multiplier: crate::domain::Multiplier::ONE,
            new_api_user_id: None,
            new_api_group: None,
            limits: Limits::default(),
            allow_private_network: false,
            enabled: true,
            hide_original: false,
            auto_sync: false,
            model_synced_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        store
            .insert_account(
                &account,
                &crate::storage::store::AccountSecrets::new(vec![0xDE, 0xAD], None),
            )
            .await
            .unwrap();
        // 再补一把好的，确认"坏的被跳过"不等于"整个池空了"。
        store
            .upsert_account_secret(
                &account.id,
                &cipher.seal(b"sk-good").unwrap(),
                &crate::security::credential_digest("sk-good"),
            )
            .await
            .unwrap();

        let pool = CredentialPool::load(&store, &cipher, std::slice::from_ref(&account))
            .await
            .expect("单把 Key 坏掉不能让快照装载失败");
        assert_eq!(pool.keys_of(&account.id).len(), 1);
        assert_eq!(&*pool.keys_of(&account.id)[0].secret, "sk-good");
    }

    /// 明文绝不能进 `Debug`：`tracing` 的字段输出会用到它。
    #[test]
    fn debug_never_prints_the_secret() {
        let key = credential("a1", "k1", "d1", true);
        let printed = format!("{key:?}");
        assert!(!printed.contains("sk-secret-value"), "{printed}");
        assert!(printed.contains("credential_digest"), "{printed}");
    }
}
