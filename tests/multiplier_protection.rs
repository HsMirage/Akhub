//! 倍率不亏本保护：系统性探针故障要按真实调度路径判定（§11.4、§26.4）。
//!
//! 刷新任务每 10 秒一个 tick，账号按各自的抖动与退避错开到期，一批里通常只有
//! 一个账号。这里用真实的 [`refresh::Scheduler`] 与 [`refresh::run_round`]
//! 逐 tick 推进，而不是把全部账号塞进同一批。

mod common;

use std::time::Duration;

use akhub::domain::{Multiplier, MultiplierMode, Protocol};
use akhub::multiplier::{Status, refresh};
use common::{FakeUpstream, TargetSpec, spawn_akhub, wire_target};
use serde_json::json;
use tokio::time::Instant;

const CHAT: Protocol = Protocol::OpenAiChat;
const MODEL: &str = "glm-4.6";
const INTERVAL: Duration = Duration::from_secs(300);
const TICK: Duration = Duration::from_secs(10);

fn billing(multiplier: f64) -> serde_json::Value {
    json!({
        "object": "billing",
        "version": 1,
        "scope": "key",
        "effective_multiplier": multiplier,
        "observed_at": 1_700_000_000,
    })
}

#[tokio::test]
async fn a_systemic_failure_is_detected_and_held_across_staggered_ticks() {
    let broken = FakeUpstream::spawn().await;
    let healthy = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    // 三个账号挂在探针会坏掉的站点上，一个挂在始终正常的站点上。
    // 0.75 / 1 落在 60%–90%：正常只有 15 分钟宽限。
    for (name, upstream) in [
        ("A", &broken),
        ("B", &broken),
        ("C", &broken),
        ("D", &healthy),
    ] {
        wire_target(
            &akhub,
            TargetSpec::new(name, &upstream.base_url, CHAT, MODEL, MODEL, 50)
                .multiplier(MultiplierMode::Sub2Api, "1"),
        )
        .await;
    }
    broken.set_billing(Some(billing(0.75)));
    healthy.set_billing(Some(billing(0.75)));

    let accounts = akhub.state.store.list_accounts().await.unwrap();
    let context = refresh::Context {
        store: akhub.state.store.clone(),
        cipher: akhub.state.cipher.clone(),
        upstream: akhub.state.upstream.clone(),
        registry: akhub.state.runtime.multipliers.clone(),
    };
    let registry = &akhub.state.runtime.multipliers;
    let mut scheduler = refresh::Scheduler::new(INTERVAL);
    let start = Instant::now();
    let start_unix = akhub::storage::now_unix();

    // 与 `app::tasks` 的刷新循环同一套步骤：取到期账号 → 刷新 → 记录结果。
    let mut batches = Vec::new();
    let mut protected_since = None;
    let mut offset = Duration::ZERO;
    while offset <= Duration::from_secs(1200) {
        let due: Vec<_> = scheduler
            .due(&accounts, start + offset)
            .into_iter()
            .cloned()
            .collect();
        let now_unix = start_unix + offset.as_secs() as i64;
        if offset.is_zero() {
            // 启动后第一个 tick 就刷新全部自动账号，不等一个间隔。
            assert_eq!(due.len(), accounts.len(), "首轮必须立即刷新全部自动账号");
        }
        if !due.is_empty() {
            let stats =
                refresh::run_round(&context, &due.iter().collect::<Vec<_>>(), now_unix).await;
            for (id, ok) in &stats.outcomes {
                scheduler.record(id, *ok, start + offset);
            }
            if offset.is_zero() {
                assert_eq!(stats.failed, 0);
                // 探针侧故障：broken 站点的计费接口从此全部失败。
                broken.set_billing(None);
            } else {
                batches.push(due.len());
            }
        }
        // 一旦过半失败，之后任何一个批次（包括 D 单独成功的批次）都不能解除。
        if registry.view().systemic_failure(now_unix) {
            protected_since.get_or_insert(offset);
        } else {
            assert!(
                protected_since.is_none(),
                "保护在 {offset:?} 被单个批次误解除"
            );
        }
        offset += TICK;
    }
    assert!(
        protected_since.is_some(),
        "错开的逐个失败也必须判定为系统性故障"
    );

    assert!(
        batches.iter().all(|size| *size < accounts.len()),
        "账号应当错开到期，否则测不到按批次判定的缺陷：{batches:?}"
    );
    let end_unix = start_unix + 1200;
    let view = registry.view();
    assert!(
        view.systemic_failure(end_unix),
        "3/4 账号的最新结果是失败，即便 D 的批次全成功也必须保持保护"
    );
    // 15 分钟宽限早已用完，但系统性故障下统一按 60 分钟算：仍可用。
    let a = accounts.iter().find(|a| a.name == "A").unwrap();
    let effective = view.effective(a, Multiplier::ONE, end_unix);
    assert_eq!(
        effective.status,
        Status::Stale,
        "探针自己坏了不该把网关打死"
    );

    // 探针恢复：A、B 刷新成功后失败比例回落到一半以下，保护解除。
    broken.set_billing(Some(billing(0.75)));
    for name in ["A", "B"] {
        let account = accounts.iter().find(|a| a.name == name).unwrap();
        refresh::run_round(&context, &[account], end_unix).await;
    }
    assert!(!registry.view().systemic_failure(end_unix));
}
