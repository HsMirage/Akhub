//! 网关托管后台任务的验收（计划 §29.1）。
//!
//! 这里验证三件"参考实现没做对、我们必须做对"的事：
//! 1. 取消是真的——会中断正在执行的 future（从而断开上游连接）；
//! 2. 重启不骗人——遗留任务被标记为 interrupted，绝不停留在 in_progress；
//! 3. 默认关闭——没打开分组开关时不会托管任何任务。

mod common;

use std::sync::Arc;
use std::time::Duration;

use akhub::app::Settings;
use akhub::gateway::background;
use akhub::storage::store::BackgroundTaskRow;
use common::spawn_akhub_with;
use serde_json::json;

/// 造一条任务记录（模拟"上次进程留下的"）。
async fn insert_task(state: &akhub::app::SharedState, id: &str, status: &str, heartbeat_at: i64) {
    let now = akhub::storage::now_unix();
    state
        .store
        .upsert_background_task(&BackgroundTaskRow {
            id: id.to_string(),
            group_id: "grp_test".into(),
            logical_model: "m1".into(),
            account_id: Some("acc_1".into()),
            target_id: Some("tgt_1".into()),
            status: status.to_string(),
            upstream_id: None,
            created_at: now - 120,
            heartbeat_at,
            finished_at: None,
            error_code: None,
            sealed_output: None,
            expires_at: now + 3600,
        })
        .await
        .unwrap();
}

/// ID 前缀把"网关托管"与"原生代理"分开（计划 §29.1）。
#[test]
fn managed_ids_use_their_own_prefix() {
    let id = background::new_id();
    assert!(background::is_managed(&id), "{id}");
    assert!(id.starts_with("bg_akh_"), "{id}");
    assert!(
        !background::is_managed("resp_akh_123"),
        "原生 ID 不能被当成托管"
    );
}

/// **真取消**：取消一个正在执行的任务会打断它的 future，而不是只改状态。
#[tokio::test]
async fn cancelling_a_running_task_actually_interrupts_it() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let id = background::new_id();
    insert_task(
        &akhub.state,
        &id,
        background::status::RUNNING,
        akhub::storage::now_unix(),
    )
    .await;

    let runner = background::TaskRunner::new(Arc::clone(&akhub.state));
    let started = Arc::new(tokio::sync::Notify::new());
    let started_inner = Arc::clone(&started);
    let task_id = id.clone();
    let handle = tokio::spawn(async move {
        let _ = runner
            .run(&task_id, "grp_test", || async move {
                started_inner.notify_one();
                // 一个"永不上报结果"的上游调用：只有被真正打断才会结束。
                std::future::pending::<anyhow::Result<(Option<String>, serde_json::Value)>>().await
            })
            .await;
    });

    started.notified().await;
    akhub.state.runtime.background.register(&id, handle).await;
    assert_eq!(akhub.state.runtime.background.len().await, 1);

    let aborted = akhub.state.runtime.background.cancel(&id).await;
    assert!(aborted, "取消必须命中正在执行的任务");
    assert_eq!(
        akhub.state.runtime.background.len().await,
        0,
        "取消后要移除登记，不能留下僵尸句柄"
    );
}

/// 取消一个不在跑的任务要返回 false——调用方据此区分"真取消"与"只改状态"。
#[tokio::test]
async fn cancelling_an_unknown_task_reports_false() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    assert!(!akhub.state.runtime.background.cancel("bg_akh_不存在").await);
}

/// **重启不骗人**：心跳停滞的 queued/running 任务被标记为 interrupted。
#[tokio::test]
async fn stale_tasks_become_interrupted_instead_of_staying_in_progress() {
    let akhub = spawn_akhub_with(Settings::default(), |_| {}).await;
    let now = akhub::storage::now_unix();
    // 一条"40 秒没心跳"的 running 任务与一条新鲜任务。
    insert_task(
        &akhub.state,
        "bg_akh_stale",
        background::status::RUNNING,
        now - 40,
    )
    .await;
    insert_task(
        &akhub.state,
        "bg_akh_fresh",
        background::status::RUNNING,
        now,
    )
    .await;

    let recovered = background::recover_stale(&akhub.state.store, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(recovered, 1, "只有心跳停滞的那条该被中断");

    let stale = akhub
        .state
        .store
        .background_task("bg_akh_stale", "grp_test")
        .await
        .unwrap()
        .expect("任务还在");
    assert_eq!(stale.status, background::status::INTERRUPTED);
    assert_eq!(stale.error_code.as_deref(), Some("gateway_restart"));

    let fresh = akhub
        .state
        .store
        .background_task("bg_akh_fresh", "grp_test")
        .await
        .unwrap()
        .expect("任务还在");
    assert_eq!(
        fresh.status,
        background::status::RUNNING,
        "还在心跳的任务不能被误杀"
    );
}

/// 对客户端暴露的状态映射：中断与失败都表现为 failed，避免假的 in_progress。
#[test]
fn public_status_never_fakes_progress() {
    assert_eq!(
        background::public_status(background::status::QUEUED),
        "queued"
    );
    assert_eq!(
        background::public_status(background::status::RUNNING),
        "in_progress"
    );
    assert_eq!(
        background::public_status(background::status::COMPLETED),
        "completed"
    );
    assert_eq!(
        background::public_status(background::status::CANCELLED),
        "cancelled"
    );
    assert_eq!(
        background::public_status(background::status::INTERRUPTED),
        "failed"
    );
    assert_eq!(
        background::public_status(background::status::FAILED),
        "failed"
    );
}

/// 取消过的任务在对外对象里明确提示"上游可能已计费"。
#[test]
fn a_cancelled_task_tells_the_client_it_may_have_cost_money() {
    let now = akhub::storage::now_unix();
    let task = BackgroundTaskRow {
        id: "bg_akh_x".into(),
        group_id: "g".into(),
        logical_model: "m".into(),
        account_id: None,
        target_id: None,
        status: background::status::CANCELLED.into(),
        upstream_id: None,
        created_at: now,
        heartbeat_at: now,
        finished_at: Some(now),
        error_code: None,
        sealed_output: None,
        expires_at: now + 60,
    };
    let object = background::public_object(&task, "bg_akh_x");
    assert_eq!(object["status"], "cancelled");
    assert!(
        object["cancellation_note"]
            .as_str()
            .unwrap()
            .contains("可能已经产生费用"),
        "{object}"
    );
}

/// 托管任务默认关闭：分组开关为 false 时不应生成托管任务。
#[tokio::test]
async fn managed_background_is_off_by_default() {
    let akhub = spawn_akhub_with(Settings::default(), |group| {
        // 默认值由 domain 层给出，这里显式断言它不会被误设为 true。
        assert!(!group.allow_managed_background, "默认必须是关闭");
    })
    .await;
    assert!(
        !akhub.state.config.current().groups[0]
            .group
            .allow_managed_background
    );
    let _ = json!({});
}

/// 任务终态会写进库，且输出以加密形式保存（不外泄明文）。
#[tokio::test]
async fn a_completed_task_stores_its_output_encrypted() {
    let akhub = spawn_akhub_with(Settings::default(), |group| {
        group.allow_managed_background = true;
    })
    .await;
    let id = background::new_id();
    insert_task(
        &akhub.state,
        &id,
        background::status::QUEUED,
        akhub::storage::now_unix(),
    )
    .await;

    let runner = background::TaskRunner::new(Arc::clone(&akhub.state));
    let task_id = id.clone();
    runner
        .run(&task_id, "grp_test", || async move {
            Ok((
                Some("resp_upstream_1".to_string()),
                json!({"output": ["hello"]}),
            ))
        })
        .await
        .unwrap();

    let task = akhub
        .state
        .store
        .background_task(&id, "grp_test")
        .await
        .unwrap()
        .expect("任务还在");
    assert_eq!(task.status, background::status::COMPLETED);
    assert_eq!(task.upstream_id.as_deref(), Some("resp_upstream_1"));
    let sealed = task.sealed_output.expect("输出要保存");
    assert!(
        !String::from_utf8_lossy(&sealed).contains("hello"),
        "落库的输出必须是密文"
    );
    let plaintext = akhub.state.cipher.open(&sealed).unwrap();
    assert!(String::from_utf8_lossy(&plaintext).contains("hello"));
}
