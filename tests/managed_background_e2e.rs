//! 托管后台任务的端到端验收（计划 §29.1）。
//!
//! 覆盖"上游不支持原生后台 + 分组开了开关"这条链路：
//! 提交 `background:true` → 拿到 `bg_akh_*` → 任务在后台真的打到上游 →
//! 查询得到 completed 与完整输出 → 取消时真的中断连接。

mod common;

use akhub::domain::{Limits, MultiplierMode, Protocol};
use akhub::gateway::background;
use common::{FakeUpstream, TargetSpec, client, spawn_akhub_with, wire_target};
use serde_json::{Value, json};
use std::time::Duration;

/// Chat 类假上游：`wire_target` 里用 `openai_chat` 协议 + `pinned`，
/// 这样账号的 Responses 端点会被判定为"没有原生后台能力"，从而走托管。
async fn managed_setup(allow_managed: bool) -> (common::Akhub, FakeUpstream, String) {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub_with(akhub::app::Settings::default(), move |group| {
        group.allow_managed_background = allow_managed;
    })
    .await;
    let wired = wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "gpt-5",
            "glm-4.6",
            50,
        )
        .pinned(),
    )
    .await;
    (akhub, upstream, wired.account_id)
}

/// 开关关闭时，`background:true` 不会托管：走正常转发路径
///（这里上游是 Chat，跨协议转成 Responses，仍然拿到普通响应而不是 bg_ 任务）。
#[tokio::test]
async fn managed_background_stays_off_until_the_group_enables_it() {
    let (akhub, _upstream, _account) = managed_setup(false).await;
    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "你好", "background": true}))
        .send()
        .await
        .unwrap();
    // 关键断言：没有拿到托管任务 ID。
    let body: Value = response.json().await.unwrap();
    assert!(
        body["id"]
            .as_str()
            .is_none_or(|id| !id.starts_with("bg_akh_")),
        "开关关闭时不该托管：{body}"
    );
    assert!(
        !background::is_managed(body["id"].as_str().unwrap_or_default()),
        "开关关闭时 ID 不能是托管前缀：{body}"
    );
}

/// 开关打开时，提交 `background:true` 立刻拿到 `bg_akh_*`，
/// 后台真的执行完，查询能拿到 completed 与完整输出。
#[tokio::test]
async fn an_enabled_group_returns_a_managed_task_that_finishes_with_output() {
    let (akhub, upstream, _account) = managed_setup(true).await;
    let response = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "你好", "background": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let created: Value = response.json().await.unwrap();
    let id = created["id"].as_str().expect("必须返回任务 ID").to_string();
    assert!(
        id.starts_with("bg_akh_"),
        "托管任务要用自己的前缀：{created}"
    );
    assert_eq!(created["background"], true, "{created}");

    // 任务在后台执行；等到 completed（上游是本地假上游，很快）。
    let mut latest = created.clone();
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let body: Value = client()
            .get(format!("{}/v1/responses/{id}", akhub.base_url))
            .bearer_auth(&akhub.key)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        latest = body;
        if latest["status"] == "completed" {
            break;
        }
    }
    assert_eq!(latest["status"], "completed", "任务最终要完成：{latest}");
    // 上游确实收到了请求，且任务里存下了真实响应对象。
    assert!(upstream.requests() >= 1, "托管任务必须真的打到上游");
    assert!(
        latest["content"].is_array() || latest["output"].is_array(),
        "查询要带回上游的真实响应内容：{latest}"
    );
    assert_eq!(
        latest["id"].as_str(),
        Some(id.as_str()),
        "对外只暴露网关 ID"
    );
}

/// 取消一个还在跑的任务：必须真的中断执行，并如实报告是否打断了连接。
#[tokio::test]
async fn cancelling_a_managed_task_reports_whether_it_aborted_a_connection() {
    let upstream = FakeUpstream::spawn().await;
    // 上游一直挂起：任务会停在 running，好让我们取消一个"确实在跑"的任务。
    upstream.fallback(common::Behavior::Hang);
    let akhub = spawn_akhub_with(akhub::app::Settings::default(), |group| {
        group.allow_managed_background = true;
    })
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "gpt-5",
            "glm-4.6",
            50,
        )
        .pinned(),
    )
    .await;

    let created: Value = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "你好", "background": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // 等任务真的进入执行（上游收到请求）。
    for _ in 0..60 {
        if upstream.requests() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(upstream.requests() >= 1, "任务应当已经打到挂起的上游");

    let cancelled: Value = client()
        .post(format!("{}/v1/responses/{id}/cancel", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    assert_eq!(
        cancelled["upstream_connection_aborted"], true,
        "必须真的中断在跑的上游连接：{cancelled}"
    );
    assert!(
        cancelled["cancellation_note"]
            .as_str()
            .unwrap_or_default()
            .contains("可能已经产生费用"),
        "要如实提示可能已计费：{cancelled}"
    );
    assert_eq!(
        akhub.state.runtime.background.len().await,
        0,
        "取消后句柄表必须清空"
    );
}

/// 删除托管任务会先中断在跑的执行，避免删了记录上游还在跑。
#[tokio::test]
async fn deleting_a_managed_task_also_stops_the_execution() {
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(common::Behavior::Hang);
    let akhub = spawn_akhub_with(akhub::app::Settings::default(), |group| {
        group.allow_managed_background = true;
    })
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "gpt-5",
            "glm-4.6",
            50,
        )
        .pinned(),
    )
    .await;

    let created: Value = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "你好", "background": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    for _ in 0..60 {
        if upstream.requests() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let deleted = client()
        .delete(format!("{}/v1/responses/{id}", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 200);
    assert_eq!(akhub.state.runtime.background.len().await, 0);

    // 记录已删：再查就是过期。
    let again: Value = client()
        .get(format!("{}/v1/responses/{id}", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["error"]["code"], "response_state_expired", "{again}");
}

/// 托管任务同样受分组隔离：别的分组查不到它。
#[tokio::test]
async fn a_managed_task_is_not_visible_from_another_group() {
    let (akhub, _upstream, _account) = managed_setup(true).await;
    let created: Value = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "你好", "background": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // 另建一个分组，用它的 Key 去查。
    let (other_key, prefix) = akhub::security::generate_group_key().unwrap();
    let other = akhub::domain::Group {
        id: akhub::storage::store::ids::group(),
        name: "另一组".into(),
        key_prefix: prefix,
        key_digest_hex: akhub.state.key_digest.digest_hex(&other_key),
        multiplier_limit: akhub::domain::Multiplier::ONE,
        weights: Default::default(),
        queue_capacity: 10,
        max_wait_secs: 60,
        allow_degrade: true,
        allow_managed_background: false,
        created_at: time::OffsetDateTime::now_utc(),
    };
    akhub.state.store.insert_group(&other).await.unwrap();
    akhub.state.reload_config().await.unwrap();

    let body: Value = client()
        .get(format!("{}/v1/responses/{id}", akhub.base_url))
        .bearer_auth(other_key.to_string())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["error"]["code"], "response_state_expired", "{body}");
    let _ = Limits::default();
    let _ = MultiplierMode::Manual;
}

/// 任务开始执行时必须进入 running，而不是一直显示 queued（§29.1）。
#[tokio::test]
async fn a_running_task_reports_in_progress_not_queued() {
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(common::Behavior::Hang);
    let akhub = spawn_akhub_with(akhub::app::Settings::default(), |group| {
        group.allow_managed_background = true;
    })
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "gpt-5",
            "glm-4.6",
            50,
        )
        .pinned(),
    )
    .await;

    let created: Value = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "你好", "background": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // 上游已经收到请求时，任务状态必须是 in_progress。
    for _ in 0..60 {
        if upstream.requests() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let body: Value = client()
        .get(format!("{}/v1/responses/{id}", akhub.base_url))
        .bearer_auth(&akhub.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["status"], "in_progress",
        "开始执行后必须显示 in_progress：{body}"
    );
}

/// 上游挂起超过请求总超时后，任务必须以失败收场，不能永远停在 running。
#[tokio::test]
async fn a_hung_upstream_ends_the_task_with_a_timeout_failure() {
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(common::Behavior::Hang);
    let settings = akhub::app::Settings {
        // 用一个很短的超时把等待压到测试能接受的长度。
        request_timeout: Duration::from_millis(600),
        ..akhub::app::Settings::default()
    };
    let akhub = spawn_akhub_with(settings, |group| {
        group.allow_managed_background = true;
    })
    .await;
    wire_target(
        &akhub,
        TargetSpec::new(
            "账号A",
            &upstream.base_url,
            Protocol::OpenAiChat,
            "gpt-5",
            "glm-4.6",
            50,
        )
        .pinned(),
    )
    .await;

    let created: Value = client()
        .post(format!("{}/v1/responses", akhub.base_url))
        .bearer_auth(&akhub.key)
        .json(&json!({"model": "gpt-5", "input": "你好", "background": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let mut latest = created.clone();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        latest = client()
            .get(format!("{}/v1/responses/{id}", akhub.base_url))
            .bearer_auth(&akhub.key)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if latest["status"] == "failed" {
            break;
        }
    }
    assert_eq!(
        latest["status"], "failed",
        "超时后必须是明确失败而不是永远 running：{latest}"
    );
    // 超时可能由网关自己的任务超时触发，也可能由上层转发路径的上游超时先触发；
    // 两种都必须留下"超时"这个可诊断的原因。
    let code = latest["error"]["code"].as_str().unwrap_or_default();
    assert!(
        code.contains("超过") || code.contains("超时"),
        "错误里要说明是超时：{latest}"
    );
}
