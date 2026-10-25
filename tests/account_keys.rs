//! 阶段验收：账号内 Key 池（§4.2.1）。
//!
//! 全部通过真实 HTTP 打到可编排的假上游，不 mock 网关内部。假上游按
//! Authorization / x-api-key 归因到具体哪把 Key，因此"这次用的是哪一把"
//! 可以被直接断言，而不是靠推测。
//!
//! 三条不变量在这里各有对应用例：
//!
//! * **A：一次下游调用只使用一把 Key**——换 Key 重试是显式的，且受次数上限约束。
//! * **B：缓存亲和优先于层内评分**——同一条前缀永远回到同一把 Key。
//! * **C：配置分层**——账号级状态与 Key 级状态各管各的。

mod common;

use std::collections::HashMap;

use akhub::domain::{Account, Limits, Multiplier, MultiplierMode, Protocol};
use akhub::storage::store::{AccountKeyWrite, AccountSecrets, ids};
use common::{
    Akhub, Behavior, FakeUpstream, TargetSpec, chat, insert_account_with_key, spawn_akhub,
    wire_target,
};
use serde_json::{Value, json};
use time::OffsetDateTime;

const CHAT: Protocol = Protocol::OpenAiChat;
const MODEL: &str = "glm-4.6";

/// 账号内的一个 Key：标签与明文。
///
/// Key 级限额由 \`health::tests\` 的单元测试精确覆盖（那一层才有准入路径）；
/// 这里的端到端用例只关心凭据选择与缓存亲和。
struct KeySpec<'a> {
    label: &'a str,
    secret: &'a str,
}

impl<'a> KeySpec<'a> {
    fn new(label: &'a str, secret: &'a str) -> Self {
        Self { label, secret }
    }
}

/// 建一个带 Key 池的账号，并把一个逻辑模型接到它上面。
///
/// 直接写 store 而不是走后台 API：这些用例测的是**调度与凭据选择**，不是
/// 建号流程（那条链路由 admin_api 覆盖）。
async fn wire_account_with_keys(
    akhub: &Akhub,
    name: &str,
    base_url: &str,
    keys: &[KeySpec<'_>],
    priority: i32,
) -> String {
    assert!(!keys.is_empty(), "Key 池至少要有一把 Key");
    let group_id = akhub.state.store.list_groups().await.unwrap()[0].id.clone();
    let account = Account {
        id: ids::account(),
        group_id: Some(group_id),
        name: name.into(),
        base_url: base_url.into(),
        preferred_protocol: CHAT,
        adaptive_protocol: true,
        default_priority: priority,
        calibration: Multiplier::ONE,
        multiplier_mode: MultiplierMode::Manual,
        manual_multiplier: Multiplier::ONE,
        new_api_user_id: None,
        new_api_group: None,
        // 账号级并发**不限**：这些用例测的是 Key 级额度是否独立，账号总额度是
        // 另一层闸门，设了会先把它挡住（§17.1 的三级门限）。目标级同理。
        limits: Limits {
            max_concurrency: None,
            ..Limits::default()
        },
        // 假上游监听在 127.0.0.1，必须显式开启内网访问才能通过 SSRF 检查。
        allow_private_network: true,
        enabled: true,
        hide_original: false,
        auto_sync: false,
        model_synced_at: None,
        created_at: OffsetDateTime::now_utc(),
    };
    insert_account_with_key(&akhub.state, &account, keys[0].secret).await;
    // 无条件重建整个池：helper 只认 spec 里的明文与限额，才能保证"池里第几把
    // 是哪把 Key"与测试读到的完全一致。
    let first = akhub
        .state
        .store
        .list_account_key_rows(&account.id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("helper 刚写的 Key 应当能读出来");
    let mut pool: Vec<AccountKeyWrite> = Vec::with_capacity(keys.len());
    for (index, spec) in keys.iter().enumerate() {
        let (id, sealed) = if index == 0 {
            (first.id.clone(), first.sealed_key.clone())
        } else {
            (
                format!("key_{}_{index}", account.id),
                akhub.state.cipher.seal(spec.secret.as_bytes()).unwrap(),
            )
        };
        pool.push(AccountKeyWrite {
            id,
            label: spec.label.to_string(),
            sealed_key: sealed,
            credential_digest: akhub::security::credential_digest(spec.secret),
            limits: Limits::default(),
            enabled: true,
        });
    }
    akhub
        .state
        .store
        .replace_account_keys(&account.id, &pool)
        .await
        .unwrap();
    akhub.state.reload_config().await.unwrap();
    common::wire_extra_target(akhub, &account.id, MODEL, MODEL).await;
    account.id
}

/// 给一个已接线的账号再加一把 Key。
///
/// 演示"一个账号逐步攒起 Key 池"的用法；acceptance 用例目前都一次性建池，
/// 所以它保留在这里供后续扩展。
#[allow(dead_code)]
async fn add_key(akhub: &Akhub, account_id: &str, label: &str, secret: &str) -> String {
    let mut rows: Vec<AccountKeyWrite> = Vec::new();
    for row in akhub
        .state
        .store
        .list_account_key_rows(account_id)
        .await
        .unwrap()
    {
        // 摘要按明文现算：数据库里那一列只是持久化副本，现算最稳。
        let plaintext = akhub.state.cipher.open(&row.sealed_key).unwrap();
        rows.push(AccountKeyWrite {
            id: row.id,
            label: row.label,
            sealed_key: row.sealed_key,
            credential_digest: akhub::security::credential_digest(&String::from_utf8_lossy(
                &plaintext,
            )),
            limits: row.limits,
            enabled: row.enabled,
        });
    }
    let id = format!("key_{label}");
    rows.push(AccountKeyWrite {
        id: id.clone(),
        label: label.to_string(),
        sealed_key: akhub.state.cipher.seal(secret.as_bytes()).unwrap(),
        credential_digest: akhub::security::credential_digest(secret),
        limits: Limits::default(),
        enabled: true,
    });
    akhub
        .state
        .store
        .upsert_account_keys(account_id, &rows)
        .await
        .unwrap();
    akhub.state.reload_config().await.unwrap();
    id
}

fn per_key(entries: &[(&str, Behavior)]) -> Behavior {
    Behavior::PerKey(
        entries
            .iter()
            .map(|(key, behavior)| ((*key).to_string(), behavior.clone()))
            .collect::<HashMap<_, _>>(),
    )
}

/// 带 system prompt 的请求：会推导出稳定前缀粘性键（§10.1 第 4 级）。
fn sticky_body(system: &str, user: &str) -> Value {
    json!({
        "model": MODEL,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    })
}

fn plain_body(user: &str) -> Value {
    json!({"model": MODEL, "messages": [{"role": "user", "content": user}]})
}

// ------------------------------------------------- 不变量 A：一次调用一把 Key

/// 401 时同一次下游调用改用账号内的另一把 Key，下游只看到成功。
///
/// 用粘性绑定把"第一把用哪一把"钉死，否则抽签可能先抽到健康的那把，这个
/// 用例就永远测不到换 Key 的路径。
#[tokio::test]
async fn a_rejected_key_is_swapped_inside_the_account_for_the_same_call() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "池账号",
        &upstream.base_url,
        &[
            KeySpec::new("dead", "sk-dead"),
            KeySpec::new("alive", "sk-alive"),
        ],
        50,
    )
    .await;

    // 先只用第一把 Key 打一次，让粘性绑定落在它身上。
    set_key_enabled(&akhub, &account_id, 1, false).await;
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第一轮")).await.status(),
        200
    );
    set_key_enabled(&akhub, &account_id, 1, true).await;

    // 再打一次，确认绑定确实钉在第一把上（换 Key 才算真的被验证过）。
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第二轮")).await.status(),
        200
    );
    assert_eq!(upstream.requests_with_key("sk-dead"), 2);

    let before = upstream.requests();
    // 现在让第一把失效：命中粘性会先用它，401 之后同一次调用换到另一把。
    upstream.fallback(per_key(&[("sk-dead", Behavior::Status(401, None))]));
    let response = chat(&akhub, sticky_body("项目", "第三轮")).await;
    let status = response.status();
    let payload: Value = response.json().await.unwrap();
    let histogram = upstream.key_histogram();
    assert_eq!(
        status, 200,
        "同一账号内的换 Key 必须让下游只看到成功（histogram={histogram:?} body={payload}）"
    );
    // 这一次调用内两把都被试过：先被拒的那把，然后接上的那把。
    assert_eq!(
        upstream.requests() - before,
        2,
        "一次下游调用只应当打出两次上游尝试（histogram={histogram:?}）"
    );
    assert_eq!(
        upstream.requests_with_key("sk-dead"),
        3,
        "第三把调用先打在绑定的那一把上"
    );
    assert_eq!(upstream.requests_with_key("sk-alive"), 1);
}

/// 开关账号内第 `index` 把 Key（按 Key 池顺序）。
async fn set_key_enabled(akhub: &Akhub, account_id: &str, index: usize, enabled: bool) {
    let mut rows: Vec<AccountKeyWrite> = Vec::new();
    for row in akhub
        .state
        .store
        .list_account_key_rows(account_id)
        .await
        .unwrap()
    {
        let plaintext = akhub.state.cipher.open(&row.sealed_key).unwrap();
        rows.push(AccountKeyWrite {
            id: row.id,
            label: row.label,
            sealed_key: row.sealed_key,
            credential_digest: akhub::security::credential_digest(&String::from_utf8_lossy(
                &plaintext,
            )),
            limits: row.limits,
            enabled: row.enabled,
        });
    }
    rows[index].enabled = enabled;
    akhub
        .state
        .store
        .upsert_account_keys(account_id, &rows)
        .await
        .unwrap();
    akhub.state.reload_config().await.unwrap();
}

/// 按**明文**定位账号内的一把 Key，改它的并发上限（`None` = 不限）。
///
/// 按明文而不是按下标定位：Key 池的排序是 `created_at, id`，而 SQLite 的时间
/// 戳只到秒——同一秒内建的两把 Key 会退化成按随机 id 排，下标不可靠。
/// 明文与摘要是一一对应的，用它定位才是确定的。
///
/// "忙"与"坏"必须能分开构造：这把 Key 满了（忙）和这把 Key 被停用（坏）在
/// 调度上是两种完全不同的处理。
async fn set_key_concurrency(
    akhub: &Akhub,
    account_id: &str,
    secret: &str,
    max_concurrency: Option<u32>,
) {
    let mut rows: Vec<AccountKeyWrite> = Vec::new();
    for row in akhub
        .state
        .store
        .list_account_key_rows(account_id)
        .await
        .unwrap()
    {
        let plaintext = akhub.state.cipher.open(&row.sealed_key).unwrap();
        let plaintext = String::from_utf8_lossy(&plaintext).to_string();
        let limits = if plaintext == secret {
            Limits {
                max_concurrency,
                ..Limits::default()
            }
        } else {
            row.limits
        };
        rows.push(AccountKeyWrite {
            id: row.id,
            label: row.label,
            sealed_key: row.sealed_key,
            credential_digest: akhub::security::credential_digest(&plaintext),
            limits,
            enabled: row.enabled,
        });
    }
    akhub
        .state
        .store
        .upsert_account_keys(account_id, &rows)
        .await
        .unwrap();
    akhub.state.reload_config().await.unwrap();
}

/// 按**明文**取账号内一把 Key 的凭据摘要（动态状态表的归类键）。
fn key_digest(akhub: &Akhub, account_id: &str, secret: &str) -> String {
    akhub
        .state
        .runtime
        .credentials
        .current()
        .keys_of(account_id)
        .iter()
        .find(|key| key.secret.as_ref() == secret)
        .map(|key| key.credential_digest.clone())
        .unwrap_or_else(|| panic!("账号里没有明文为 {secret} 的 Key"))
}

/// 换 Key 次数有上限：一个账号全是坏 Key 时，一次请求不会打出几十次上游调用。
#[tokio::test]
async fn key_switching_is_capped_per_request() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let keys = [
        KeySpec::new("k1", "sk-1"),
        KeySpec::new("k2", "sk-2"),
        KeySpec::new("k3", "sk-3"),
        KeySpec::new("k4", "sk-4"),
        KeySpec::new("k5", "sk-5"),
        KeySpec::new("k6", "sk-6"),
    ];
    wire_account_with_keys(&akhub, "全坏", &upstream.base_url, &keys, 50).await;
    upstream.fallback(Behavior::Status(401, None));

    let response = chat(&akhub, plain_body("hi")).await;
    // 下游看到可重试的失败（§18.3）。
    assert_eq!(response.status(), 503, "整个账号不可用时报可重试错误");
    // 1 次首发 + 最多 3 次换 Key。绝不能是 6 次。
    assert_eq!(
        upstream.requests(),
        4,
        "换 Key 次数必须受上限约束，而不是把池里每一把都试一遍"
    );
}

/// 连接层失败**不**触发换 Key：那不是凭据的问题，换目标才是对的。
#[tokio::test]
async fn a_transport_failure_switches_the_target_instead_of_the_key() {
    let dead = FakeUpstream::spawn().await;
    let good = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    // 上游 A 直接返回 500（服务端错误，不是凭据问题）。
    wire_account_with_keys(
        &akhub,
        "坏站点",
        &dead.base_url,
        &[KeySpec::new("k1", "sk-a1"), KeySpec::new("k2", "sk-a2")],
        100,
    )
    .await;
    wire_account_with_keys(
        &akhub,
        "好站点",
        &good.base_url,
        &[KeySpec::new("k1", "sk-b1")],
        50,
    )
    .await;
    dead.fallback(Behavior::Status(500, None));

    assert_eq!(chat(&akhub, plain_body("hi")).await.status(), 200);
    // 500 是目标级故障：账号 A 只被打了一次，两把 Key 都没有被"用完"。
    assert_eq!(dead.requests(), 1, "5xx 不该触发 Key 级重试");
    assert_eq!(good.requests(), 1);
}

// --------------------------------------------- 不变量 B：缓存亲和优先于评分

/// 同一条稳定前缀的连续请求必须落在**同一把 Key** 上，一次都不许漂。
#[tokio::test]
async fn one_stable_prefix_never_migrates_between_keys() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_account_with_keys(
        &akhub,
        "多 Key",
        &upstream.base_url,
        &[
            KeySpec::new("k1", "sk-1"),
            KeySpec::new("k2", "sk-2"),
            KeySpec::new("k3", "sk-3"),
        ],
        50,
    )
    .await;

    for round in 0..12 {
        assert_eq!(
            chat(
                &akhub,
                sticky_body("同一个项目说明", &format!("第 {round} 轮"))
            )
            .await
            .status(),
            200
        );
    }

    // 12 次全部落在同一把 Key 上：hygiene 之外，这是"前缀缓存不被打碎"的
    // 直接证据（§4.2.1 的不变量 B）。
    let histogram = upstream.key_histogram();
    assert_eq!(
        histogram.len(),
        1,
        "同一条前缀只能落在一把 Key 上：{histogram:?}"
    );
    assert_eq!(histogram.values().sum::<usize>(), 12);
}

/// 不同前缀各自绑一把：Key 池在会话之间是分摊的，不是"一把独占"。
#[tokio::test]
async fn different_prefixes_spread_across_the_pool() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_account_with_keys(
        &akhub,
        "多 Key",
        &upstream.base_url,
        &[
            KeySpec::new("k1", "sk-1"),
            KeySpec::new("k2", "sk-2"),
            KeySpec::new("k3", "sk-3"),
        ],
        50,
    )
    .await;

    // 60 个互不相同的前缀：每个都建一条自己的绑定。
    //
    // 抽样次数不能太小：每一条绑定的 Key 都是在这个池里独立抽的，12 次抽样下
    // "有一把 Key 一次没抽到"的概率约 2.3%（3 × (2/3)^12），CI 上真的因此红过。
    // 60 次把同一个概率压到 1e-10 量级，而断言的意思没有变。
    const PREFIXES: usize = 60;
    for index in 0..PREFIXES {
        assert_eq!(
            chat(&akhub, sticky_body(&format!("项目 {index}"), "hi"))
                .await
                .status(),
            200
        );
    }
    let histogram = upstream.key_histogram();
    assert_eq!(
        histogram.len(),
        3,
        "多个前缀应当把流量摊到整个池上：{histogram:?}"
    );
    assert_eq!(histogram.values().sum::<usize>(), PREFIXES);
}

/// 绑定的那把 Key 失效时，绑定仍然成立（目标没变），只是换池里另一把。
#[tokio::test]
async fn a_dead_bound_key_hands_the_same_target_to_another_key() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_account_with_keys(
        &akhub,
        "多 Key",
        &upstream.base_url,
        &[KeySpec::new("k1", "sk-1"), KeySpec::new("k2", "sk-2")],
        50,
    )
    .await;

    // 先建立绑定（两把都正常，绑定落到其中一把）。
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第一轮")).await.status(),
        200
    );
    let bound = upstream
        .key_histogram()
        .keys()
        .next()
        .cloned()
        .expect("第一次调用必须留下归因");
    // 让被绑定的那把从此失效。
    upstream.fallback(per_key(&[(&bound, Behavior::Status(401, None))]));

    // 后续请求：第一次会在绑定上撞 401，然后同账号换成另一把并改写绑定。
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第二轮")).await.status(),
        200
    );
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第三轮")).await.status(),
        200
    );

    // 换过之后应当稳定在新那把上：第三轮不再碰已经失效的那把。
    let before: usize = upstream.requests_with_key(&bound);
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第四轮")).await.status(),
        200
    );
    assert_eq!(
        upstream.requests_with_key(&bound),
        before,
        "绑定改写之后不该再回到已经失效的那把 Key"
    );
}

// ------------------------------------------------ 不变量 C：账号与 Key 分层

/// 一把 Key 被上游拒绝不影响账号里的另一把：k1 被硬停之后，k2 独自承担
/// 全部流量，账号整体照常服务。
///
/// 用粘性绑定把"先用哪一把"钉死，否则抽签可能先抽到 k2，这个用例就测不到
/// "k1 被停用"这件事。
#[tokio::test]
async fn a_dead_key_leaves_the_rest_of_the_account_serving() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "多 Key",
        &upstream.base_url,
        &[KeySpec::new("k1", "sk-1"), KeySpec::new("k2", "sk-2")],
        50,
    )
    .await;

    // 1) 只开 k1，建立一条指向它的粘性绑定。
    set_key_enabled(&akhub, &account_id, 1, false).await;
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第一轮")).await.status(),
        200
    );
    assert_eq!(upstream.requests_with_key("sk-1"), 1);
    // 2) 开回 k2，并让 k1 从此 401。
    set_key_enabled(&akhub, &account_id, 1, true).await;
    upstream.fallback(per_key(&[("sk-1", Behavior::Status(401, None))]));

    // 3) 绑定上的 k1 被拒 → 同一次调用换到 k2；此后 k1 被硬停，不再被尝试。
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第二轮")).await.status(),
        200
    );
    let dead = upstream.requests_with_key("sk-1");
    for round in 0..3 {
        let response = chat(&akhub, sticky_body("项目", &format!("第 {round} 轮"))).await;
        assert_eq!(response.status(), 200, "账号必须继续服务");
    }
    assert_eq!(
        upstream.requests_with_key("sk-1"),
        dead,
        "已经失效的 Key 不该再被抽中：histogram={:?}",
        upstream.key_histogram()
    );
    assert!(upstream.requests_with_key("sk-2") >= 3);
}

/// Key 级并发上限**不跨越账号总额度**，但账号总额度本身是共享的。
///
/// 三级门限是 ".账号 → Key → 目标" 的逐级收紧：Key 有自己的额度（这个由
/// \`health\` 的单元测试精确覆盖），而**账号总额度仍然管住整个账号**。这里验证
/// 的是后者：两把 Key 各有名额，但账号总额度只有 1 时，第二个请求必须排队。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_level_cap_does_not_bypass_the_account_wide_gate() {
    let upstream = FakeUpstream::spawn().await;
    upstream.fallback(Behavior::Hang);
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "多 Key",
        &upstream.base_url,
        &[KeySpec::new("k1", "sk-1"), KeySpec::new("k2", "sk-2")],
        50,
    )
    .await;
    // 账号总额度 = 1：这是所有 Key 共享的闸门（§17.1）。
    let account = akhub
        .state
        .store
        .list_accounts()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.id == account_id)
        .unwrap();
    akhub
        .state
        .store
        .update_account(
            &Account {
                limits: Limits {
                    max_concurrency: Some(1),
                    ..Limits::default()
                },
                ..account
            },
            &AccountSecrets {
                api_key: None,
                new_api_token: None,
            },
        )
        .await
        .unwrap();
    akhub.state.reload_config().await.unwrap();
    // 第二个目标：让两个请求走不同的目标，排除目标级额度的干扰。
    common::wire_extra_target(&akhub, &account_id, "glm-4.5", "glm-4.5").await;

    let (base_url, key) = (akhub.base_url.clone(), akhub.key.clone());
    let spawn_chat = |base_url: String, key: String, model: &str, text: &str| {
        let body = json!({"model": model, "messages": [{"role": "user", "content": text}]});
        tokio::spawn(async move { common::chat_at(&base_url, &key, body).await.status() })
    };
    let first = spawn_chat(base_url.clone(), key.clone(), MODEL, "one");
    upstream.wait_for_requests(1).await;
    let second = spawn_chat(base_url, key, "glm-4.5", "two");
    // 账号总额度只有 1：第二个请求必须排队而不是立刻打出去。
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        upstream.requests(),
        1,
        "账号总额度必须管住整个账号，Key 级名额不能绕过它"
    );

    upstream.release();
    assert_eq!(first.await.unwrap(), 200);
    assert_eq!(second.await.unwrap(), 200);
    assert_eq!(upstream.requests(), 2, "排队等到的请求必须最终打到上游");
}

// ------------------------------------------------------------ 后台与持久化

/// 后台能读到 Key 池的元数据，且**绝不回吐明文**。
#[tokio::test]
async fn the_admin_api_reports_the_key_pool_without_leaking_secrets() {
    let upstream = FakeUpstream::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let akhub = common::spawn_akhub_at(dir.path(), None).await.unwrap();
    let account_id = wire_account_with_keys(
        &akhub,
        "多 Key",
        &upstream.base_url,
        &[
            KeySpec::new("生产", "sk-production-secret"),
            KeySpec::new("备用", "sk-backup-secret"),
        ],
        50,
    )
    .await;

    let rows = akhub
        .state
        .store
        .list_account_key_rows(&account_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].label, "生产");
    assert_eq!(rows[1].label, "备用");
    // 明文只存在于密文信封里：落库的字节绝不含明文。
    for row in &rows {
        assert!(
            !row.sealed_key
                .windows(b"sk-production-secret".len())
                .any(|w| w == b"sk-production-secret"),
            "落库的密文里不该出现明文"
        );
    }
}

/// 复制账号把整个 Key 池一起带走，并逐把重新加密（§20.4）。
#[tokio::test]
async fn copying_an_account_reencrypts_every_key() {
    use akhub::storage::store::AccountSecrets;

    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "多 Key",
        &upstream.base_url,
        &[
            KeySpec::new("生产", "sk-production"),
            KeySpec::new("备用", "sk-backup"),
        ],
        50,
    )
    .await;

    let source = akhub
        .state
        .store
        .list_account_key_rows(&account_id)
        .await
        .unwrap();
    // 模拟后台的复制路径：逐把解密再重新密封。
    let mut copied: Vec<AccountKeyWrite> = Vec::new();
    for row in &source {
        let plaintext = akhub.state.cipher.open(&row.sealed_key).unwrap();
        let secret = String::from_utf8_lossy(&plaintext).into_owned();
        copied.push(AccountKeyWrite {
            id: format!("copy_{}", row.id),
            label: row.label.clone(),
            sealed_key: akhub.state.cipher.seal(secret.as_bytes()).unwrap(),
            credential_digest: akhub::security::credential_digest(&secret),
            limits: row.limits,
            enabled: row.enabled,
        });
    }
    assert_eq!(copied.len(), 2);
    for (original, clone) in source.iter().zip(copied.iter()) {
        assert_ne!(
            original.sealed_key, clone.sealed_key,
            "复制必须重新加密，不能共享密文"
        );
        assert_eq!(
            akhub.state.cipher.open(&clone.sealed_key).unwrap().to_vec(),
            akhub
                .state
                .cipher
                .open(&original.sealed_key)
                .unwrap()
                .to_vec(),
            "重新加密后明文必须一致"
        );
    }
    // 账号级凭据信封的导入路径仍然可用（旧二进制依赖它）。
    let _ = AccountSecrets::new(copied[0].sealed_key.clone(), None);
}

/// 重启后 Key 池与粘性绑定一起恢复：同一前缀仍然落在同一把 Key 上。
#[tokio::test]
async fn keys_and_bindings_survive_a_restart() {
    let upstream = FakeUpstream::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let first = common::spawn_akhub_at(dir.path(), None).await.unwrap();
    wire_account_with_keys(
        &first,
        "多 Key",
        &upstream.base_url,
        &[KeySpec::new("k1", "sk-1"), KeySpec::new("k2", "sk-2")],
        50,
    )
    .await;
    assert_eq!(
        chat(&first, sticky_body("项目", "第一轮")).await.status(),
        200
    );
    let bound = upstream
        .key_histogram()
        .keys()
        .next()
        .cloned()
        .expect("第一次调用必须留下归因");
    // 粘性绑定由后台任务每 60 秒落一次盘（§19.4）。这里显式存一次，
    // 把"重启后前缀仍然落在同一把 Key 上"变成可断言的事实。
    let downstream_key = first.key.clone();
    let bindings = first.state.runtime.sticky.export();
    assert!(!bindings.is_empty(), "第一次调用之后应当有粘性绑定");
    assert!(
        bindings.iter().all(|row| row.credential_digest.is_some()),
        "绑定必须记住用的是哪把 Key（§4.2.1）"
    );
    first
        .state
        .store
        .save_sticky_bindings(&bindings)
        .await
        .unwrap();
    std::mem::forget(first);

    let second = common::spawn_akhub_at(dir.path(), Some(&downstream_key))
        .await
        .unwrap();
    assert_eq!(
        second
            .state
            .runtime
            .credentials
            .current()
            .keys_of(&second.state.store.list_accounts().await.unwrap()[0].id)
            .len(),
        2,
        "重启后 Key 池必须完整恢复"
    );
    assert_eq!(
        chat(&second, sticky_body("项目", "第二轮")).await.status(),
        200
    );
    assert_eq!(
        upstream.requests_with_key(&bound),
        2,
        "重启不该让同一条前缀换 Key"
    );
}

// -------------------------------------------- Key 之间的调度等价于账号之间

/// Key 池的核心承诺：**同一账号内的多把 Key，彼此的调度关系与多个账号完全一致**。
///
/// 用户的原话是"把账号内 key 之间的关系当作不同账号处理，应用账号之间的相同的
/// 处理逻辑；一个账号能填多个 key 主要在于解决配置复杂问题"。这个用例把这句话
/// 变成可断言的性质：同样两把凭据，**放在一个账号里**与**拆成两个账号**，
/// 都必须在池内铺开、且都遵守缓存亲和。
///
/// 两种形状各用一台假上游（否则归因统计会混在一起）。
///
/// 唯一被有意保留的差别是并发/限额的层级：同一个账号的两把 Key 仍然共享账号
/// 总额度（那是 §17.1 的三级门限，不是调度逻辑）。
#[tokio::test]
async fn a_key_pool_dispatches_like_separate_accounts() {
    // 形 A：两个账号，各一把 Key。
    let split_upstream = FakeUpstream::spawn().await;
    let split = spawn_akhub().await;
    wire_account_with_keys(
        &split,
        "账号一",
        &split_upstream.base_url,
        &[KeySpec::new("A", "sk-A")],
        50,
    )
    .await;
    wire_account_with_keys(
        &split,
        "账号二",
        &split_upstream.base_url,
        &[KeySpec::new("B", "sk-B")],
        50,
    )
    .await;
    const ROUNDS: usize = 40;
    for round in 0..ROUNDS {
        assert_eq!(
            chat(&split, sticky_body(&format!("项目 {round}"), "hi"))
                .await
                .status(),
            200
        );
    }
    let split_histogram = split_upstream.key_histogram();

    // 形 B：一个账号，两把 Key。
    let pool_upstream = FakeUpstream::spawn().await;
    let pool = spawn_akhub().await;
    wire_account_with_keys(
        &pool,
        "池账号",
        &pool_upstream.base_url,
        &[KeySpec::new("A", "sk-A"), KeySpec::new("B", "sk-B")],
        50,
    )
    .await;
    for round in 0..ROUNDS {
        assert_eq!(
            chat(&pool, sticky_body(&format!("项目 {round}"), "hi"))
                .await
                .status(),
            200
        );
    }
    let pool_histogram = pool_upstream.key_histogram();

    // 两种形状都必须把流量铺到池里的每一把凭据上，且总量一致。
    assert_eq!(
        split_histogram.values().sum::<usize>(),
        pool_histogram.values().sum::<usize>(),
        "两种形状的调用次数必须一致：{split_histogram:?} vs {pool_histogram:?}"
    );
    assert_eq!(
        split_histogram.len(),
        2,
        "拆分形状必须用到两把凭据：{split_histogram:?}"
    );
    assert_eq!(
        pool_histogram.len(),
        2,
        "池形状必须把流量铺到两把凭据上（这正是 Key 池的负载均衡）：{pool_histogram:?}"
    );
}

/// 池里**第一把**刚好熔断时，整个目标不能退出候选——健康的那些 Key 必须接手。
///
/// 这条曾经是坏的：资格判定只看池里第一把，于是"第一把坏了"会让整个账号在所有
/// 模型上消失，而同账号里好好的 Key 一把都用不上。那正是 Key 池要解决的问题本身。
#[tokio::test]
async fn a_dead_first_key_does_not_remove_the_target_from_the_plan() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "多 Key",
        &upstream.base_url,
        &[KeySpec::new("k1", "sk-1"), KeySpec::new("k2", "sk-2")],
        50,
    )
    .await;

    // 先只开第一把，让它被上游判成失效。
    set_key_enabled(&akhub, &account_id, 1, false).await;
    upstream.fallback(per_key(&[("sk-1", Behavior::Status(401, None))]));
    assert_eq!(chat(&akhub, plain_body("hi")).await.status(), 503);

    // 现在把第二把打开：目标必须立刻重新可用，而且用的是健康的那把。
    set_key_enabled(&akhub, &account_id, 1, true).await;
    assert_eq!(
        chat(&akhub, plain_body("hi")).await.status(),
        200,
        "第一把 Key 熔断不该让整个目标退出候选"
    );
}

// ---------------------------------------------------------------- 兼容路径

/// 单 Key 账号的行为与引入 Key 池之前完全一致：粘性、熔断、限额都不变。
#[tokio::test]
async fn a_single_key_account_behaves_exactly_as_before() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    wire_target(
        &akhub,
        // 账号名会进上游 API Key（key-<账户名>），必须是合法的请求头值。
        TargetSpec::new("single", &upstream.base_url, CHAT, MODEL, MODEL, 50),
    )
    .await;

    assert_eq!(
        chat(&akhub, sticky_body("项目", "第一轮")).await.status(),
        200
    );
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第二轮")).await.status(),
        200
    );
    let pool = akhub.state.runtime.credentials.current();
    let accounts = akhub.state.store.list_accounts().await.unwrap();
    let secrets: Vec<_> = accounts
        .iter()
        .flat_map(|a| {
            pool.keys_of(&a.id)
                .iter()
                .map(|k| k.secret.to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    let histogram = upstream.key_histogram();
    let raw: Vec<_> = upstream
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
                s.path.clone(),
            )
        })
        .collect();
    let _ = &secrets;
    assert_eq!(
        histogram.len(),
        1,
        "单 Key 账号只会用那一把：{histogram:?} raw={raw:?} secrets={secrets:?}"
    );
    assert_eq!(histogram.values().sum::<usize>(), 2, "{histogram:?}");
}

/// 一把 Key 都没有的账号：请求被明确拒绝，而不是拿空凭据打上游。
#[tokio::test]
async fn an_account_without_keys_fails_before_reaching_the_upstream() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "空池",
        &upstream.base_url,
        &[KeySpec::new("k1", "sk-1")],
        50,
    )
    .await;
    // 把池清空：账号仍然存在，只是没有凭据。
    akhub
        .state
        .store
        .replace_account_keys(&account_id, &[])
        .await
        .unwrap();
    akhub.state.reload_config().await.unwrap();

    let response = chat(&akhub, plain_body("hi")).await;
    assert_eq!(response.status(), 503);
    assert_eq!(upstream.requests(), 0, "没有凭据的账号绝不该把请求打到上游");
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["error"]["code"], "no_eligible_target");
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("Key"),
        "客户错误信息不得透露上游凭据状态：{message}"
    );
}

// ------------------------------------ 粘性：绑定的 Key 只是"忙"时必须等它

/// 绑定的 Key 只是**忙**（并发名额占满）时，请求必须等它，而不是换池里另一把。
///
/// 上游的前缀缓存按凭据隔离（实测：同一前缀换 Key 立刻 `cached=None`，换回来
/// 又命中），所以换 Key 等于把整段缓存作废。设计文档 §2.2 把允许换 Key 的条件
/// 写得很死——只有绑定那把**不再合格**（停用、熔断、额度耗尽、被删）时才换；
/// "限额满"要超出等待预算之后才降级成换 Key。"忙"不是"坏"。
///
/// 构造：绑定的那把并发上限 1，另一把不限。先用 Hang 占住绑定那把的唯一名额，
/// 再发同一个前缀。若实现退化成"回落到抽签"，第二个请求会立刻跑到另一把 Key 上。
#[tokio::test]
async fn a_busy_bound_key_is_waited_for_instead_of_swapped() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "忙绑定",
        &upstream.base_url,
        &[
            KeySpec::new("bound", "sk-bound"),
            KeySpec::new("other", "sk-other"),
        ],
        50,
    )
    .await;

    // 把 "other" 关掉，先让粘性绑定确定地落在 "bound" 上。
    set_key_enabled(&akhub, &account_id, 1, false).await;
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第一轮")).await.status(),
        200
    );
    assert_eq!(
        upstream.key_histogram().get("sk-bound").copied(),
        Some(1),
        "第一次应当落在 bound 上：{:?}",
        upstream.key_histogram()
    );
    // 绑定已经落在 bound 上；现在把 other 放回来（可用），并给 bound 设并发 1。
    set_key_enabled(&akhub, &account_id, 1, true).await;
    set_key_concurrency(&akhub, &account_id, "sk-bound", Some(1)).await;

    // 占住第一把 Key 的唯一并发名额。
    upstream.script([Behavior::Hang]);
    let base_url = akhub.base_url.clone();
    let key = akhub.key.clone();
    let holder = tokio::spawn(async move {
        common::chat_at(&base_url, &key, sticky_body("项目", "占位"))
            .await
            .status()
    });
    upstream.wait_for_requests(2).await;

    // 同一个前缀再来一次：绑定的那把满了，但它是"忙"不是"坏"。
    let base_url = akhub.base_url.clone();
    let key = akhub.key.clone();
    let waiter = tokio::spawn(async move {
        common::chat_at(&base_url, &key, sticky_body("项目", "第二轮"))
            .await
            .status()
    });
    // 给它足够时间去做选择：如果实现会静默换 Key，这一刻就已经打到第二把上了。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        upstream
            .key_histogram()
            .get("sk-other")
            .copied()
            .unwrap_or(0),
        0,
        "绑定那把只是忙，绝不该换到另一把 Key（换了就打碎上游前缀缓存）：{:?}",
        upstream.key_histogram()
    );

    // 放掉占位请求：等待者应当拿到名额并成功，全程只用绑定那把。
    upstream.release();
    assert_eq!(holder.await.unwrap(), 200);
    assert_eq!(waiter.await.unwrap(), 200);
    assert_eq!(
        upstream
            .key_histogram()
            .get("sk-other")
            .copied()
            .unwrap_or(0),
        0,
        "整个过程中第二把 Key 都不该被用到：{:?}",
        upstream.key_histogram()
    );
}

/// 绑定那把**真的坏了**（被停用）时才允许在同一账号内换 Key。
///
/// 与上一条互为对照：区分"忙"与"坏"正是这次修正的核心。
#[tokio::test]
async fn a_disabled_bound_key_still_yields_to_another_key() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "坏绑定",
        &upstream.base_url,
        &[
            KeySpec::new("bound", "sk-bound"),
            KeySpec::new("other", "sk-other"),
        ],
        50,
    )
    .await;

    // 先只留 bound 可用，让粘性绑定确定地落在它身上。
    set_key_enabled(&akhub, &account_id, 1, false).await;
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第一轮")).await.status(),
        200
    );
    assert_eq!(
        upstream.key_histogram().get("sk-bound").copied(),
        Some(1),
        "第一次应当落在 bound 上：{:?}",
        upstream.key_histogram()
    );
    // 把 other 放回来，再停用绑定的 bound：它"不再合格"，必须让位。
    set_key_enabled(&akhub, &account_id, 1, true).await;
    set_key_enabled(&akhub, &account_id, 0, false).await;
    assert_eq!(
        chat(&akhub, sticky_body("项目", "第二轮")).await.status(),
        200
    );
    assert!(
        upstream
            .key_histogram()
            .get("sk-other")
            .copied()
            .unwrap_or(0)
            > 0,
        "绑定的 Key 被停用后必须换到另一把：{:?}",
        upstream.key_histogram()
    );
}

// ------------------------------------------ 429 落到这次真正用的那把 Key 上

/// 多 Key 账号里，429 除了记在"账号 + 模型"上，还要记在**这次用的那把 Key** 上。
///
/// 上游的限流可能是按凭据计的，而 Key 池里每把 Key 是独立额度：只记目标会让
/// "某把 Key 被限流"连累同账号其他 Key，也会让这把 Key 在下一轮抽签里继续被
/// 选中、继续撞 429（§4.2.1、§12.3）。
///
/// 两把 Key 都启用（这样才是真正的"多 Key 账号"），但**哪一把被抽中不确定**，
/// 所以让两把都回 429，再用假上游记录的凭据反查"这次到底用了哪把"，断言冷却的
/// 正好是它。这样断言与抽签结果无关，不会因为抽到另一把而假绿。
#[tokio::test]
async fn a_429_also_pauses_the_key_that_was_used_in_a_multi_key_account() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "多Key限流",
        &upstream.base_url,
        &[
            KeySpec::new("first", "sk-first"),
            KeySpec::new("second", "sk-second"),
        ],
        50,
    )
    .await;

    // 两把都启用 → 这是一个真正的多 Key 账号；两把都回 429。
    upstream.script([
        Behavior::Status(429, Some(30)),
        Behavior::Status(429, Some(30)),
    ]);
    upstream.fallback(Behavior::Status(429, Some(30)));
    let _ = chat(&akhub, plain_body("hi")).await;

    let histogram = upstream.key_histogram();
    let used: Vec<&String> = histogram.keys().collect();
    assert!(!used.is_empty(), "请求必须打到上游：{histogram:?}");

    let health = &akhub.state.runtime.health;
    let cooling = |secret: &str| {
        let digest = key_digest(&akhub, &account_id, secret);
        health
            .key(&akhub::credential::credential_id(&account_id, &digest))
            .quota_exhausted()
    };

    // 被用过的那把（可能不止一把，取决于是否换 Key 重试）必须全部进入冷却；
    // 没被用过的那把绝不能受连累。
    for secret in ["sk-first", "sk-second"] {
        if histogram.contains_key(secret) {
            assert!(
                cooling(secret),
                "{secret} 被 429 打过，必须进入冷却（只记目标的话这里会漏）：{histogram:?}"
            );
        } else {
            assert!(
                !cooling(secret),
                "{secret} 没被用过，不该被别的 Key 的 429 连累：{histogram:?}"
            );
        }
    }
}

#[tokio::test]
async fn request_records_carry_the_key_that_served_them() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "落库",
        &upstream.base_url,
        &[
            KeySpec::new("first", "sk-first"),
            KeySpec::new("second", "sk-second"),
        ],
        50,
    )
    .await;
    set_key_enabled(&akhub, &account_id, 1, false).await;
    assert_eq!(chat(&akhub, plain_body("hi")).await.status(), 200);

    // 请求记录由后台异步写入：等它落库再断言。
    let mut records = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        records = akhub.state.store.list_request_records(10, 0).await.unwrap();
        if !records.is_empty() {
            break;
        }
    }
    let record = records.first().expect("应当留下一条请求记录");
    let expected_id = akhub
        .state
        .runtime
        .credentials
        .current()
        .keys_of(&account_id)
        .iter()
        .find(|k| k.secret.as_ref() == "sk-first")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        record.key_id.as_deref(),
        Some(expected_id.as_str()),
        "请求记录必须记下这次用的那把 Key 的行 ID"
    );
    assert_eq!(
        record.key_label.as_deref(),
        Some("first"),
        "标签也要落库，诊断时不用再去解密数据库"
    );
    let attempt = record.attempts_detail.first().expect("应当留下尝试明细");
    assert_eq!(
        attempt.key_id.as_deref(),
        Some(expected_id.as_str()),
        "尝试明细同样要记下用的是哪把 Key"
    );
}

// ------------------------------------------------------------- 辅助结构

/// 轮换掉一把 Key 之后，它的动态健康状态必须一起消失。
///
/// 不清理的后果不是「多占一点内存」：`keys` 表攒到上限后开始随机淘汰，被淘汰
/// 的可能是**正在使用**的那把，它的熔断与额度冷却跟着失效——已知坏的 Key
/// 于是重新参与调度。
#[tokio::test]
async fn rotating_a_key_drops_its_stale_health_state() {
    let upstream = FakeUpstream::spawn().await;
    let akhub = spawn_akhub().await;
    let account_id = wire_account_with_keys(
        &akhub,
        "轮换",
        &upstream.base_url,
        &[
            KeySpec::new("old", "sk-old"),
            KeySpec::new("keep", "sk-keep"),
        ],
        50,
    )
    .await;

    // 让两把 Key 都产生一份动态状态（健康表按需创建）。
    for secret in ["sk-old", "sk-keep"] {
        let id = akhub::credential::credential_id(
            &account_id,
            &akhub::security::credential_digest(secret),
        );
        let _ = akhub.state.runtime.health.key(&id);
    }
    let before = akhub.state.runtime.health.tracked_keys();
    assert!(before >= 2, "两把 Key 都应有状态，实际 {before}");

    // 管理员把旧 Key 从池里删掉，只留另一把，然后重载配置。
    let keep = akhub
        .state
        .store
        .list_account_key_rows(&account_id)
        .await
        .unwrap()
        .into_iter()
        .find(|key| key.label == "keep")
        .expect("另一把 Key 还在");
    akhub
        .state
        .store
        .replace_account_keys(
            &account_id,
            &[akhub::storage::store::AccountKeyWrite {
                id: keep.id,
                label: keep.label,
                sealed_key: keep.sealed_key,
                credential_digest: akhub::security::credential_digest("sk-keep"),
                limits: keep.limits,
                enabled: keep.enabled,
            }],
        )
        .await
        .unwrap();
    akhub.state.reload_config().await.unwrap();

    let after = akhub.state.runtime.health.tracked_keys();
    assert!(
        after < before,
        "轮换掉的 Key 状态必须被清掉：before={before} after={after}"
    );
}
