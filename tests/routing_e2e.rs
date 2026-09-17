//! 阶段 2 验收：严格阶梯、层内排队、粘性、熔断范围、倍率门与重启恢复
//! （§26.3、§26.4、§26.7）。
//!
//! 全部通过真实 HTTP 打到可编排的假上游，不 mock 网关内部。

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use akhub::app::{AppState, Settings};
use akhub::domain::{Limits, Multiplier, MultiplierMode, Protocol};
use akhub::health::{Outcome, Unavailable};
use akhub::multiplier::{Status, refresh};
use akhub::routing::score::Dimension;
use akhub::storage::store::MultiplierSnapshotRow;
use common::{
    Akhub, Behavior, FakeUpstream, TargetSpec, bulky_chat_body, chat, chat_at, messages, serve,
    spawn_akhub, spawn_akhub_with, wire_extra_target, wire_target,
};
use serde_json::{Value, json};

const CHAT: Protocol = Protocol::OpenAiChat;
const MODEL: &str = "glm-4.6";

fn one_slot() -> Limits {
    Limits {
        max_concurrency: Some(1),
        ..Limits::default()
    }
}

fn small_body(system: &str, user: &str) -> Value {
    json!({
        "model": MODEL,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    })
}

async fn error_code(response: reqwest::Response) -> String {
    let body: Value = response.json().await.unwrap();
    body["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ------------------------------------------------------------ 严格阶梯与排队

#[tokio::test]
async fn a_busy_top_layer_queues_instead_of_sinking_to_the_next_layer() {
    let top = FakeUpstream::spawn().await;
    let low = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &top.base_url, CHAT, MODEL, MODEL, 100).limits(one_slot()),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("B", &low.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;

    // 占住 A 唯一的并发名额，模拟一个还没结束的慢请求。
    let held = akhub
        .state
        .runtime
        .health
        .try_admit(&a.account_id, &a.target_id, one_slot(), 0)
        .unwrap();

    let waiting = tokio::spawn(chat(&akhub, small_body("s", "u")));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "第 1 层只是忙，请求应当在本层排队");
    assert_eq!(
        low.requests(),
        0,
        "第 1 层还有合格目标时，第 2 层绝不能拿到流量"
    );

    // 名额释放，排队的请求立刻走 A。
    drop(held);
    let response = waiting.await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(top.requests(), 1);
    assert_eq!(low.requests(), 0);
}

#[tokio::test]
async fn queue_full_when_the_group_forbids_waiting() {
    let top = FakeUpstream::spawn().await;
    let akhub = spawn_akhub_with(Settings::default(), |group| group.queue_capacity = 0).await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &top.base_url, CHAT, MODEL, MODEL, 100).limits(one_slot()),
    )
    .await;
    let _held = akhub
        .state
        .runtime
        .health
        .try_admit(&a.account_id, &a.target_id, one_slot(), 0)
        .unwrap();

    let started = Instant::now();
    let response = chat(&akhub, small_body("s", "u")).await;
    assert_eq!(response.status(), 429);
    assert!(response.headers().contains_key("retry-after"));
    assert_eq!(error_code(response).await, "queue_full");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "容量为 0 的分组不该等待，必须立即拒绝"
    );
    assert_eq!(top.requests(), 0);
}

#[tokio::test]
async fn queue_timeout_when_the_layer_stays_busy_past_the_deadline() {
    let top = FakeUpstream::spawn().await;
    let settings = Settings {
        request_timeout: Duration::from_millis(600),
        ..Settings::default()
    };
    let akhub = spawn_akhub_with(settings, |_| {}).await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &top.base_url, CHAT, MODEL, MODEL, 100).limits(one_slot()),
    )
    .await;
    let _held = akhub
        .state
        .runtime
        .health
        .try_admit(&a.account_id, &a.target_id, one_slot(), 0)
        .unwrap();

    let response = chat(&akhub, small_body("s", "u")).await;
    assert_eq!(response.status(), 429);
    assert!(response.headers().contains_key("retry-after"));
    assert_eq!(error_code(response).await, "queue_timeout");
    assert_eq!(top.requests(), 0);
}

/// 分组的"队列最长等待"要比请求总超时更早生效（§6.3）：
/// 用户最多按分组设置等这么久就拿到可重试的 429，而不是干等请求总超时。
#[tokio::test]
async fn the_group_max_wait_caps_queue_waiting_before_the_request_timeout() {
    let top = FakeUpstream::spawn().await;
    let settings = Settings {
        // 请求总超时远大于分组的最长等待。
        request_timeout: Duration::from_secs(30),
        ..Settings::default()
    };
    let akhub = spawn_akhub_with(settings, |group| {
        group.max_wait_secs = 1;
    })
    .await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &top.base_url, CHAT, MODEL, MODEL, 100).limits(one_slot()),
    )
    .await;
    let _held = akhub
        .state
        .runtime
        .health
        .try_admit(&a.account_id, &a.target_id, one_slot(), 0)
        .unwrap();

    let started = std::time::Instant::now();
    let response = chat(&akhub, small_body("s", "u")).await;
    let waited = started.elapsed();

    assert_eq!(response.status(), 429);
    assert_eq!(error_code(response).await, "queue_timeout");
    assert!(
        waited < Duration::from_secs(10),
        "必须按分组的最长等待（1s）退出，而不是等满请求总超时：实际 {waited:?}"
    );
    assert_eq!(top.requests(), 0);
}

#[tokio::test]
async fn a_slow_upstream_is_neither_retried_nor_tripped() {
    let slow = FakeUpstream::spawn().await;
    slow.fallback(Behavior::Hang);
    let backup = FakeUpstream::spawn().await;
    let settings = Settings {
        request_timeout: Duration::from_millis(500),
        ..Settings::default()
    };
    let akhub = spawn_akhub_with(settings, |_| {}).await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &slow.base_url, CHAT, MODEL, MODEL, 100),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("B", &backup.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;

    let response = chat(&akhub, small_body("s", "u")).await;
    assert_eq!(response.status(), 504);
    assert_eq!(error_code(response).await, "upstream_timeout");
    // 总超时后不向第二个目标重放，避免两个上游同时生成（§13.5）。
    assert_eq!(backup.requests(), 0, "慢请求不切换");
    assert_eq!(slow.requests(), 1);
    // 单纯变慢只降评分，不熔断（§12.1）。
    assert!(
        akhub
            .state
            .runtime
            .health
            .check(&a.account_id, &a.target_id, Limits::default())
            .is_ok()
    );
    slow.release();
}

#[tokio::test]
async fn streaming_responses_hold_the_concurrency_slot_until_upstream_finishes() {
    let upstream = FakeUpstream::spawn().await;
    upstream.script([Behavior::StreamThenHang]);
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("stream", &upstream.base_url, CHAT, MODEL, MODEL, 50).limits(one_slot()),
    )
    .await;

    let first = messages(
        &akhub,
        json!({
            "model": MODEL,
            "stream": true,
            "messages": [{"role": "user", "content": "first"}]
        }),
    )
    .await;
    assert_eq!(first.status(), 200);

    let second = tokio::spawn(messages(
        &akhub,
        json!({"model": MODEL, "messages": [{"role": "user", "content": "second"}]}),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let still_waiting = !second.is_finished();
    let requests_before_release = upstream.requests();
    upstream.release();

    let _ = first.text().await;
    assert_eq!(second.await.unwrap().status(), 200);
    assert!(still_waiting, "流式响应未完成前不能释放并发名额");
    assert_eq!(requests_before_release, 1);
    assert_eq!(upstream.requests(), 2);
}

#[tokio::test]
async fn cheap_failures_do_not_count_against_any_attempt_limit() {
    // §26.3：第 1 层 5 个目标，3 个连接失败，第 4 个仍被尝试且成功。
    //
    // "必然拒绝连接"用固定特权端口：早期实现是 bind(:0) 后立刻 drop，赌端口
    // 不会被别人占用；CI 上并行跑用例时这个赌注会输——另一个用例的假上游
    // 正好绑到同一个临时端口，于是"死目标"活了过来，请求落到别人的上游上，
    // 同进程并行跑的 queue_timeout 用例也会被串扰。特权端口非 root 绑不了，
    // 连接必然 ECONNREFUSED，结果稳定。
    let dead: Vec<String> = (1..=3)
        .map(|port| format!("http://127.0.0.1:{port}"))
        .collect();
    let healthy = [FakeUpstream::spawn().await, FakeUpstream::spawn().await];

    let akhub = spawn_akhub().await;
    for (index, url) in dead.iter().enumerate() {
        let name = format!("dead{index}");
        wire_target(&akhub, TargetSpec::new(&name, url, CHAT, MODEL, MODEL, 50)).await;
    }
    for (index, upstream) in healthy.iter().enumerate() {
        let name = format!("ok{index}");
        wire_target(
            &akhub,
            TargetSpec::new(&name, &upstream.base_url, CHAT, MODEL, MODEL, 50),
        )
        .await;
    }

    let response = chat(&akhub, small_body("s", "u")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        healthy[0].requests() + healthy[1].requests(),
        1,
        "恰好一个健康目标承接了请求"
    );
}

// ------------------------------------------------------------------ 粘性

/// 找出粘性绑定到了哪台假上游。
fn bound_side<'a>(
    x: &'a FakeUpstream,
    y: &'a FakeUpstream,
) -> (&'a FakeUpstream, &'a FakeUpstream) {
    if x.requests() > 0 { (x, y) } else { (y, x) }
}

#[tokio::test]
async fn sticky_requests_follow_the_first_target_and_move_when_it_breaks() {
    let x = FakeUpstream::spawn().await;
    let y = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("X", &x.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("Y", &y.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;

    let system = "你是项目 Alpha 的编码助手";
    assert_eq!(
        chat(&akhub, small_body(system, "第一问")).await.status(),
        200
    );
    let (bound, other) = bound_side(&x, &y);

    // 同一 system prompt、不同用户消息：粘性命中不参与抽签（§9.5）。
    for round in 0..6 {
        let body = small_body(system, &format!("第 {round} 轮，时间 {round}:00"));
        assert_eq!(chat(&akhub, body).await.status(), 200);
    }
    assert_eq!(bound.requests(), 7);
    assert_eq!(other.requests(), 0);
    assert_eq!(akhub.state.runtime.sticky.len(), 1);

    // 原目标失败一次：切换并把绑定挪到新目标，之后不再抢回（§10.2）。
    bound.script([Behavior::Status(500, None)]);
    assert_eq!(chat(&akhub, small_body(system, "再问")).await.status(), 200);
    assert_eq!(other.requests(), 1);
    for _ in 0..3 {
        assert_eq!(chat(&akhub, small_body(system, "继续")).await.status(), 200);
    }
    assert_eq!(other.requests(), 4);
    assert_eq!(bound.requests(), 8, "旧目标不再被强制抢回");
}

#[tokio::test]
async fn a_sticky_request_waits_for_a_busy_target_according_to_its_budget() {
    let x = FakeUpstream::spawn().await;
    let y = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let wired_x = wire_target(
        &akhub,
        TargetSpec::new("X", &x.base_url, CHAT, MODEL, MODEL, 50).limits(one_slot()),
    )
    .await;
    let wired_y = wire_target(
        &akhub,
        TargetSpec::new("Y", &y.base_url, CHAT, MODEL, MODEL, 50).limits(one_slot()),
    )
    .await;

    let system = "你是项目 Beta 的编码助手";
    assert_eq!(
        chat(&akhub, bulky_chat_body(system, "首问")).await.status(),
        200
    );
    let (bound, other) = bound_side(&x, &y);
    let bound_wired = if std::ptr::eq(bound, &x) {
        &wired_x
    } else {
        &wired_y
    };

    // 占住绑定目标的唯一名额。30 KB 的请求愿意等 12 秒，绝不立刻换号。
    let held = akhub
        .state
        .runtime
        .health
        .try_admit(
            &bound_wired.account_id,
            &bound_wired.target_id,
            one_slot(),
            0,
        )
        .unwrap();
    let waiting = tokio::spawn(chat(&akhub, bulky_chat_body(system, "大请求")));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!waiting.is_finished(), "粘性请求应当在原目标的队列里等待");
    assert_eq!(other.requests(), 0, "等待期间不能换号");
    drop(held);
    assert_eq!(waiting.await.unwrap().status(), 200);
    assert_eq!(bound.requests(), 2);

    // 小请求的等待预算是 0：重建缓存几乎不花钱，直接换号并重绑（§10.3）。
    let _held = akhub
        .state
        .runtime
        .health
        .try_admit(
            &bound_wired.account_id,
            &bound_wired.target_id,
            one_slot(),
            0,
        )
        .unwrap();
    let started = Instant::now();
    assert_eq!(
        chat(&akhub, small_body(system, "小请求")).await.status(),
        200
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(other.requests(), 1);
    // 绑定已经挪到新目标：原目标空出来之后也不再抢回。
    drop(_held);
    assert_eq!(
        chat(&akhub, small_body(system, "又一个小请求"))
            .await
            .status(),
        200
    );
    assert_eq!(other.requests(), 2);
    assert_eq!(bound.requests(), 2);
}

#[tokio::test]
async fn a_sticky_request_honours_retry_after_within_its_budget() {
    let x = FakeUpstream::spawn().await;
    let y = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("X", &x.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("Y", &y.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;

    let system = "你是项目 Gamma 的编码助手";
    assert_eq!(
        chat(&akhub, bulky_chat_body(system, "首问")).await.status(),
        200
    );
    let (bound, other) = bound_side(&x, &y);

    // Retry-After 1 秒 ≤ 12 秒预算：原地等待并重试同一目标（§10.3）。
    bound.script([Behavior::Status(429, Some(1))]);
    let started = Instant::now();
    assert_eq!(
        chat(&akhub, bulky_chat_body(system, "大请求"))
            .await
            .status(),
        200
    );
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "必须真的等过 Retry-After"
    );
    assert_eq!(bound.requests(), 3, "429 之后应当重试同一目标");
    assert_eq!(other.requests(), 0);

    // 小请求预算为 0，Retry-After 等不起：立即换号。
    bound.script([Behavior::Status(429, Some(1))]);
    let started = Instant::now();
    assert_eq!(
        chat(&akhub, small_body(system, "小请求")).await.status(),
        200
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(bound.requests(), 4);
    assert_eq!(other.requests(), 1);
}

// ------------------------------------------------------------ 流式切换边界

#[tokio::test]
async fn an_error_event_before_content_switches_but_a_delta_does_not() {
    let a = FakeUpstream::spawn().await;
    let b = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let anthropic = Protocol::AnthropicMessages;
    wire_target(
        &akhub,
        TargetSpec::new("A", &a.base_url, anthropic, "claude", "claude-up", 100),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("B", &b.base_url, anthropic, "claude", "claude-up", 50),
    )
    .await;
    let body = json!({"model": "claude", "stream": true, "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]});

    // 只收到开始标记与错误事件：上游还没产生任何成本，可以切换（§13.4）。
    a.script([Behavior::StreamErrorEvent]);
    let response = messages(&akhub, body.clone()).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("content_block_delta"), "{text}");
    assert!(
        !text.contains("overloaded_error"),
        "错误事件不该泄漏给客户端：{text}"
    );
    assert_eq!(a.requests(), 1);
    assert_eq!(b.requests(), 1);

    // 已经送出语义增量后中断：禁止拼接第二个上游，客户端看到的是截断的流。
    a.script([Behavior::StreamThenAbort]);
    let response = messages(&akhub, body).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap_or_default();
    assert!(
        !text.contains("message_stop"),
        "不得伪造正常完成事件：{text}"
    );
    assert_eq!(a.requests(), 2);
    assert_eq!(b.requests(), 1, "第一个语义事件之后不得切换");
}

// ------------------------------------------------------------ 熔断范围

#[tokio::test]
async fn an_invalid_key_pauses_the_whole_account_but_a_429_only_the_model() {
    let up1 = FakeUpstream::spawn().await;
    let up2 = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a1 = wire_target(
        &akhub,
        TargetSpec::new("A1", &up1.base_url, CHAT, "glm-4.6", "glm-4.6", 100),
    )
    .await;
    wire_extra_target(&akhub, &a1.account_id, "glm-4.5", "glm-4.5").await;
    let a2 = wire_target(
        &akhub,
        TargetSpec::new("A2", &up2.base_url, CHAT, "glm-4.6", "glm-4.6", 50),
    )
    .await;
    wire_extra_target(&akhub, &a2.account_id, "glm-4.5", "glm-4.5").await;

    // 401 证明 Key 失效：同一账号下的另一个模型也立即停用（§12.1）。
    up1.script([Behavior::Status(401, None)]);
    assert_eq!(
        chat(&akhub, json!({"model": "glm-4.6", "messages": []}))
            .await
            .status(),
        200
    );
    assert_eq!((up1.requests(), up2.requests()), (1, 1));
    assert_eq!(
        chat(&akhub, json!({"model": "glm-4.5", "messages": []}))
            .await
            .status(),
        200
    );
    assert_eq!(
        (up1.requests(), up2.requests()),
        (1, 2),
        "Key 失效的账号不该再被尝试"
    );
    assert_eq!(
        akhub
            .state
            .runtime
            .health
            .check(&a1.account_id, &a1.target_id, Limits::default()),
        Err(Unavailable::KeyInvalid)
    );

    // 管理员换了凭据：硬停解除。
    akhub
        .state
        .runtime
        .health
        .clear_account_faults(&a1.account_id);

    // 429 只影响"账号 + 模型"：另一个模型照常走原账号（§12.3）。
    up1.script([Behavior::Status(429, Some(30))]);
    assert_eq!(
        chat(&akhub, json!({"model": "glm-4.6", "messages": []}))
            .await
            .status(),
        200
    );
    assert_eq!((up1.requests(), up2.requests()), (2, 3));
    assert_eq!(
        chat(&akhub, json!({"model": "glm-4.5", "messages": []}))
            .await
            .status(),
        200
    );
    assert_eq!((up1.requests(), up2.requests()), (3, 3));
}

#[tokio::test]
async fn a_run_of_faults_trips_the_breaker_and_the_next_request_skips_the_target() {
    let bad = FakeUpstream::spawn().await;
    let good = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &bad.base_url, CHAT, MODEL, MODEL, 100),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("B", &good.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;
    bad.fallback(Behavior::Status(500, None));
    // 没有 system prompt 就没有粘性键：每个请求都独立走阶梯。
    let one_off = json!({"model": MODEL, "messages": [{"role": "user", "content": "u"}]});

    for _ in 0..5 {
        assert_eq!(chat(&akhub, one_off.clone()).await.status(), 200);
    }
    assert_eq!(bad.requests(), 5, "熔断前每次都先试高优先级目标");
    assert_eq!(
        akhub
            .state
            .runtime
            .health
            .check(&a.account_id, &a.target_id, Limits::default()),
        Err(Unavailable::Cooling)
    );
    // 熔断后不再碰它：请求直接落到下一层。
    assert_eq!(chat(&akhub, one_off).await.status(), 200);
    assert_eq!(bad.requests(), 5);
    assert_eq!(good.requests(), 6);
}

// ------------------------------------------------------------ 倍率门

fn snapshot(
    account_id: &str,
    multiplier: &str,
    status: Status,
    stale_since: Option<i64>,
) -> MultiplierSnapshotRow {
    MultiplierSnapshotRow {
        account_id: account_id.into(),
        multiplier: Multiplier::parse(multiplier).unwrap(),
        source: MultiplierMode::Sub2Api,
        status: status.as_str().into(),
        observed_at: None,
        refreshed_at: 0,
        stale_since,
        last_error: None,
    }
}

#[tokio::test]
async fn stale_multipliers_stay_usable_until_the_grace_period_ends() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &upstream.base_url, CHAT, MODEL, MODEL, 50)
            .multiplier(MultiplierMode::Sub2Api, "0.5"),
    )
    .await;
    let now = akhub::storage::now_unix();

    // 还没刷新过：用手填值顶着，可用但已在宽限期内。
    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 200);

    // 过期 30 分钟、余量充足（0.5 / 1）：仍在 60 分钟宽限期内。
    akhub
        .state
        .store
        .upsert_multiplier_snapshot(&snapshot(
            &a.account_id,
            "0.5",
            Status::Stale,
            Some(now - 30 * 60),
        ))
        .await
        .unwrap();
    akhub.state.reseed_multipliers().await.unwrap();
    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 200);

    // 过期 2 小时：宽限期结束，硬停，且不可重试。
    akhub
        .state
        .store
        .upsert_multiplier_snapshot(&snapshot(
            &a.account_id,
            "0.5",
            Status::Stale,
            Some(now - 2 * 3600),
        ))
        .await
        .unwrap();
    akhub.state.reseed_multipliers().await.unwrap();
    let response = chat(&akhub, small_body("s", "u")).await;
    assert_eq!(response.status(), 403);
    assert!(!response.headers().contains_key("retry-after"));
    assert_eq!(error_code(response).await, "multiplier_unknown");

    // 余量不足 10%（0.95 / 1）：刷新一失败就立即硬停。
    akhub
        .state
        .store
        .upsert_multiplier_snapshot(&snapshot(
            &a.account_id,
            "0.95",
            Status::Stale,
            Some(now - 1),
        ))
        .await
        .unwrap();
    akhub.state.reseed_multipliers().await.unwrap();
    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 403);

    // 刷新恢复：自动恢复调度，不需要手动启用（§11.4）。
    akhub
        .state
        .store
        .upsert_multiplier_snapshot(&snapshot(&a.account_id, "0.5", Status::Known, None))
        .await
        .unwrap();
    akhub.state.reseed_multipliers().await.unwrap();
    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 200);
    assert_eq!(upstream.requests(), 3);
}

fn refresh_context(akhub: &Akhub) -> refresh::Context {
    refresh::Context {
        store: akhub.state.store.clone(),
        cipher: akhub.state.cipher.clone(),
        upstream: akhub.state.upstream.clone(),
        registry: Arc::clone(&akhub.state.runtime.multipliers),
    }
}

fn billing(multiplier: f64) -> Value {
    json!({
        "object": "billing",
        "version": 1,
        "scope": "key",
        "effective_multiplier": multiplier,
        "observed_at": 1_700_000_000,
    })
}

#[tokio::test]
async fn sub2api_probe_results_drive_the_multiplier_gate() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let a = wire_target(
        &akhub,
        TargetSpec::new("A", &upstream.base_url, CHAT, MODEL, MODEL, 50)
            .multiplier(MultiplierMode::Sub2Api, "1"),
    )
    .await;
    let account = akhub.state.store.list_accounts().await.unwrap().remove(0);
    let context = refresh_context(&akhub);
    let now = akhub::storage::now_unix();

    upstream.set_billing(Some(billing(0.5)));
    let stats = refresh::run_round(&context, &[&account], now).await;
    assert_eq!((stats.attempted, stats.failed), (1, 0));
    let effective =
        akhub
            .state
            .runtime
            .multipliers
            .view()
            .effective(&account, Multiplier::ONE, now);
    assert_eq!(effective.status, Status::Known);
    assert_eq!(effective.value, Multiplier::parse("0.5").unwrap());
    let probe = upstream.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(probe.path, "/v1/sub2api/billing");
    assert_eq!(
        probe.headers["authorization"],
        format!("Bearer {}", a.api_key)
    );

    // 上游改价到 1.5，超过分组上限 1：硬停，且不可重试（§11.5）。
    upstream.set_billing(Some(billing(1.5)));
    refresh::run_round(&context, &[&account], now).await;
    let response = chat(&akhub, small_body("s", "u")).await;
    assert_eq!(response.status(), 403);
    assert_eq!(error_code(response).await, "multiplier_exceeded");

    // 价格回落：自动恢复。
    upstream.set_billing(Some(billing(0.5)));
    refresh::run_round(&context, &[&account], now).await;
    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 200);

    // 探针失败：保留最后已知值并进入宽限期，请求不中断（§11.4）。
    upstream.set_billing(None);
    let stats = refresh::run_round(&context, &[&account], now).await;
    assert_eq!(stats.failed, 1);
    let effective =
        akhub
            .state
            .runtime
            .multipliers
            .view()
            .effective(&account, Multiplier::ONE, now);
    assert_eq!(effective.status, Status::Stale);
    assert_eq!(effective.value, Multiplier::parse("0.5").unwrap());
    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 200);

    // 状态已持久化，重启后从这里继续。
    let rows = akhub.state.store.list_multiplier_snapshots().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "multiplier_stale");
    assert_eq!(rows[0].multiplier, Multiplier::parse("0.5").unwrap());

    // 格式错误的响应同样按失败处理，绝不猜一个默认值。
    upstream.set_billing(Some(
        json!({"object": "billing", "version": 1, "scope": "site", "effective_multiplier": 0.1}),
    ));
    let stats = refresh::run_round(&context, &[&account], now).await;
    assert_eq!(stats.failed, 1, "站点级计费范围回答不了这把 Key 的倍率");
}

#[tokio::test]
async fn new_api_probe_uses_the_groups_endpoint_with_both_credentials() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("vip", &upstream.base_url, CHAT, MODEL, MODEL, 50)
            .multiplier(MultiplierMode::NewApi, "1")
            .new_api("tok-123", "42", Some("vip")),
    )
    .await;
    wire_target(
        &akhub,
        TargetSpec::new("unknown-group", &upstream.base_url, CHAT, MODEL, MODEL, 50)
            .multiplier(MultiplierMode::NewApi, "1")
            .new_api("tok-456", "43", None),
    )
    .await;
    let accounts = akhub.state.store.list_accounts().await.unwrap();
    let context = refresh_context(&akhub);
    let now = akhub::storage::now_unix();

    upstream.set_groups(Some(json!({
        "success": true,
        "data": {
            "default": {"ratio": 1, "desc": "默认"},
            "vip": {"ratio": 0.3, "desc": "VIP"},
        },
    })));
    let stats = refresh::run_round(&context, &accounts.iter().collect::<Vec<_>>(), now).await;
    assert_eq!(stats.failed, 0);

    let view = akhub.state.runtime.multipliers.view();
    assert_eq!(
        view.effective(&accounts[0], Multiplier::ONE, now).value,
        Multiplier::parse("0.3").unwrap()
    );
    // 不知道自己在哪一档时取最高倍率：把成本估高才是安全方向。
    assert_eq!(
        view.effective(&accounts[1], Multiplier::ONE, now).value,
        Multiplier::ONE
    );

    let probes: Vec<_> = upstream
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|seen| seen.path == "/api/user/self/groups")
        .cloned()
        .collect();
    assert_eq!(probes.len(), 2);
    let vip = probes
        .iter()
        .find(|p| p.headers["new-api-user"] == "42")
        .unwrap();
    // 访问令牌直接放在 Authorization 里，不是推理用的 sk-xxx（§11.2）。
    assert_eq!(vip.headers["authorization"], "tok-123");

    // 令牌过期：探针失败，进入宽限期，错误原因可见但不含凭据。
    upstream.set_groups(Some(json!({"success": false, "message": "unauthorized"})));
    let stats = refresh::run_round(&context, &accounts.iter().collect::<Vec<_>>(), now).await;
    assert_eq!(stats.failed, 2);
    let view = akhub.state.runtime.multipliers.view();
    let (_, entry) = view
        .entries()
        .find(|(id, _)| **id == accounts[0].id)
        .unwrap();
    let error = entry.last_error.clone().unwrap();
    assert!(error.contains("unauthorized"), "{error}");
    assert!(!error.contains("tok-123"));
}

#[tokio::test]
async fn a_systemic_probe_failure_extends_every_grace_period() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    // 两个薄余量账号：正常情况下刷新一失败就立即硬停。
    for name in ["A", "B"] {
        wire_target(
            &akhub,
            TargetSpec::new(name, &upstream.base_url, CHAT, MODEL, MODEL, 50)
                .multiplier(MultiplierMode::Sub2Api, "0.95"),
        )
        .await;
    }
    let accounts = akhub.state.store.list_accounts().await.unwrap();
    let context = refresh_context(&akhub);
    let now = akhub::storage::now_unix();

    upstream.set_billing(Some(billing(0.95)));
    refresh::run_round(&context, &accounts.iter().collect::<Vec<_>>(), now).await;
    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 200);

    // 两个账号同一轮全部失败：判定为探针侧故障，宽限期统一延长到 60 分钟。
    upstream.set_billing(None);
    let stats = refresh::run_round(&context, &accounts.iter().collect::<Vec<_>>(), now).await;
    assert_eq!(stats.failed, 2);
    let view = akhub.state.runtime.multipliers.view();
    assert!(view.systemic_failure(now));
    assert_eq!(
        view.effective(&accounts[0], Multiplier::ONE, now + 30 * 60)
            .status,
        Status::Stale,
        "探针自己坏了不该把整个网关打死"
    );
    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 200);
}

// ------------------------------------------------------------ 重启恢复

#[tokio::test]
async fn sticky_bindings_and_perf_snapshots_survive_a_restart() {
    let x = FakeUpstream::spawn().await;
    let y = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let wired_x = wire_target(
        &akhub,
        TargetSpec::new("X", &x.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;
    let wired_y = wire_target(
        &akhub,
        TargetSpec::new("Y", &y.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;

    let system = "你是项目 Delta 的编码助手";
    for round in 0..25 {
        let body = small_body(system, &format!("第 {round} 轮"));
        assert_eq!(chat(&akhub, body).await.status(), 200);
    }
    let (bound, other) = bound_side(&x, &y);
    assert_eq!(bound.requests(), 25);
    let bound_target = if std::ptr::eq(bound, &x) {
        &wired_x.target_id
    } else {
        &wired_y.target_id
    };
    let dimension = Dimension {
        protocol: CHAT,
        streaming: false,
    };
    assert!(
        akhub
            .state
            .runtime
            .perf
            .stats(bound_target, dimension)
            .is_warm()
    );

    // 60 秒快照任务的工作在关闭前也会做一次。
    akhub::app::tasks::flush_snapshots(&akhub.state).await;

    // "重启"：同一数据目录上再起一个进程。
    let restarted = AppState::bootstrap(akhub.data_dir(), Settings::default())
        .await
        .unwrap();
    assert_eq!(restarted.runtime.sticky.len(), 1, "粘性绑定重启后恢复");
    let stats = restarted.runtime.perf.stats(bound_target, dimension);
    assert!(stats.is_warm(), "评分从快照继续，不从零开始");
    assert_eq!(stats.samples, 25);

    // 同一前缀仍打到原目标（§26.7）。
    let base_url = serve(Arc::clone(&restarted)).await;
    assert_eq!(
        chat_at(&base_url, &akhub.key, small_body(system, "重启后"))
            .await
            .status(),
        200
    );
    assert_eq!(bound.requests(), 26);
    assert_eq!(other.requests(), 0);

    // 超过 24 小时的性能快照与超过 1 小时的粘性绑定都被丢弃。
    sqlx::query("UPDATE target_perf_snapshot SET updated_at = 0")
        .execute(akhub.state.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE sticky_bindings SET last_used_at = 0")
        .execute(akhub.state.store.pool())
        .await
        .unwrap();
    let stale = AppState::bootstrap(akhub.data_dir(), Settings::default())
        .await
        .unwrap();
    assert!(!stale.runtime.perf.stats(bound_target, dimension).is_warm());
    assert_eq!(stale.runtime.sticky.len(), 0);
}

#[tokio::test]
async fn request_records_capture_scheduling_telemetry() {
    let x = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        TargetSpec::new("X", &x.base_url, CHAT, MODEL, MODEL, 50)
            .multiplier(MultiplierMode::Manual, "0.5"),
    )
    .await;

    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 200);
    assert_eq!(chat(&akhub, small_body("s", "u")).await.status(), 200);

    let mut records = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        records = akhub.state.store.list_request_records(10, 0).await.unwrap();
        if records.len() == 2 {
            break;
        }
    }
    assert_eq!(records.len(), 2);
    // 列表按时间倒序；同一秒内按写入顺序倒序，后完成的在前。
    let latest = &records[0];
    assert_eq!(latest.attempts, 1);
    assert!(latest.sticky_hit, "第二个请求应当命中粘性");
    assert!(!records[1].sticky_hit, "第一个请求是首次绑定，不算命中");
    assert_eq!(
        latest.effective_multiplier,
        Some(Multiplier::parse("0.5").unwrap())
    );
    // 成本反事实基准只能在请求发生时记录（§11.6）。
    assert_eq!(
        latest.cheapest_multiplier,
        Some(Multiplier::parse("0.5").unwrap())
    );
    assert_eq!(
        latest.dearest_multiplier,
        Some(Multiplier::parse("0.5").unwrap())
    );
}

#[tokio::test]
async fn admissions_settle_correctly_through_the_public_api() {
    // 健康状态机的公开接口在网关之外也要能独立使用（后台"测试"按钮会用到）。
    let akhub = spawn_akhub().await;
    let admission = akhub
        .state
        .runtime
        .health
        .try_admit("acc", "tgt", Limits::default(), 0)
        .unwrap();
    admission.settle(Outcome::Success, Some(10));
    assert!(
        akhub
            .state
            .runtime
            .health
            .check("acc", "tgt", Limits::default())
            .is_ok()
    );
}
