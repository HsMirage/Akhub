//! 首次设置口令（§23.2）。
//!
//! 原生部署默认监听 0.0.0.0，首次设置接口又必须免登录——不加限制时，同一网络
//! 里谁先打开 `/admin` 谁就成了管理员。做法与 Jenkins 的初始解锁口令相同：
//! 还没有管理员时生成一次性口令，打到日志里并写进数据目录（仅属主可读），
//! 设置接口必须带上它；设置成功后删除文件。
//!
//! 口令以文件为唯一真相：进程重启时沿用已有文件，不会让日志里那串作废。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// 前端提交口令用的请求头。
pub const HEADER: &str = "x-akhub-setup-token";
/// 数据目录下的口令文件名。
pub const FILE_NAME: &str = "setup-token";

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

/// 确保口令文件存在并返回口令；已有就沿用。
pub fn ensure(data_dir: &Path) -> Result<String> {
    let file = path(data_dir);
    if let Some(existing) = read(data_dir) {
        return Ok(existing);
    }
    let token = URL_SAFE_NO_PAD.encode(&*crate::security::random_bytes(18)?);
    write_private(&file, token.as_bytes())
        .with_context(|| format!("写入首次设置口令失败：{}", file.display()))?;
    Ok(token)
}

/// 呈递的口令是否正确。文件不存在时一律拒绝。
pub fn verify(data_dir: &Path, presented: &str) -> bool {
    read(data_dir).is_some_and(|expected| {
        crate::security::ct_eq(expected.as_bytes(), presented.trim().as_bytes())
    })
}

/// 启动时把口令醒目地打出来。
///
/// 直接写 stderr 而不走 tracing：全局日志层会把长串不透明字符当密钥脱敏，
/// 而这一串恰恰是要给人看、要复制的。`docker logs` / `journalctl` 都能看到。
pub fn announce(data_dir: &Path, token: &str) {
    let bar = "*".repeat(64);
    eprintln!(
        "\n{bar}\n尚未创建管理员。首次设置需要下面这个一次性口令：\n\n    {token}\n\n\
         口令同时保存在 {}（仅属主可读），设置完成后自动删除。\n{bar}\n",
        path(data_dir).display()
    );
}

/// 设置完成（或已有管理员）后删除口令文件。
pub fn remove(data_dir: &Path) {
    let file = path(data_dir);
    match std::fs::remove_file(&file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, path = %file.display(), "删除首次设置口令文件失败"),
    }
}

fn read(data_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path(data_dir)).ok()?;
    let token = raw.trim();
    (!token.is_empty()).then(|| token.to_string())
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_stable_across_restarts_and_verifies_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        let token = ensure(dir.path()).unwrap();
        assert_eq!(ensure(dir.path()).unwrap(), token, "重启后沿用同一口令");
        assert!(verify(dir.path(), &token));
        assert!(!verify(dir.path(), "猜的"));
        assert!(!verify(dir.path(), ""));
        remove(dir.path());
        assert!(!verify(dir.path(), &token), "删除后任何口令都不再有效");
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        ensure(dir.path()).unwrap();
        let mode = std::fs::metadata(path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
