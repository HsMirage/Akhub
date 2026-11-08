//! 备份跨机恢复：分组 Key 必须照常可用（§20.2、§23.5）。

use akhub::app::{AppState, Settings, SharedState};
use akhub::auth::{Credential, authenticate};
use akhub::domain::{Group, Multiplier, SchedulingWeights};
use akhub::storage::store::ids;
use time::OffsetDateTime;

async fn add_group(state: &SharedState, name: &str) -> String {
    let (key, prefix) = akhub::security::generate_group_key().unwrap();
    let group = Group {
        id: ids::group(),
        name: name.into(),
        key_prefix: prefix,
        key_digest_hex: state.key_digest.digest_hex(&key),
        multiplier_limit: Multiplier::ONE,
        weights: SchedulingWeights::default(),
        queue_capacity: 100,
        max_wait_secs: 60,
        allow_managed_background: false,
        allow_degrade: true,
        created_at: OffsetDateTime::now_utc(),
    };
    state.store.insert_group(&group).await.unwrap();
    key.to_string()
}

fn authenticates(state: &SharedState, key: &str) -> bool {
    let config = state.config.current();
    let credential = Credential {
        key: key.to_string(),
        anthropic_style: false,
    };
    authenticate(&config, &state.key_digest, &credential).is_ok()
}

#[tokio::test]
async fn group_keys_survive_a_restore_on_a_machine_with_another_master_key() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = AppState::bootstrap(source_dir.path(), Settings::default())
        .await
        .unwrap();
    let first = add_group(&source, "主力").await;
    let second = add_group(&source, "备用").await;
    source.reload_config().await.unwrap();
    assert!(authenticates(&source, &first) && authenticates(&source, &second));

    let bytes = akhub::security::backup::export_backup(&source.store, &source.cipher, "口令")
        .await
        .unwrap();

    let target_dir = tempfile::tempdir().unwrap();
    let target = AppState::bootstrap(target_dir.path(), Settings::default())
        .await
        .unwrap();
    assert_ne!(
        std::fs::read(source_dir.path().join("master.key")).unwrap(),
        std::fs::read(target_dir.path().join("master.key")).unwrap(),
        "两台机器的主密钥必须不同，测试才有意义"
    );
    let data = akhub::security::backup::decrypt_backup(&bytes, "口令").unwrap();
    target
        .store
        .import_backup(&data, &target.cipher)
        .await
        .expect("两个分组的恢复不能撞唯一约束");
    target.reload_config().await.unwrap();

    assert!(authenticates(&target, &first), "第一个分组的 Key 必须可用");
    assert!(authenticates(&target, &second), "第二个分组的 Key 必须可用");
    assert!(!authenticates(&target, "akh-随便写的"));

    // 恢复后的机器重启一次：pepper 已用本机主密钥密封落库，照常可用。
    drop(target);
    let restarted = AppState::bootstrap(target_dir.path(), Settings::default())
        .await
        .unwrap();
    assert!(authenticates(&restarted, &first));
}

/// 备份文件来自外部：账号 Base URL 指向内网又没开内网访问时，拒绝恢复。
#[tokio::test]
async fn a_backup_pointing_at_metadata_endpoints_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::bootstrap(dir.path(), Settings::default())
        .await
        .unwrap();
    add_group(&state, "主力").await;
    let bytes = akhub::security::backup::export_backup(&state.store, &state.cipher, "pw")
        .await
        .unwrap();
    let mut data = akhub::security::backup::decrypt_backup(&bytes, "pw").unwrap();
    data.accounts.push(serde_json::json!({
        "id": "acc", "name": "坏账号", "base_url": "http://169.254.169.254/latest",
        "allow_private_network": false,
    }));
    let error = akhub::security::backup::validate_restorable(&data)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("坏账号"), "{error}");

    // 显式开启内网访问的账号照常放行。
    data.accounts.last_mut().unwrap()["allow_private_network"] = serde_json::json!(true);
    akhub::security::backup::validate_restorable(&data).unwrap();
}
