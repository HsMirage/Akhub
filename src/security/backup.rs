//! 配置备份与恢复（§23.5）。
//!
//! 导出：内存里生成只含配置的规范化 JSON，用 Argon2id 从备份密码派生密钥，
//! 整体 XChaCha20-Poly1305 加密。备份包含分组、账号（含上游 Key）、模型
//! 选择集、别名、逻辑模型与调度目标；绝不包含请求正文、Responses 历史、
//! 性能记录、粘性绑定或运行日志。
//!
//! 恢复：先完整解密校验，再通过配置事务原子切换。密码错误、文件损坏或
//! 出现 0 个有效分组时不修改现有配置。
//!
//! 上游 Key 的重加密：备份里保存的是**明文 Key**（整体已被备份口令加密），
//! 恢复时用**本机主密钥**重新 seal——备份文件跨机器可用，密文不跨机器。

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::security::{self, Cipher};

/// 备份格式版本。恢复时校验；不认识的版本绝不尝试。
pub const BACKUP_FORMAT_VERSION: u64 = 1;

/// 随密文一起保存的盐长度（字节）。
const SALT_LEN: usize = 16;
/// XChaCha20-Poly1305 的 nonce 长度。
const NONCE_LEN: usize = 24;

/// 备份里的一份完整配置快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupData {
    pub format_version: u64,
    pub created_at: i64,
    pub groups: Vec<Value>,
    pub accounts: Vec<Value>,
    /// 上游凭据：`{account_id, api_key, new_api_token}`，明文（整体加密保护）。
    ///
    /// v10 起它是 **Key 池第一把 Key 的镜像**：Key 池是唯一真相，这里保留一份
    /// 是为了让旧二进制与旧备份的恢复路径仍然可用（§4.2.1）。
    pub secrets: Vec<Value>,
    /// 账号内 Key 池：`{id, account_id, label, api_key, limits..., enabled}`。
    ///
    /// 带 `serde(default)`：v10 之前导出的备份没有这一项，恢复时按"从
    /// `secrets` 展开成一把 Key"处理，不能因为多了个字段就拒绝一份本来
    /// 完好的备份（§23.5）。
    #[serde(default)]
    pub account_keys: Vec<Value>,
    pub logical_models: Vec<Value>,
    pub dispatch_targets: Vec<Value>,
    pub account_models: Vec<Value>,
    /// 系统设置（保留期、超时、同步间隔等）。
    ///
    /// 带 `serde(default)`：v1 时代导出的备份没有这一项，恢复时按默认值处理，
    /// 不能因为多了个字段就拒绝一份本来完好的备份（§23.5）。
    #[serde(default)]
    pub app_settings: Vec<Value>,
    /// New API 站点级凭据：`{base_url, user_id, token}`，令牌明文（整体加密保护）。
    #[serde(default)]
    pub new_api_sites: Vec<Value>,
    /// 分组 Key 摘要用的 pepper（hex）。分组摘要只有配上它才能在别的机器上
    /// 验证下游 Key（§20.2）。早期备份没有这一项。
    #[serde(default)]
    pub group_key_pepper: Option<String>,
}

impl BackupData {
    /// 解出 pepper；格式不对视为损坏的备份。
    pub fn group_key_pepper(&self) -> Result<Option<[u8; 32]>> {
        let Some(raw) = &self.group_key_pepper else {
            return Ok(None);
        };
        let bytes = hex::decode(raw).context("备份里的分组 Key pepper 不是合法 hex")?;
        let pepper = <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| anyhow::anyhow!("备份里的分组 Key pepper 长度非法"))?;
        Ok(Some(pepper))
    }
}

/// 导出的加密信封。
#[derive(Debug, Serialize, Deserialize)]
struct BackupEnvelope {
    format_version: u64,
    /// base64 盐；派生 Argon2id 密钥用。
    salt: String,
    /// base64 密文（nonce 前置）。
    ciphertext: String,
}

/// 从当前数据库导出一份配置快照并加密（§23.5）。
pub async fn export_backup(
    store: &crate::storage::Store,
    cipher: &Cipher,
    password: &str,
) -> Result<Vec<u8>> {
    if password.is_empty() {
        bail!("备份密码不能为空");
    }
    let data = BackupData {
        format_version: BACKUP_FORMAT_VERSION,
        created_at: crate::storage::now_unix(),
        groups: store.backup_groups().await?,
        accounts: store.backup_accounts().await?,
        secrets: store.backup_secrets(cipher).await?,
        account_keys: store.backup_account_keys(cipher).await?,
        logical_models: store.backup_logical_models().await?,
        dispatch_targets: store.backup_targets().await?,
        account_models: store.backup_account_models().await?,
        app_settings: store.backup_app_settings().await?,
        new_api_sites: store.backup_new_api_sites(cipher).await?,
        group_key_pepper: Some(hex::encode(
            crate::security::keyring::load_pepper(store, cipher).await?,
        )),
    };
    if data.groups.is_empty() {
        bail!("当前配置没有任何分组，导出空备份没有意义");
    }

    let plaintext = serde_json::to_vec(&data)?;
    encrypt_envelope(&plaintext, password)
}

/// 解密一份备份并返回配置快照（不落库，校验由调用方继续）。
pub fn decrypt_backup(bytes: &[u8], password: &str) -> Result<BackupData> {
    let plaintext = decrypt_envelope(bytes, password)?;
    let data: BackupData =
        serde_json::from_slice(&plaintext).context("备份内容不是合法的配置快照")?;
    if data.format_version != BACKUP_FORMAT_VERSION {
        bail!(
            "备份格式版本 {} 不受支持（当前支持 {}）",
            data.format_version,
            BACKUP_FORMAT_VERSION
        );
    }
    if data.groups.is_empty() {
        bail!("备份里没有任何分组：损坏或非 Akhub 备份");
    }
    data.group_key_pepper()?;
    validate_restorable(&data)?;
    Ok(data)
}

/// 恢复前复查账号与站点的 Base URL（§23.3）：备份文件来自外部，不能绕过
/// 保存账号时的那道 SSRF 校验。账号显式开启了内网访问的才放行内网地址。
pub fn validate_restorable(data: &BackupData) -> Result<()> {
    use crate::security::url_guard::{UrlGuardError, validate_base_url};
    for account in &data.accounts {
        let raw = account
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let allow_private = match account.get("allow_private_network") {
            Some(Value::Bool(value)) => *value,
            Some(Value::Number(value)) => value.as_i64().unwrap_or(0) != 0,
            _ => false,
        };
        match validate_base_url(raw) {
            Ok(_) | Err(UrlGuardError::BlockedAddress(_)) if allow_private => {}
            Ok(_) => {}
            Err(error) => {
                let name = account
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                bail!("备份里的账号「{name}」Base URL 不可用：{error}");
            }
        }
    }
    for site in &data.new_api_sites {
        let raw = site
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Err(error) = validate_base_url(raw) {
            // 站点凭据被内网账号共用时同样允许内网：只拦格式问题。
            if !matches!(error, UrlGuardError::BlockedAddress(_)) {
                bail!("备份里的站点 {raw} 不可用：{error}");
            }
        }
    }
    Ok(())
}

/// 用 Argon2id + XChaCha20-Poly1305 加密（§23.5）。
fn encrypt_envelope(plaintext: &[u8], password: &str) -> Result<Vec<u8>> {
    // random_bytes 返回 Zeroizing<Vec<u8>>；取 &[u8] 切片做派生与编码。
    let salt = security::random_bytes(SALT_LEN)?;
    let key = Cipher::derive_backup_key(password, salt.as_ref())?;
    let aead = XChaCha20Poly1305::new((&key).into());
    let nonce_bytes = security::random_bytes(NONCE_LEN)?;
    let nonce = XNonce::try_from(&nonce_bytes[..]).expect("nonce 长度由常量保证");
    let ciphertext = aead
        .encrypt(&nonce, plaintext)
        .map_err(|_| anyhow::anyhow!("备份加密失败"))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);

    let envelope = BackupEnvelope {
        format_version: BACKUP_FORMAT_VERSION,
        salt: STANDARD_NO_PAD.encode(salt.as_slice()),
        ciphertext: STANDARD_NO_PAD.encode(&out),
    };
    Ok(serde_json::to_vec_pretty(&envelope)?)
}

fn decrypt_envelope(bytes: &[u8], password: &str) -> Result<Vec<u8>> {
    let envelope: BackupEnvelope =
        serde_json::from_slice(bytes).context("文件不是合法的 Akhub 备份信封")?;
    let salt = STANDARD_NO_PAD
        .decode(envelope.salt.as_bytes())
        .context("备份盐损坏")?;
    if salt.len() != SALT_LEN {
        bail!("备份盐长度异常");
    }
    let raw = STANDARD_NO_PAD
        .decode(envelope.ciphertext.as_bytes())
        .context("备份密文损坏")?;
    if raw.len() <= NONCE_LEN {
        bail!("备份密文长度异常");
    }
    let key = Cipher::derive_backup_key(password, &salt)?;
    let aead = XChaCha20Poly1305::new((&key).into());
    let (nonce_bytes, ciphertext) = raw.split_at(NONCE_LEN);
    let nonce = XNonce::try_from(nonce_bytes).expect("nonce 长度已在上面校验");
    aead.decrypt(&nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("备份密码错误或文件被篡改"))
        .context("备份解密失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_roundtrips_and_rejects_wrong_passwords() {
        let data = BackupData {
            format_version: BACKUP_FORMAT_VERSION,
            created_at: 1,
            groups: vec![serde_json::json!({"id": "g1", "name": "主力"})],
            accounts: vec![],
            secrets: vec![serde_json::json!({"account_id": "a1", "api_key": "sk-test"})],
            logical_models: vec![],
            dispatch_targets: vec![],
            account_models: vec![],
            account_keys: vec![],
            app_settings: vec![],
            new_api_sites: vec![],
            group_key_pepper: None,
        };
        let bytes = encrypt_envelope(&serde_json::to_vec(&data).unwrap(), "口令123").unwrap();

        // 密码错误：明确报错，绝不返回半份数据。
        let wrong = decrypt_envelope(&bytes, "口令456");
        assert!(wrong.is_err());

        let decrypted = decrypt_backup(&bytes, "口令123").unwrap();
        assert_eq!(decrypted.groups, data.groups);
        assert_eq!(decrypted.secrets, data.secrets);
    }

    #[test]
    fn a_tampered_file_is_rejected() {
        let data = BackupData {
            format_version: BACKUP_FORMAT_VERSION,
            created_at: 1,
            groups: vec![serde_json::json!({"id": "g1"})],
            accounts: vec![],
            secrets: vec![],
            logical_models: vec![],
            dispatch_targets: vec![],
            account_models: vec![],
            account_keys: vec![],
            app_settings: vec![],
            new_api_sites: vec![],
            group_key_pepper: None,
        };
        let mut bytes = encrypt_envelope(&serde_json::to_vec(&data).unwrap(), "pw").unwrap();
        // 翻转信封 JSON 里的一个密文字节。
        let last = bytes.last_mut().unwrap();
        *last ^= 0x01;
        assert!(
            decrypt_backup(&bytes, "pw").is_err(),
            "篡改必须被认证标签拦下"
        );
    }
}
