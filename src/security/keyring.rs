//! 启动时的主密钥核对与分组 Key pepper 装载（§20.2、§23.5）。
//!
//! 两件事放在一起，是因为它们都必须在"数据库已打开、服务还没起来"这一刻
//! 决定，而且都关系到同一个问题：**这份数据库是不是用这把主密钥加密的**。
//!
//! - 主密钥核对：`app_settings` 里存一条用主密钥密封的校验值。当前密钥打不开
//!   它，说明 `master.key` 丢了或 `AKHUB_MASTER_KEY` 配错了——此时拒绝启动，
//!   而不是悄悄生成新密钥、跳过所有上游 Key、`/health/ready` 照样 200。
//! - 分组 Key pepper：独立随机的 32 字节，密封后存在 `app_settings`。它随加密
//!   备份迁移，换机恢复后下游 Key 照常可用。

use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::{Cipher, KeyDigest, MasterKey};
use crate::storage::Store;

/// 主密钥校验值在 `app_settings` 里的键。
pub const VERIFIER_KEY: &str = "master_key_verifier";
/// 分组 Key pepper（已密封）在 `app_settings` 里的键。
pub const PEPPER_KEY: &str = "group_key_pepper";
/// 这两条都与本机主密钥绑定，绝不能随备份原样搬到另一台机器。
pub const MACHINE_BOUND_SETTINGS: [&str; 2] = [VERIFIER_KEY, PEPPER_KEY];

/// 校验值的明文。内容无所谓，只要固定。
const VERIFIER_PLAINTEXT: &[u8] = b"akhub:master-key-verifier:v1";
/// 无校验值时最多抽多少条密文试解。
const SAMPLE_LIMIT: i64 = 64;

/// 解锁后的密钥材料。
pub struct Keys {
    pub cipher: Cipher,
    pub key_digest: KeyDigest,
    pub master_key_from_env: bool,
}

/// 加载主密钥并与数据库核对，再装载分组 Key pepper。
///
/// 规则：
/// 1. 没有主密钥：数据库里没有任何密封数据才生成新密钥，否则拒绝启动；
/// 2. 有校验值：当前密钥必须能打开它；
/// 3. 没有校验值但有密封数据（升级前的库）：至少一条能解开才补写校验值；
/// 4. 没有任何密封数据：直接写校验值。
pub async fn unlock(data_dir: &Path, store: &Store) -> Result<Keys> {
    let verifier = store.app_setting(VERIFIER_KEY).await?;
    let samples = sealed_samples(store).await?;
    let sealed_present = verifier.is_some() || !samples.is_empty();

    let master = match MasterKey::load(data_dir)? {
        Some(master) => master,
        None if sealed_present => bail!(missing_key_message(data_dir)),
        None => {
            tracing::info!(path = %super::master_key_path(data_dir).display(), "已生成新的主密钥");
            MasterKey::generate(data_dir)?
        }
    };
    let cipher = master.cipher();

    match verifier {
        Some(encoded) => {
            let opened = STANDARD
                .decode(encoded.as_bytes())
                .ok()
                .and_then(|sealed| cipher.open(&sealed).ok());
            if opened.as_deref().map(Vec::as_slice) != Some(VERIFIER_PLAINTEXT) {
                bail!(mismatch_message(data_dir, master.from_env));
            }
        }
        None => {
            if !samples.is_empty() && !samples.iter().any(|blob| cipher.open(blob).is_ok()) {
                bail!(mismatch_message(data_dir, master.from_env));
            }
            store
                .set_app_setting(
                    VERIFIER_KEY,
                    &STANDARD.encode(cipher.seal(VERIFIER_PLAINTEXT)?),
                )
                .await
                .context("写入主密钥校验值失败")?;
        }
    }

    let pepper = match store.app_setting(PEPPER_KEY).await? {
        Some(encoded) => open_pepper(&cipher, &encoded)?,
        None => {
            // 已有分组的摘要是用早期"由主密钥派生"的 pepper 算的，沿用它；
            // 全新的库直接随机生成。
            let pepper = if has_groups(store).await? {
                master.derived_digest_pepper()
            } else {
                random_pepper()?
            };
            store
                .set_app_setting(PEPPER_KEY, &seal_pepper(&cipher, &pepper)?)
                .await
                .context("写入分组 Key pepper 失败")?;
            pepper
        }
    };

    Ok(Keys {
        cipher,
        key_digest: KeyDigest::new(pepper),
        master_key_from_env: master.from_env,
    })
}

/// 读取当前库里的 pepper（恢复备份之后刷新内存用）。
pub async fn load_pepper(store: &Store, cipher: &Cipher) -> Result<[u8; 32]> {
    let encoded = store
        .app_setting(PEPPER_KEY)
        .await?
        .context("数据库里缺少分组 Key pepper")?;
    open_pepper(cipher, &encoded)
}

/// 生成一个新的随机 pepper。
pub fn random_pepper() -> Result<[u8; 32]> {
    let bytes = super::random_bytes(32)?;
    let mut pepper = [0u8; 32];
    pepper.copy_from_slice(&bytes);
    Ok(pepper)
}

/// 用本机主密钥密封 pepper，得到可写进 `app_settings` 的文本。
pub fn seal_pepper(cipher: &Cipher, pepper: &[u8; 32]) -> Result<String> {
    Ok(STANDARD.encode(cipher.seal(pepper)?))
}

fn open_pepper(cipher: &Cipher, encoded: &str) -> Result<[u8; 32]> {
    let sealed = STANDARD
        .decode(encoded.as_bytes())
        .context("分组 Key pepper 编码损坏")?;
    let plain = cipher.open(&sealed).context("分组 Key pepper 解不开")?;
    <[u8; 32]>::try_from(plain.as_slice()).context("分组 Key pepper 长度非法")
}

/// 抽取若干条用主密钥密封的数据。空串与过短的值不算（不可能是信封）。
async fn sealed_samples(store: &Store) -> Result<Vec<Vec<u8>>> {
    sqlx::query_scalar(
        "SELECT blob FROM (
             SELECT sealed_key AS blob FROM upstream_account_keys
             UNION ALL SELECT api_key FROM upstream_secrets
             UNION ALL SELECT new_api_token FROM upstream_secrets
             UNION ALL SELECT sealed_token FROM new_api_sites
         ) WHERE blob IS NOT NULL AND length(blob) > 24
         LIMIT ?",
    )
    .bind(SAMPLE_LIMIT)
    .fetch_all(store.pool())
    .await
    .context("读取密封数据样本失败")
}

async fn has_groups(store: &Store) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM groups")
        .fetch_one(store.pool())
        .await?;
    Ok(count > 0)
}

fn missing_key_message(data_dir: &Path) -> String {
    format!(
        "找不到主密钥，但数据库里已有用主密钥加密的数据，拒绝启动（不会生成新密钥覆盖）。\n\
         请把原来的主密钥文件放回 {}，或设置 AKHUB_MASTER_KEY 为原值后再启动。\n\
         如果原密钥确已丢失：移走整个数据目录重新初始化，再在后台用加密备份文件恢复配置。",
        super::master_key_path(data_dir).display()
    )
}

fn mismatch_message(data_dir: &Path, from_env: bool) -> String {
    let source = if from_env {
        "环境变量 AKHUB_MASTER_KEY".to_string()
    } else {
        format!("主密钥文件 {}", super::master_key_path(data_dir).display())
    };
    format!(
        "主密钥与数据库不匹配：当前使用的{source}解不开库里的加密数据，拒绝启动。\n\
         请恢复原来的 master.key 文件，或把 AKHUB_MASTER_KEY 改回原值（注意它优先于密钥文件）。\n\
         如果原密钥确已丢失：移走整个数据目录重新初始化，再在后台用加密备份文件恢复配置。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store_in(dir: &Path) -> Store {
        Store::new(crate::storage::open(dir).await.unwrap())
    }

    #[tokio::test]
    async fn a_fresh_directory_gets_a_key_verifier_and_random_pepper() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        let keys = unlock(dir.path(), &store).await.unwrap();
        assert!(super::super::master_key_path(dir.path()).exists());
        assert!(store.app_setting(VERIFIER_KEY).await.unwrap().is_some());
        let pepper = load_pepper(&store, &keys.cipher).await.unwrap();
        assert_eq!(
            keys.key_digest.digest("akh-x"),
            KeyDigest::new(pepper).digest("akh-x")
        );
        // 第二次启动用同一把密钥，照常通过。
        unlock(dir.path(), &store).await.unwrap();
    }

    #[tokio::test]
    async fn a_lost_master_key_refuses_to_start_instead_of_generating_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        unlock(dir.path(), &store).await.unwrap();
        std::fs::remove_file(super::super::master_key_path(dir.path())).unwrap();

        let error = unlock(dir.path(), &store).await.err().unwrap().to_string();
        assert!(error.contains("找不到主密钥"), "{error}");
        assert!(
            !super::super::master_key_path(dir.path()).exists(),
            "绝不能悄悄生成新密钥"
        );
    }

    #[tokio::test]
    async fn a_different_master_key_refuses_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        unlock(dir.path(), &store).await.unwrap();
        std::fs::remove_file(super::super::master_key_path(dir.path())).unwrap();
        std::fs::write(super::super::master_key_path(dir.path()), [9u8; 32]).unwrap();

        let error = unlock(dir.path(), &store).await.err().unwrap().to_string();
        assert!(error.contains("主密钥与数据库不匹配"), "{error}");
    }

    /// 升级前的库没有校验值：密封数据一条都解不开时拒绝，而不是补写一个错的校验值。
    #[tokio::test]
    async fn legacy_sealed_data_without_verifier_must_decrypt_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path()).await;
        let other = MasterKey::for_tests(3).cipher();
        sqlx::query(
            "INSERT INTO new_api_sites (base_url, user_id, sealed_token, updated_at)
             VALUES ('https://x.example', '1', ?, 0)",
        )
        .bind(other.seal(b"token").unwrap())
        .execute(store.pool())
        .await
        .unwrap();
        std::fs::write(super::super::master_key_path(dir.path()), [1u8; 32]).unwrap();

        let error = unlock(dir.path(), &store).await.err().unwrap().to_string();
        assert!(error.contains("主密钥与数据库不匹配"), "{error}");
        assert!(store.app_setting(VERIFIER_KEY).await.unwrap().is_none());

        // 换成真正加密它的那把：通过，并补写校验值。
        std::fs::write(super::super::master_key_path(dir.path()), [3u8; 32]).unwrap();
        unlock(dir.path(), &store).await.unwrap();
        assert!(store.app_setting(VERIFIER_KEY).await.unwrap().is_some());
    }
}
