//! 自更新的落地路径：下载 → 校验 sha256 → 原子替换二进制。
//!
//! 全部在临时目录里做：目标是测试自己造的文件，绝不碰正在运行的测试二进制。
//! 部署形态用 Registry::with_deploy 覆盖，不去改进程环境变量——测试是并行的。

mod common;

use akhub::update::{Deploy, Registry};
use common::{FakeGithub, make_release_archive};

const NEW_BINARY: &[u8] = b"#!/bin/sh\necho akhub 9.9.9\n";

fn write_old_binary(dir: &std::path::Path) -> std::path::PathBuf {
    let target = dir.join("akhub");
    std::fs::write(&target, b"old-binary").unwrap();
    target
}

/// 临时目录里不该留下任何 .akhub-* 半成品（写坏的二进制比旧二进制危险得多）。
fn leftovers(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(".akhub-"))
        .collect()
}

#[tokio::test]
async fn a_verified_release_replaces_the_binary_and_keeps_a_backup() {
    let archive = make_release_archive("9.9.9", NEW_BINARY);
    let github = FakeGithub::spawn("v9.9.9", archive).await;
    let dir = tempfile::tempdir().unwrap();
    let target = write_old_binary(dir.path());

    let registry = Registry::new(github.base_url.clone(), false).with_deploy(Deploy::Binary);
    let outcome = registry.install(&target, None).await.expect("更新应当成功");

    assert_eq!(outcome.from, env!("CARGO_PKG_VERSION"));
    assert_eq!(outcome.to, "9.9.9");
    assert!(outcome.need_restart, "换完二进制必须重启才生效");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        NEW_BINARY,
        "目标必须被换成新二进制"
    );

    let backup = outcome.backup.expect("必须留下旧版本备份");
    assert_eq!(std::fs::read(&backup).unwrap(), b"old-binary");
    assert!(leftovers(dir.path()).is_empty(), "临时文件没清干净");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "替换后的二进制必须可执行");
    }

    // 更新完成后再查一次：进程里记着「已落盘待重启」的版本，磁盘上已经是新的。
    let status = registry.check(true).await;
    assert_eq!(status.pending_version.as_deref(), Some("9.9.9"));
    assert_eq!(
        status.current,
        env!("CARGO_PKG_VERSION"),
        "当前进程版本不变"
    );
}

#[tokio::test]
async fn a_bad_checksum_stops_the_update_before_anything_is_written() {
    let archive = make_release_archive("9.9.9", NEW_BINARY);
    // 形状合法但内容必然对不上的哈希：验证的是"校验不过就停"，而不是"清单里没有这一行"。
    let github = FakeGithub::spawn_with_checksum("v9.9.9", archive, &"0".repeat(64)).await;
    let dir = tempfile::tempdir().unwrap();
    let target = write_old_binary(dir.path());

    let registry = Registry::new(github.base_url.clone(), false).with_deploy(Deploy::Binary);
    let error = registry.install(&target, None).await.unwrap_err();
    assert!(
        error.contains("sha256"),
        "失败原因要说清是校验不过：{error}"
    );

    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"old-binary",
        "校验不过必须保持原二进制不动"
    );
    assert!(leftovers(dir.path()).is_empty());
    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .all(|entry| !entry.file_name().to_string_lossy().contains(".bak-")),
        "还没替换就不该产生备份"
    );
}

#[tokio::test]
async fn an_explicit_version_can_be_installed_even_without_asking_the_latest() {
    let archive = make_release_archive("9.9.9", NEW_BINARY);
    let github = FakeGithub::spawn("v9.9.9", archive).await;
    let dir = tempfile::tempdir().unwrap();
    let target = write_old_binary(dir.path());

    let registry = Registry::new(github.base_url.clone(), false).with_deploy(Deploy::Binary);
    // 指定版本时只查这个 tag，不碰 /releases/latest。
    let outcome = registry
        .install(&target, Some("v9.9.9"))
        .await
        .expect("指定版本应当能装");
    assert_eq!(outcome.to, "9.9.9");
    assert_eq!(github.hits(), 0, "指定版本不该再去查 latest");
}

#[tokio::test]
async fn container_and_source_deploys_are_refused_with_a_usable_command() {
    let dir = tempfile::tempdir().unwrap();
    let target = write_old_binary(dir.path());

    let docker = Registry::new("http://127.0.0.1:1", false).with_deploy(Deploy::Docker);
    let error = docker.install(&target, Some("9.9.9")).await.unwrap_err();
    assert!(error.contains("docker compose"), "{error}");
    assert_eq!(std::fs::read(&target).unwrap(), b"old-binary");

    let source = Registry::new("http://127.0.0.1:1", false).with_deploy(Deploy::Source);
    let error = source.install(&target, Some("9.9.9")).await.unwrap_err();
    assert!(error.contains("cargo build"), "{error}");

    let windows = Registry::new("http://127.0.0.1:1", false).with_deploy(Deploy::Windows);
    let error = windows.install(&target, Some("9.9.9")).await.unwrap_err();
    assert!(error.contains("install.ps1"), "{error}");
}

#[tokio::test]
async fn installing_from_a_directory_we_cannot_write_says_what_to_do_instead() {
    // 用一个不存在的目录扮演「没有权限」：失败原因必须包含可执行的替代路径。
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("missing").join("akhub");
    let registry = Registry::new("http://127.0.0.1:1", false).with_deploy(Deploy::Binary);
    let error = registry.install(&target, Some("9.9.9")).await.unwrap_err();
    assert!(
        error.contains("sudo akhub --update") || error.contains("权限"),
        "{error}"
    );
}
