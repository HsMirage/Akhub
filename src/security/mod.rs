//! 主密钥、对称加密、Key 摘要、日志脱敏与 SSRF 防护。
//!
//! 所有需要落库的上游凭据都经过 [`Cipher`] 加密；下游分组 Key 只保存
//! 带 pepper 的 HMAC 摘要，不保存可恢复明文（§20.2）。

pub mod backup;
pub mod redact;
pub mod url_guard;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

pub const MASTER_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// 生成 `n` 字节密码学随机数据。
pub fn random_bytes(n: usize) -> Result<Zeroizing<Vec<u8>>> {
    let mut buf = Zeroizing::new(vec![0u8; n]);
    getrandom::fill(buf.as_mut_slice()).context("操作系统随机数源不可用")?;
    Ok(buf)
}

/// 进程主密钥。优先使用 `AKHUB_MASTER_KEY`，否则使用数据目录下的密钥文件。
pub struct MasterKey {
    bytes: Zeroizing<[u8; MASTER_KEY_LEN]>,
    /// 主密钥来自环境变量而非数据目录文件。
    pub from_env: bool,
}

impl MasterKey {
    /// 加载主密钥；数据目录中不存在时生成一个仅当前用户可读写的新文件。
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        if let Ok(raw) = std::env::var("AKHUB_MASTER_KEY") {
            let decoded = decode_key_material(raw.trim())
                .context("AKHUB_MASTER_KEY 必须是 32 字节的 hex 或 base64 值")?;
            return Ok(Self {
                bytes: decoded,
                from_env: true,
            });
        }

        let path = master_key_path(data_dir);
        if path.exists() {
            let raw = std::fs::read(&path)
                .with_context(|| format!("读取主密钥文件失败：{}", path.display()))?;
            if raw.len() != MASTER_KEY_LEN {
                bail!(
                    "主密钥文件长度非法：期望 {MASTER_KEY_LEN} 字节，实际 {}",
                    raw.len()
                );
            }
            let mut bytes = Zeroizing::new([0u8; MASTER_KEY_LEN]);
            bytes.copy_from_slice(&raw);
            return Ok(Self {
                bytes,
                from_env: false,
            });
        }

        let generated = random_bytes(MASTER_KEY_LEN)?;
        write_private_file(&path, &generated)?;
        let mut bytes = Zeroizing::new([0u8; MASTER_KEY_LEN]);
        bytes.copy_from_slice(&generated);
        Ok(Self {
            bytes,
            from_env: false,
        })
    }

    /// 用于加密上游凭据的 AEAD。
    pub fn cipher(&self) -> Cipher {
        Cipher::new(&derive_subkey(&self.bytes, b"akhub:secret-box:v1"))
    }

    /// 用于下游分组 Key 摘要的 pepper。
    pub fn key_digest(&self) -> KeyDigest {
        KeyDigest {
            pepper: derive_subkey(&self.bytes, b"akhub:group-key-digest:v1"),
        }
    }
}

pub fn master_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("master.key")
}

fn decode_key_material(raw: &str) -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>> {
    let decoded = hex::decode(raw)
        .ok()
        .or_else(|| URL_SAFE_NO_PAD.decode(raw).ok())
        .or_else(|| base64::engine::general_purpose::STANDARD.decode(raw).ok())
        .context("无法解析为 hex 或 base64")?;
    if decoded.len() != MASTER_KEY_LEN {
        bail!(
            "长度非法：期望 {MASTER_KEY_LEN} 字节，实际 {}",
            decoded.len()
        );
    }
    let mut out = Zeroizing::new([0u8; MASTER_KEY_LEN]);
    out.copy_from_slice(&decoded);
    Ok(out)
}

/// 用固定标签从主密钥派生子密钥，让不同用途互不相关。
fn derive_subkey(master: &[u8; MASTER_KEY_LEN], label: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256>>::new_from_slice(master).expect("HMAC-SHA256 接受任意长度密钥");
    mac.update(label);
    mac.finalize().into_bytes().into()
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("创建主密钥文件失败：{}", path.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

/// XChaCha20-Poly1305 信封加密。每条记录使用独立随机 nonce。
#[derive(Clone)]
pub struct Cipher {
    aead: XChaCha20Poly1305,
}

impl Cipher {
    fn new(key: &[u8; 32]) -> Self {
        Self {
            aead: XChaCha20Poly1305::new(key.into()),
        }
    }

    /// 加密为 `nonce || ciphertext` 的自包含信封。
    /// 固定密钥的构造器，只给测试用（生产路径一律经 [\`MasterKey::cipher\`]）。
    #[cfg(test)]
    pub fn for_tests(key: &[u8; 32]) -> Self {
        Self::new(key)
    }

    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce_bytes = random_bytes(NONCE_LEN)?;
        let nonce = XNonce::try_from(&nonce_bytes[..]).expect("nonce 长度由常量保证");
        let mut out = Vec::with_capacity(NONCE_LEN + plaintext.len() + 16);
        out.extend_from_slice(&nonce);
        let ciphertext = self
            .aead
            .encrypt(&nonce, plaintext)
            .map_err(|_| anyhow::anyhow!("加密失败"))?;
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// 解密 [`Cipher::seal`] 产生的信封；篡改会导致认证失败。
    pub fn open(&self, envelope: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        if envelope.len() <= NONCE_LEN {
            bail!("密文长度非法");
        }
        let (nonce_bytes, ciphertext) = envelope.split_at(NONCE_LEN);
        let plaintext = self
            .aead
            .decrypt(
                &XNonce::try_from(nonce_bytes).expect("nonce 长度已在上面校验"),
                ciphertext,
            )
            .map_err(|_| anyhow::anyhow!("解密失败：密文被篡改或主密钥不匹配"))?;
        Ok(Zeroizing::new(plaintext))
    }

    /// 从备份密码派生一把对称密钥（Argon2id，§23.5）。
    ///
    /// 盐随机生成并随密文一起保存；派生参数固定在代码里——备份的互操作性
    /// 优先于参数可调性，参数升级意味着旧备份无法恢复，必须走版本号。
    pub fn derive_backup_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
        use argon2::Argon2;
        let mut key = [0u8; 32];
        Argon2::default()
            .hash_password_into(password.as_bytes(), salt, &mut key)
            .map_err(|e| anyhow::anyhow!("备份密钥派生失败：{e}"))?;
        Ok(key)
    }
}

/// 下游分组 Key 的不可逆摘要器。
#[derive(Clone)]
pub struct KeyDigest {
    pepper: [u8; 32],
}

impl KeyDigest {
    /// 固定 pepper 的构造器，只给测试用。
    #[cfg(test)]
    pub fn for_tests(pepper: [u8; 32]) -> Self {
        Self { pepper }
    }

    /// 计算摘要。相同输入恒等，用于 O(1) 查表；比较在等长数组上进行。
    pub fn digest(&self, key: &str) -> [u8; 32] {
        let mut mac =
            <Hmac<Sha256>>::new_from_slice(&self.pepper).expect("HMAC-SHA256 接受任意长度密钥");
        mac.update(key.as_bytes());
        mac.finalize().into_bytes().into()
    }

    pub fn digest_hex(&self, key: &str) -> String {
        hex::encode(self.digest(key))
    }
}

/// 上游凭据的稳定摘要（hex），用于把 Key 级动态状态归到一起。
///
/// **只用固定标签的 HMAC，不掺主密钥**：同一把 Key 在换机恢复、重装或重新
/// 密封之后必须得到同一个摘要，否则"改个标签就丢掉熔断状态"或"重新粘贴
/// 同一把 Key 就认不出它是同一把"会成为常态。
///
/// 摘要不是凭据：它既不能用于鉴权，也不足以从 128 位以上随机熵的 Key 反推
/// 原文。它进入内存状态表与数据库的 `credential_digest` 列，不进日志、
/// 不进 API 响应（§23.4）。
pub fn credential_digest(api_key: &str) -> String {
    let mut mac = <Hmac<Sha256>>::new_from_slice(b"akhub:credential-digest:v1")
        .expect("HMAC-SHA256 接受任意长度密钥");
    mac.update(api_key.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 定长常量时间比较（§19.2）。
///
/// 长度不同直接返回 false（长度本身不是秘密）；长度相同时逐字节异或累加，
/// **不提前返回**，因此耗时只与长度有关，与"前几个字节猜对了"无关。
pub fn ct_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 生成一把新的下游分组 Key，返回明文与可显示前缀。
pub fn generate_group_key() -> Result<(Zeroizing<String>, String)> {
    let entropy = random_bytes(32)?;
    let key = Zeroizing::new(format!("akh-{}", URL_SAFE_NO_PAD.encode(&*entropy)));
    let prefix = key.chars().take(12).collect::<String>();
    Ok((key, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_master() -> MasterKey {
        MasterKey {
            bytes: Zeroizing::new([7u8; MASTER_KEY_LEN]),
            from_env: true,
        }
    }

    #[test]
    fn seal_open_roundtrip() {
        let cipher = test_master().cipher();
        let sealed = cipher.seal(b"sk-secret").unwrap();
        assert_eq!(&*cipher.open(&sealed).unwrap(), b"sk-secret");
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let cipher = test_master().cipher();
        let mut sealed = cipher.seal(b"sk-secret").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(cipher.open(&sealed).is_err());
    }

    #[test]
    fn nonce_is_unique_per_record() {
        let cipher = test_master().cipher();
        assert_ne!(cipher.seal(b"same").unwrap(), cipher.seal(b"same").unwrap());
    }

    #[test]
    fn digest_is_stable_and_key_specific() {
        let digest = test_master().key_digest();
        assert_eq!(digest.digest("akh-a"), digest.digest("akh-a"));
        assert_ne!(digest.digest("akh-a"), digest.digest("akh-b"));
    }

    #[test]
    fn generated_key_prefix_matches_key() {
        let (key, prefix) = generate_group_key().unwrap();
        assert!(key.starts_with(&prefix));
        assert_eq!(prefix.len(), 12);
    }
}
