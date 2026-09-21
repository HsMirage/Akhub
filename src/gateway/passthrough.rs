//! 请求转发：端点选择、跨协议转换、故障切换与流式边界。
//!
//! 同协议时只改鉴权头、Base URL 和模型名，其余字节按原样送达，未知字段因此
//! 天然保留。上游没有匹配端点时才进入 [`crate::protocol`] 的中间格式转换，
//! 转换中丢弃的白名单能力会写进 `X-Akhub-Degraded` 与请求记录（§14.8）。
//!
//! 调度侧的三件事：按严格阶梯与层内评分排出尝试顺序、粘性命中时直接复用已
//! 绑定目标、以及**流式切换边界**——在只收到 HTTP 头、空白、注释、ping 或协议
//! 开始标记时仍可切换，一旦发出有语义的增量就禁止拼接第二个上游。

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;

use crate::app::SharedState;
use crate::capability;
use crate::config::{GroupView, TargetView};
use crate::domain::{Multiplier, Protocol};
use crate::gateway::error::{ErrorCode, GatewayError};
use crate::gateway::{responses, settle, stream, translate};
use crate::health;
use crate::multiplier;
use crate::protocol::degrade;
use crate::protocol::translate::Translation;
use crate::routing::{self, queue, score, sticky};
use crate::storage::store::{AttemptRecord, RequestRecord};
use crate::upstream::endpoints::{self, Choice};
use crate::upstream::{self, Endpoint};

/// 允许从上游回传给下游的响应头。其余一律丢弃，避免泄漏上游身份（§14.7）。
const FORWARDED_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "retry-after",
    "x-ratelimit-limit-requests",
    "x-ratelimit-remaining-requests",
    "x-ratelimit-reset-requests",
    "x-ratelimit-limit-tokens",
    "x-ratelimit-remaining-tokens",
    "x-ratelimit-reset-tokens",
    "anthropic-ratelimit-requests-remaining",
    "anthropic-ratelimit-tokens-remaining",
];

/// 没有 tokenizer 时的保守 Token 估算：按字节数除以这个系数。
///
/// 英文大约 4 字节一个 token，中文 UTF-8 下大约 1.5。取 3 是偏保守的中间值：
/// 宁可高估把自己挡在限流外，也不要低估越过上游的 TPM（§17.2）。
const BYTES_PER_TOKEN: usize = 3;

/// 等待 RPM / TPM 窗口释放时的轮询间隔。
///
/// 并发名额有信号量可等，限流窗口只随时间推移释放，没有可等的信号。
const RATE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 按上游的 `Retry-After` 等待时额外多等的余量，避免卡在冷却结束的临界点上。
const RETRY_AFTER_MARGIN: Duration = Duration::from_millis(100);

/// 一次请求针对**同一个目标**最多换几把 Key（§4.2.1）。
///
/// 换 Key 属于 §13.1 的廉价失败（连凭据都没被接受），但它必须设上限：一个
/// 账号里 50 把 Key 全是坏的时候，一次下游请求会打出 50 次上游尝试。取 3 与
/// "三把坏 Key 就足够断定这个账号的凭据有问题"这个判断一致。
const MAX_KEY_SWITCHES_PER_TARGET: usize = 3;

/// 挑 Key 的结果（§4.2.1）。
///
/// **"忙"与"坏"必须分开**：所有 Key 都只是暂时限流/并发满时，这个候选应当进入
/// 排队等待，与账号层的 §13.6 完全一致；只有一把可用的 Key 都没有时才算这个
/// 候选不合格。
enum Picked {
    Ready(Option<Arc<crate::credential::Credential>>),
    /// 账号里所有 Key 都只是忙——等一会儿就有名额。
    Busy,
    /// 账号一把能用的 Key 都没有。
    Unavailable(String),
}

/// 一次尝试的归属：正常结束，还是"这把 Key 不行、换一把再试"。
enum Attempted {
    /// 已经有最终结果（成功、终止性错误或可切换失败）。
    Done(Flow),
    /// 凭据被上游拒绝。带着准入一并返回，由调用方结算到**那一把** Key 上。
    CredentialFailed(health::Admission),
}

/// 凭据被拒绝对应的健康结果。
///
/// 只有 401/403 与额度耗尽会走到这里（调用方已经筛过），所以保留
/// `Retry-After` 供额度熔断使用。
fn bad_key_outcome(retry_after: Option<Duration>) -> health::Outcome {
    match retry_after {
        Some(wait) => health::Outcome::QuotaExhausted {
            retry_after: Some(wait),
        },
        None => health::Outcome::KeyInvalid,
    }
}

/// 需要原样发送的请求正文。`content_type` 保留客户端提供的 multipart
/// boundary，不能被统一 JSON 头覆盖。
#[derive(Debug, Clone)]
pub struct RawBody {
    pub bytes: Vec<u8>,
    pub content_type: HeaderValue,
}

/// 一次转发请求的全部输入。
pub struct Forward<'a> {
    pub state: &'a SharedState,
    pub group: &'a Arc<GroupView>,
    pub endpoint: Endpoint,
    pub request_id: &'a str,
    pub downstream_headers: &'a HeaderMap,
    /// 已解析的请求体。模型名会被逐目标改写。
    pub body: serde_json::Value,
    /// 可选的原始请求体（目前用于 multipart 图片编辑）。
    pub raw: Option<RawBody>,
    pub logical_model: String,
    pub request_bytes: usize,
    pub started_at: Instant,
    /// 请求进入网关时的 Unix 秒，用于请求记录按真实开始时间归档。
    pub started_unix: i64,
    /// Responses 状态链处理计划。所有入口都携带；非 Responses 入口它只是
    /// 没有引用与固定候选的空计划。
    pub chain: responses::ChainPlan,
}

/// 单次尝试的失败原因，决定是否继续尝试下一个目标（§13.2、§13.3）。
#[derive(Debug)]
enum AttemptFailure {
    /// 廉价失败：上游未开始生成，可以切换（§13.1）。
    Switchable {
        code: ErrorCode,
        message: String,
        upstream_status: Option<StatusCode>,
        /// 上游给出的恢复时间。粘性请求据此决定等待还是换号（§10.3）。
        retry_after: Option<Duration>,
    },
    /// 明确属于下游请求本身的问题，切换到别的目标也是同样结果。
    /// 装箱是因为 `Response` 比其余变体大一个数量级，而这是**失败**分支：
    /// 让成功路径为它多搬 128 字节不划算。
    Terminal(Box<Response>),
    /// 这条路由不存在。换个端点再试**同一个**目标，不算这个目标失败——
    /// 账号首选 Chat 但上游也有 Messages 时，正是靠这一步学会走哪条路
    /// （§14.2、§16.7）。
    MissingEndpoint,
}

impl AttemptFailure {
    fn switchable(code: ErrorCode, message: String) -> Self {
        Self::Switchable {
            code,
            message,
            upstream_status: None,
            retry_after: None,
        }
    }

    fn with_status(self, status: StatusCode) -> Self {
        match self {
            Self::Switchable {
                code,
                message,
                retry_after,
                ..
            } => Self::Switchable {
                code,
                message,
                upstream_status: Some(status),
                retry_after,
            },
            terminal => terminal,
        }
    }
}

/// 一次请求的可观测统计，最终写进请求记录。
#[derive(Default)]
struct Telemetry {
    attempts: i64,
    queued: Duration,
    /// 为保住前缀缓存而等待的时长，与普通排队分开计（§6.6、§24.1）。
    sticky_wait: Option<Duration>,
    /// 这次粘性等待用到的缓存新鲜度系数（§10.3）。
    sticky_freshness: Option<f64>,
    sticky_hit: bool,
    cheapest: Option<Multiplier>,
    dearest: Option<Multiplier>,
    effective: Option<Multiplier>,
    /// 实际使用的上游端点。
    endpoint: Option<Endpoint>,
    /// 本次为了完成请求丢弃的白名单能力（§14.8）。
    degraded: Vec<String>,
    /// 倍率来源：`auto` / `manual`（§24.1）。
    multiplier_source: Option<&'static str>,
    /// 候选过滤原因摘要（§24.1）。
    filter_summary: Option<String>,
    /// 最终选中的层；粘性命中时是绑定目标所在的层（§24.1）。
    selected_layer: Option<i64>,
    /// 本次请求实际使用的 Key 的内部 ID 与标签（§4.2.1）。
    ///
    /// **只记 ID 与标签，绝不记凭据本身**：请求记录要能回答"这次走的哪把
    /// Key"，但它是给人看的诊断信息，不是密钥仓库（§23.4、§24.1）。
    key_id: Option<String>,
    key_label: Option<String>,
}

/// 一次请求在候选之间游走时的全部可变状态。
struct Walk<'a> {
    forward: &'a Forward<'a>,
    /// 三个协议的转换缓存。同协议目标不碰它（§14.1）。
    translation: &'a Translation<'a>,
    telemetry: Telemetry,
    streaming: bool,
    estimated_tokens: u64,
    deadline: Instant,
    /// 队列等待的截止时刻：分组配置的"最长等待"与请求总超时取更早者（§6.3）。
    queue_deadline: Instant,
    now_unix: i64,
    /// 已经真正发过请求的（目标, 凭据）。同一个目标可以换 Key 重试，但同一对
    /// 组合只发一次（§4.2.1、§13.1）。
    attempted: Vec<AttemptedKey>,
    /// 最后一次可切换失败，用来在候选耗尽时决定错误码。
    last: Option<(ErrorCode, String)>,
    /// 最后一次失败附带的 `Retry-After`，粘性路径据此决定是否原地等待（§10.3）。
    last_retry_after: Option<Duration>,
    /// 本次请求最终的 (输入, 输出) Token，成功时写入（§6.6）。
    usage_parts: (Option<u64>, Option<u64>),
    /// 非流式成功时拿到的完整 Token 细分（§11.6）。
    usage_detail: Option<stream::UsageBreakdown>,
    /// 每次上游尝试的明细，随请求记录一起落库（§6.6）。
    attempt_log: Vec<AttemptRecord>,
    /// 弱身份（稳定前缀）软亲和所偏好的**凭据摘要**（§10.1 修订）。
    ///
    /// 软亲和偏的是"那把 Key"，不是"那个目标"：上游的前缀缓存按凭据隔离，
    /// 保住 Key 才是保住缓存。目标会不会变由层内抽签决定——某个账号变慢时，
    /// 评分把它的权重压下去，流量才真的走得掉。
    soft_affinity: Option<String>,
}

/// 一次请求里"用过哪把 Key"的记录（§4.2.1）。
type AttemptedKey = (String, Option<String>);

/// 一次尝试要发出的东西：端点、已改写模型名的请求体与降级记录。
struct Prepared {
    endpoint: Endpoint,
    body: serde_json::Value,
    /// 原始请求体经过当前目标模型名替换后的字节；存在时不做 JSON 发射。
    raw: Option<Vec<u8>>,
    degraded: Vec<String>,
}

/// 三级门限：账号总额度 → Key 覆盖 → 目标覆盖（§4.2.1）。
fn admission_limits(
    candidate: &routing::Candidate,
    credential: Option<&Arc<crate::credential::Credential>>,
) -> health::AdmissionLimits {
    health::AdmissionLimits {
        account: candidate.target.account.limits,
        key: credential.map(|key| key.limits).unwrap_or_default(),
        target: candidate.target.target.limits,
    }
}

/// 这次尝试的发起者：账号 + 具体 Key + 目标。
///
/// `key_id` 填凭据摘要：动态状态表按"账号 + 摘要"归类，换标签或重新粘贴
/// 同一把 Key 都不会丢掉熔断与额度状态（§4.2.1）。
fn admission_caller<'a>(
    candidate: &'a routing::Candidate,
    credential: Option<&'a Arc<crate::credential::Credential>>,
) -> health::Caller<'a> {
    health::Caller {
        account_id: &candidate.target.account.id,
        key_id: credential.map(|key| key.credential_digest.as_str()),
        target_id: &candidate.target.target.id,
    }
}

/// 一段游走的结果。
enum Flow {
    /// 已经拿到可以直接返回的响应（成功、终止性错误或网关错误）。
    Done(Response),
    /// 这一段没有结果，继续下一段。
    Continue,
    /// 等待预算耗尽。粘性路径据此降级，层路径据此报 `queue_timeout`。
    Exhausted,
    /// 分组排队总容量已满。
    QueueFull,
}

/// 按严格阶梯依次尝试候选目标，返回第一个成功的上游响应。
pub async fn forward(forward: Forward<'_>) -> Response {
    // 网关托管后台任务：满足条件时登记任务并立刻返回任务对象，真正的执行在
    // 后台跑（计划 §29.1）。不满足条件就照常走同步转发。
    if let Some(response) = crate::gateway::background::maybe_start_managed(&forward).await {
        return response;
    }
    forward_no_managed(forward).await
}

/// 不走托管分流的转发入口。
///
/// 托管任务内部调用它：任务执行时已经去掉 `background`，再进一次分流既没有
/// 意义，也会让 `Send` 推断形成环（`forward` → 托管 → spawn(`execute`) →
/// `forward` → …）。类型上把这条回路切断，编译器才能证明 future 是 `Send`。
pub async fn forward_no_managed(forward: Forward<'_>) -> Response {
    let streaming = forward
        .body
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let now_unix = crate::storage::now_unix();
    let multipliers = forward.state.runtime.multipliers.view();

    // 状态链带可重放正文时，资格过滤与转换都按**合并体**算：引用不在里面，
    // 跨协议目标因此保持合格（§15.2）。合并体不存在时按原始请求体。
    let multipliers_ref = &multipliers;
    forward_inner(forward, streaming, now_unix, multipliers_ref).await
}

async fn forward_inner<'a>(
    forward: Forward<'a>,
    streaming: bool,
    now_unix: i64,
    multipliers: &'a multiplier::View,
) -> Response {
    // 中间格式最多解析一次、每个目标协议最多发射一次（§19.4）。
    let translation = Translation::new(
        forward.endpoint.protocol(),
        forward.chain.body_for_translation(&forward.body),
    );
    // 凭据快照在整次请求内保持不变：换 Key 只能通过显式的重试发生，配置在
    // 请求中途被改动不会让后半段跑到另一把 Key 上（§4.2.1 的不变量 A）。
    let credentials = forward.state.runtime.credentials.current();
    let context = routing::Context {
        health: &forward.state.runtime.health,
        perf: &forward.state.runtime.perf,
        multipliers,
        credentials: &credentials,
        evidence: &forward.state.runtime.evidence,
        capabilities: &forward.state.runtime.capabilities,
        translation: &translation,
        endpoint: forward.endpoint,
        allow_degrade: forward.group.group.allow_degrade,
        protocol: forward.endpoint.protocol(),
        streaming,
        now_unix,
        now: Instant::now(),
        // 粘性绑定的凭据在下面查到之后才知道；计划本身不需要它。
        bound_credential: None,
    };

    let deadline = forward.started_at + forward.state.settings.get().request_timeout;
    let queue_deadline = match forward.group.group.max_wait_secs {
        // 0 = 跟随请求总超时。
        0 => deadline,
        secs => deadline.min(forward.started_at + Duration::from_secs(u64::from(secs))),
    };
    let mut walk = Walk {
        forward: &forward,
        translation: &translation,
        telemetry: Telemetry::default(),
        streaming,
        estimated_tokens: estimate_tokens(forward.request_bytes, &forward.body),
        deadline,
        queue_deadline,
        now_unix,
        attempted: Vec::new(),
        last: None,
        last_retry_after: None,
        usage_parts: (None, None),
        usage_detail: None,
        attempt_log: Vec::new(),
        // 软亲和偏好哪把 Key 由绑定决定，填充在下面（弱身份才有）。
        soft_affinity: None,
    };

    // 粘性键必须在**抽签之前**推导出来：弱身份（稳定前缀）的绑定要作为抽签的
    // 权重倾斜参与，而不是事后短路（§10.1 修订）。
    let sticky_key = sticky::derive(
        &forward.state.key_digest,
        &forward.group.group.id,
        &forward.logical_model,
        forward.downstream_headers,
        &forward.body,
    );
    // 绑定是"软"还是"硬"，取决于粘性键的来源：
    //
    // * **强身份**（1/2/3 级：响应链、显式会话头、prompt_cache_key）：客户端
    //   已经说明这是"同一件事的延续"，换号会真的打断它，必须钉死。
    // * **弱身份**（4 级：稳定前缀摘要）：它只是"同一个 agent 项目"。被同一套
    //   system prompt 与工具定义命中的**所有**会话都落在一个键上，把这种键硬
    //   钉在一个账号上就等于"一个项目 = 一个上游账号"，永远没有第二次抽签的
    //   机会——现场（gpt-boom / gpt-5.6-sol，259 条请求）正是如此：265 次粘性
    //   命中把首次抽签的结果整整固化了一天，评分与速度怎么变都没用。
    //
    // 弱绑定改为**软亲和**：只把绑定目标在层内抽签里的权重放大若干倍，不再
    // 短路抽签。这既保住"同一个项目倾向于落在同一个账号"（前缀缓存的价值），
    // 又让每个前缀在每次请求都有一次重新分配的机会（负载均衡的价值）。
    let soft = sticky_key
        .as_ref()
        .is_some_and(|(_, origin)| *origin == sticky::Origin::StablePrefix);
    let existing = sticky_key
        .as_ref()
        .and_then(|(key, _)| forward.state.runtime.sticky.get(key, now_unix));

    // ① 缓存已凉的绑定不再钉住（§10.1 修订）。
    //
    // 粘性命中的价值全部来自上游的前缀缓存；缓存凉了之后，继续钉住只是让
    // 首次抽签的结果永久固化——上游已经变慢变贵，流量却一分都挪不动。此时
    // 把它当成"没有绑定"重新按分数抽签，成功后用新赢家重绑：这是一次**零
    // 成本**的再平衡机会（缓存本来就要重建）。
    //
    // **响应链（第 1 级）例外**：那一级的粘性保护的不是缓存，而是"上游的响应
    // 状态真的存在那个账号上"。换号必须重放合并后的正文，而 store:false 的
    // 客户端没有正文可重放，挪过去只会 404。所以它永远保持硬钉住。
    let chain_bound = sticky_key
        .as_ref()
        .is_some_and(|(_, origin)| *origin == sticky::Origin::ResponseChain);
    // 记下被丢弃的那条绑定的新鲜度：sticky_hit=false 而 sticky_freshness 有值，
    // 就是"有绑定、但凉到不值得钉"的签名，便于在请求记录里解释这次为什么换了号。
    let cold_binding = existing
        .as_ref()
        .filter(|binding| !chain_bound && sticky::cache_is_cold(now_unix - binding.last_used_at));
    let cold_freshness =
        cold_binding.map(|binding| sticky::freshness_for(now_unix - binding.last_used_at));
    let existing = if cold_freshness.is_some() {
        None
    } else {
        existing
    };
    if let Some(freshness) = cold_freshness {
        walk.telemetry.sticky_freshness = Some(freshness);
    }

    let plan = match routing::plan(
        forward.group,
        &forward.logical_model,
        &context,
        // 只有弱绑定才做权重倾斜；硬绑定走下面的第一步，不经过抽签。
        soft.then(|| existing.as_ref().map(|binding| binding.target_id.as_str()))
            .flatten(),
        &mut score::random_unit,
    ) {
        Ok(plan) => plan,
        Err(failure) => return walk.fail(failure.code, failure.message),
    };
    walk.telemetry.cheapest = plan.cheapest;
    walk.telemetry.dearest = plan.dearest;
    walk.telemetry.filter_summary = Some(plan.filter_summary());
    walk.telemetry.selected_layer = plan.layers.first().map(|layer| i64::from(layer.priority));
    // 倍率来源与额度状态：都取自账号配置与动态状态，解释"为什么它能被选中"（§24.1）。
    walk.telemetry.multiplier_source = plan
        .layers
        .first()
        .and_then(|layer| layer.candidates.first())
        .map(|candidate| candidate.target.account.multiplier_mode.as_str());

    // ③ 守门式钉住（§10.1 修订）。
    //
    // 第 2/3 级（会话头、prompt_cache_key）以前是"永远钉死"：首次抽签落在谁
    // 身上，这条会话就再也不会换号，哪怕那个账号后來慢了一倍、贵了一倍。现场
    // 形态正是如此——一条 prompt_cache_key 连打 520 次全在同一个账号上。
    //
    // 现在改成"只要它还守得住分数就一直钉着"：同层里有别人领先超过 margin 时，
    // 这一次请求放弃钉住，直接走层内正常顺序（那本身就是按分数抽签的结果）。
    // 钉住带来的缓存收益因此只在它真的还划算时才保留。
    //
    // 两道闸门防止抖动：
    //   * margin 随缓存重建的代价上升（④，见 sticky::migrate_margin）；
    //   * 每会话每 MIGRATE_COOLDOWN 只允许翻盘一次（迟滞）。
    // 第 1 级（响应链）不在其列：它保护的是上游状态，不是缓存。
    let escaped = !soft
        && !chain_bound
        && existing.as_ref().is_some_and(|binding| {
            !sticky::migrate_cooling_down(now_unix, binding.migrated_at)
                && routing::pin_is_outpaced(
                    &plan,
                    &binding.target_id,
                    sticky::migrate_margin(forward.request_bytes, now_unix - binding.last_used_at),
                )
        });
    if escaped {
        // 没走绑定，就不是粘性命中；但仍然把新鲜度写下来，这样请求记录里
        // "hit=false 且 freshness>0.1" 就是"守门放行"的签名（§24.1）。
        walk.telemetry.sticky_freshness = existing
            .as_ref()
            .map(|binding| sticky::freshness_for(now_unix - binding.last_used_at));
    }

    // 强身份粘性命中的请求不参与抽签，直接走已绑定目标（§9.5）；弱身份
    // （稳定前缀）的绑定也在这里取出来，但只是为了拿到它的 Key 亲和——
    // 下面会把它清成 None、不抢占第一步。
    let bound = sticky_key.as_ref().and_then(|_| {
        if escaped {
            // 放弃钉住：返回 None 会走层内正常顺序。注意不能让下面那条
            // "目标已不合格"的清绑逻辑误伤它——绑定依然有效，只是这次不划算。
            return None;
        }
        let binding = existing?;
        // 普通粘性只能在当前最高合格层内生效：低层绑定不能绕过已恢复的高层
        // （§9.5）。这条硬边界与凭据无关，不能被 Key 池改掉。
        let candidate = routing::binding_target(&plan, &binding.target_id, true)?;
        // 绑定记的是"目标 + Key"（§4.2.1）。钉住那把 Key 做资格判定：它坏了
        // 就不该继续用这个绑定，否则每次请求都在 Key 之间漂移，把上游按凭据
        // 隔离的前缀缓存打碎。
        let pinned = binding.credential_digest.clone();
        let scoped = context.with_bound_credential(pinned.clone());
        if routing::sticky_still_valid(forward.group, &candidate.target, &scoped) {
            return Some((candidate, binding, pinned));
        }
        // 目标还合格、只是那把 Key 不再可用时，绑定仍然成立：请求照样打到这个
        // 目标，由发请求那一刻重新选一把 Key 并改写绑定。这比"整个绑定作废、
        // 重新抽签"温和得多——目标（也就省下了的连接与配额）没有变。
        let target_ok = routing::sticky_still_valid(forward.group, &candidate.target, &context);
        let key_gone = pinned.is_some()
            && binding.credential_digest.as_deref().is_some_and(|digest| {
                credentials
                    .by_digest(&candidate.target.account.id, digest)
                    .is_none()
            });
        (target_ok && key_gone).then_some((candidate, binding, None))
    });
    if bound.is_none()
        && !escaped
        && let Some((key, _)) = &sticky_key
    {
        // 绑定还在但目标已经不合格：清除，重新抽签（§10.2）。
        // 守门放行（escaped）不走这里：绑定依然有效，只是这一次不划算，
        // 清掉它会顺手把迁移迟滞的锚点也一起丢掉。
        forward.state.runtime.sticky.clear(key);
    }

    // 弱绑定不做第一步：它不抢占，只以"Key 亲和"的形式参与（§10.1 修订）。
    //
    // 关键的区分是**偏的是 Key，不是目标**：真正保住前缀缓存的是"同一把凭据"
    // （§4.2.1 的不变量 B），而目标会不会变正是评分该管的事。把目标也钉死，
    // 就等于某个账号一旦变慢就再也无法被流量反馈发现——现场（gpt-boom /
    // gpt-5.6-sol）正是 265 次粘性命中把首次抽签的结果锁死了一整天。
    // 目标变不变交给层内抽签（上面的 affinity 只做权重倾斜），Key 则优先复用
    // 绑定里的那一把。
    let bound = if soft {
        if let Some((_, binding, _)) = bound.as_ref() {
            walk.soft_affinity = binding.credential_digest.clone();
            // 软命中同样是"命中"：不等待，但要在记录里区分于"根本没粘上"（§24.1）。
            walk.telemetry.sticky_hit = true;
            walk.telemetry.sticky_wait = Some(Duration::ZERO);
            walk.telemetry.sticky_freshness =
                Some(sticky::freshness_for(now_unix - binding.last_used_at));
        }
        None
    } else {
        bound
    };

    // 第一步：粘性命中时先按等待预算争取原目标（§10.3）。
    if let Some((candidate, binding, pinned)) = bound {
        walk.telemetry.sticky_hit = true;
        // 命中粘性即开始计这一项：即使目标当时有空位、一秒没等，也要写下 0
        // 而不是 null——"命中但没等"与"根本没命中"是两回事（§24.1）。
        walk.telemetry.sticky_wait = Some(Duration::ZERO);
        // 粘性等待同样不能超过分组的"队列最长等待"（§6.3）。
        let freshness = sticky::freshness_for(now_unix - binding.last_used_at);
        let budget = sticky::wait_budget(forward.request_bytes, now_unix - binding.last_used_at)
            .min(
                walk.queue_deadline
                    .saturating_duration_since(Instant::now()),
            );
        // 新鲜度系数记下来，解释"这次为什么愿意等/不愿意等"（§24.1）。
        walk.telemetry.sticky_freshness = Some(freshness);
        let outcome = walk
            .wait_and_run(
                &[candidate],
                budget,
                &sticky_key,
                true,
                pinned.as_deref(),
                binding.credential_digest.as_deref(),
            )
            .await;
        match outcome {
            Flow::Done(response) => return response,
            Flow::QueueFull => return walk.queue_full(),
            // 预算耗尽或原目标失败：降级为无粘性请求重新走层内选择，并重绑
            // 粘性（§10.3）。这也让"高并发时同一前缀自然分散到几个号"成为
            // 免费行为。
            Flow::Continue | Flow::Exhausted => {}
        }
    }

    // 第二步：逐层游走。层内先耗尽，再降层；层内全忙则在本层排队（§13.6）。
    for layer in &plan.layers {
        if walk.attempted.len() >= routing::MAX_TARGET_ATTEMPTS {
            break;
        }
        match walk.walk_layer(&layer.candidates, &sticky_key).await {
            Flow::Done(response) => return response,
            Flow::QueueFull => return walk.queue_full(),
            Flow::Exhausted => {
                return walk.fail(ErrorCode::QueueTimeout, "等待可用目标超时".into());
            }
            Flow::Continue => {}
        }
    }

    // 候选全部用尽。用最后一次失败的性质决定错误码，让客户端的重试行为正确。
    let (code, message) = walk.last.take().unwrap_or((
        ErrorCode::NoEligibleTarget,
        format!("逻辑模型 {} 当前没有可用的调度目标", forward.logical_model),
    ));
    walk.fail(code, message)
}

impl Walk<'_> {
    /// 在一层内游走：有空位的立即用，全部失败就降层，全忙则在本层排队。
    ///
    /// "忙"不会导致降层——你设 `A=100 B=50`，B 永远拿不到流量，除非 A 真的
    /// 不合格；并发满不是不合格（§9.2、§13.6）。
    async fn walk_layer(
        &mut self,
        candidates: &[routing::Candidate],
        sticky_key: &Option<(sticky::Key, sticky::Origin)>,
    ) -> Flow {
        let (lossless, degraded): (Vec<_>, Vec<_>) = candidates
            .iter()
            .partition(|candidate| candidate.is_lossless());

        // 先完整耗尽无损候选。无损候选只是忙并不算失败，此时不能绕过它们
        // 直接使用降级候选，否则“降级只在故障切换时生效”会被并发高峰打破。
        let busy = match self.try_candidates(lossless, sticky_key).await {
            Ok(busy) => busy,
            Err(flow) => return flow,
        };
        if !busy.is_empty() {
            let budget = self
                .queue_deadline
                .saturating_duration_since(Instant::now());
            match self
                .wait_and_run(&busy, budget, sticky_key, false, None, None)
                .await
            {
                Flow::Continue => {}
                other => return other,
            }
        }

        // 只有无损候选都失败后，才尝试白名单降级候选。
        let busy = match self.try_candidates(degraded, sticky_key).await {
            Ok(busy) => busy,
            Err(flow) => return flow,
        };
        if busy.is_empty() {
            return Flow::Continue;
        }
        let budget = self
            .queue_deadline
            .saturating_duration_since(Instant::now());
        match self
            .wait_and_run(&busy, budget, sticky_key, false, None, None)
            .await
        {
            Flow::Continue => Flow::Exhausted,
            other => other,
        }
    }

    /// 尝试一组同类候选，返回暂时忙的目标供本阶段排队。
    ///
    /// `Err` 里带的是给客户端的完整响应，故意不装箱：这条路径每次请求只走
    /// 一次，装箱反而多一次分配；clippy 的体积告警在这里按已知代价放行。
    #[allow(clippy::result_large_err)]
    async fn try_candidates<'a>(
        &mut self,
        candidates: Vec<&'a routing::Candidate>,
        sticky_key: &Option<(sticky::Key, sticky::Origin)>,
    ) -> Result<Vec<&'a routing::Candidate>, Flow> {
        let mut busy: Vec<&'a routing::Candidate> = Vec::new();
        for candidate in candidates {
            if self.attempted.len() >= routing::MAX_TARGET_ATTEMPTS {
                break;
            }
            // 这一轮还没有选定 Key，用"绑定/无绑定"这一维判断是否试过；
            // 同一目标换 Key 的重复在这里不会被误跳过（§4.2.1）。
            if self.already_tried(candidate, None) {
                continue;
            }
            if Instant::now() >= self.deadline {
                return Err(Flow::Done(
                    self.fail(ErrorCode::UpstreamTimeout, "请求已达到总超时".into()),
                ));
            }
            match self.select_credential(candidate, None, None, None) {
                Picked::Ready(credential) => match self.try_admit(candidate, credential.as_ref()) {
                    Ok(admission) => {
                        match self.run(candidate, admission, sticky_key, None, None).await {
                            Flow::Continue => {}
                            other => return Err(other),
                        }
                    }
                    Err(reason) if reason.is_queueable() => busy.push(candidate),
                    Err(reason) => self.note_unavailable(candidate, reason),
                },
                // 选不出 Key（全忙或一把能用的都没有）在本层排队等名额：
                // 与 §13.6 同一条规则，忙不等于坏。
                Picked::Busy => busy.push(candidate),
                // 没有可用 Key 是"坏"：这个候选本次彻底不可用。
                Picked::Unavailable(message) => {
                    self.last = Some((ErrorCode::NoEligibleTarget, message));
                }
            }
        }
        Ok(busy)
    }

    /// 在一组候选上等待名额；等到就带着名额做终检并发出请求。
    ///
    /// `budget` 是愿意等待的上限，同时受请求总超时约束。返回 `Continue`
    /// 表示候选都试过且都失败了，`Exhausted` 表示还在忙但预算已耗尽。
    ///
    /// `sticky` 打开时额外执行 §10.3 的 429 规则：上游给出的 `Retry-After`
    /// 不超过剩余预算就原地等待并重试同一个目标，而不是立刻换号——换号的
    /// 代价是整份前缀缓存重建。
    async fn wait_and_run(
        &mut self,
        candidates: &[&routing::Candidate],
        budget: Duration,
        sticky_key: &Option<(sticky::Key, sticky::Origin)>,
        sticky: bool,
        pinned: Option<&str>,
        preferred: Option<&str>,
    ) -> Flow {
        let mut pending: Vec<&routing::Candidate> = candidates
            .iter()
            .copied()
            .filter(|candidate| !self.already_tried(candidate, None))
            .collect();
        let started = Instant::now();
        // 只有真的要等，才占用分组的排队名额；一次等待只占一个。
        let mut ticket: Option<queue::QueueTicket> = None;
        // 429 后的原地重试每个目标只给一次，否则一个一直 429 的上游能把整个
        // 预算耗光。
        let mut retried_after_429 = false;
        // 关闭信号：置位后排队中的请求不再干等，立刻返回可重试错误（§25.3）。
        let mut shutdown = self.forward.state.runtime.subscribe_shutdown();

        loop {
            if self.forward.state.runtime.is_shutting_down() {
                return Flow::Done(self.fail(
                    ErrorCode::QueueTimeout,
                    "服务正在关闭，排队中的请求已取消，请稍后重试".into(),
                ));
            }
            // 先试一次不排队的准入：任一目标有空位就立即使用，根本不排队。
            let mut index = 0;
            while index < pending.len() {
                let candidate = pending[index];
                let credential = match self.select_credential(candidate, pinned, preferred, None) {
                    Picked::Ready(credential) => credential,
                    // 忙：留在 pending 里，稍后按信号量唤醒再试。
                    Picked::Busy => {
                        index += 1;
                        continue;
                    }
                    Picked::Unavailable(message) => {
                        self.last = Some((ErrorCode::NoEligibleTarget, message));
                        pending.remove(index);
                        continue;
                    }
                };
                match self.try_admit(candidate, credential.as_ref()) {
                    Ok(admission) => {
                        pending.remove(index);
                        match self
                            .run(candidate, admission, sticky_key, pinned, preferred)
                            .await
                        {
                            Flow::Continue => {
                                if sticky
                                    && !retried_after_429
                                    && let Some(wait) = self.retry_after_within(budget, started)
                                {
                                    // 原地等待也是排队，同样占用分组的总容量。
                                    if ticket.is_none() {
                                        match self.enter_queue() {
                                            Some(entered) => ticket = Some(entered),
                                            None => return Flow::QueueFull,
                                        }
                                    }
                                    retried_after_429 = true;
                                    self.wait_out_retry_after(wait).await;
                                    self.forget_attempt(candidate);
                                    pending.insert(index, candidate);
                                }
                            }
                            other => return other,
                        }
                    }
                    Err(reason) if reason.is_queueable() => index += 1,
                    Err(reason) => {
                        self.note_unavailable(candidate, reason);
                        pending.remove(index);
                    }
                }
            }
            if pending.is_empty() {
                return Flow::Continue;
            }

            let remaining = routing::clamp_wait(
                budget.saturating_sub(started.elapsed()),
                self.deadline.saturating_duration_since(Instant::now()),
            );
            if remaining.is_zero() {
                return Flow::Exhausted;
            }
            if ticket.is_none() {
                match self.enter_queue() {
                    Some(entered) => ticket = Some(entered),
                    None => return Flow::QueueFull,
                }
            }

            // 并发名额靠信号量唤醒；RPM / TPM 窗口只会随时间推移释放，没有可
            // 等的信号，只能按短间隔轮询。
            let rate_limited = pending.iter().any(|candidate| {
                let credential = match self.select_credential(candidate, pinned, preferred, None) {
                    Picked::Ready(credential) => credential,
                    // 选不出 Key 时按"限流"处理：RPM / TPM 窗口只会随时间释放，
                    // 用短轮询比干等到底更早恢复。
                    Picked::Busy | Picked::Unavailable(_) => None,
                };
                matches!(
                    self.check(candidate, credential.as_ref()),
                    Err(health::Unavailable::RateLimited)
                )
            });
            let slice = if rate_limited {
                remaining.min(RATE_POLL_INTERVAL)
            } else {
                remaining
            };
            let capacities: Vec<_> = pending
                .iter()
                .map(|candidate| {
                    let caller = health::Caller {
                        account_id: &candidate.target.account.id,
                        key_id: None,
                        target_id: &candidate.target.target.id,
                    };
                    match self.select_credential(candidate, pinned, preferred, None) {
                        Picked::Ready(credential) => {
                            let caller = health::Caller {
                                key_id: credential.as_ref().map(|key| key.id.as_str()),
                                ..caller
                            };
                            self.forward
                                .state
                                .runtime
                                .health
                                .capacity(caller, admission_limits(candidate, credential.as_ref()))
                        }
                        // 还没选定 Key：必须能等到**任意一把** Key 的名额释放，
                        // 否则"两把都满"会等一个永远不会到来的唤醒。
                        Picked::Busy | Picked::Unavailable(_) => {
                            let pool = self.forward.state.runtime.credentials.current();
                            self.forward.state.runtime.health.capacity_any_key(
                                caller,
                                admission_limits(candidate, None),
                                pool.keys_of(&candidate.target.account.id),
                            )
                        }
                    }
                })
                .collect();

            let waited = Instant::now();
            let outcome = queue::wait_for_any_capacity(capacities, slice, &mut shutdown).await;

            let elapsed = waited.elapsed();
            // 粘性路径上的等待记到 sticky_wait，其余记到普通排队（§6.6、§24.1）。
            // 两者分开才能回答"这次请求为前缀缓存等了多久"。
            if sticky {
                let total = self.telemetry.sticky_wait.unwrap_or_default() + elapsed;
                self.telemetry.sticky_wait = Some(total);
            } else {
                self.telemetry.queued += elapsed;
            }

            if let queue::CapacityWaitOutcome::ShuttingDown = outcome {
                return Flow::Done(self.fail(
                    ErrorCode::QueueTimeout,
                    "服务正在关闭，排队中的请求已取消，请稍后重试".into(),
                ));
            }
            if let queue::CapacityWaitOutcome::Ready(index, permit) = outcome {
                let candidate = pending[index];
                // 被唤醒后重新选一次 Key 并做完整终检：等待期间倍率、配置或
                // Key 的健康状态都可能已经变了（§13.6）。
                let credential = match self.select_credential(candidate, pinned, preferred, None) {
                    Picked::Ready(credential) => credential,
                    // 忙：名额已经到手，先还回去，回到等待循环。
                    Picked::Busy => continue,
                    Picked::Unavailable(message) => {
                        self.last = Some((ErrorCode::NoEligibleTarget, message));
                        pending.remove(index);
                        continue;
                    }
                };
                match self.admit_with_permit(candidate, credential.as_ref(), permit) {
                    Ok(admission) => {
                        pending.remove(index);
                        match self
                            .run(candidate, admission, sticky_key, pinned, preferred)
                            .await
                        {
                            Flow::Continue => {}
                            other => return other,
                        }
                    }
                    Err(reason) if reason.is_queueable() => {}
                    Err(reason) => {
                        self.note_unavailable(candidate, reason);
                        pending.remove(index);
                    }
                }
            }
        }
    }

    /// 选出这次调用要用的 Key（§4.2.1）。
    ///
    /// `pinned` 是响应链钉住的凭据摘要（最强约束）；`sticky` 是"已经有并发名额、
    /// 不想在最后一步换 Key 打碎缓存"时的偏好。真正的可用性由
    /// [`crate::credential::select_key`] 结合健康状态判断。
    fn select_credential(
        &self,
        candidate: &routing::Candidate,
        pinned: Option<&str>,
        sticky: Option<&str>,
        excluded: Option<&str>,
    ) -> Picked {
        let account = &candidate.target.account;
        let pool = self.forward.state.runtime.credentials.current();
        let keys = pool.keys_of(&account.id);
        let health = &self.forward.state.runtime.health;
        // 账号与目标两级的额度先校到配置值：这里只做资格判断、不校准的话，会
        // 拿着一个"1<<20 个名额"的信号量把"已满"看成"有空位"，排队与限流就全
        // 失效了（§13.6、§17.1）。Key 级由下面的 reconcile_and_check 负责。
        health
            .account(&account.id)
            .reconcile_capacity(account.limits.max_concurrency);
        health
            .target(&candidate.target.target.id)
            .reconcile_capacity(candidate.target.target.limits.max_concurrency);
        // Key 状态的时钟是 tokio 的：测试用 `tokio::time::pause` 精确推进冷却。
        let now = tokio::time::Instant::now();
        // 软亲和在这里统一生效：调用方不必各自传递。它只是一条**偏好**，
        // 排在响应链钉住之后、随机抽签之前（§4.2.1、§10.1 修订）。
        let sticky = sticky.or(self.soft_affinity.as_deref());
        let choice = crate::credential::select_key(
            keys,
            pinned,
            sticky,
            excluded,
            |key| {
                let state = health.key(&crate::credential::credential_id(
                    &key.account_id,
                    &key.credential_digest,
                ));
                match state.reconcile_and_check(key.limits, now) {
                    Ok(()) => Ok(true),
                    Err(reason) if reason.is_queueable() => Err(()),
                    Err(_) => Ok(false),
                }
            },
            &mut score::random_unit,
        );
        match choice {
            crate::credential::KeyChoice::Ready(Some(key)) => Picked::Ready(Some(Arc::clone(key))),
            crate::credential::KeyChoice::Ready(None) => Picked::Unavailable(format!(
                "账号「{}」没有可用的 API Key（可能未配置、已停用或已全部失效）",
                account.name
            )),
            // 全部在忙不是失败：交给上层的排队路径，与 §13.6 同一条规则。
            crate::credential::KeyChoice::Busy => Picked::Busy,
        }
    }

    /// 发起一次尝试并把结果同时喂给健康状态与性能统计。
    ///
    /// 只有 `Continue` 表示"可以换下一个"；其余情况都已经有了最终响应。
    ///
    /// 凭据级失败（401/403、额度耗尽）会在**同一个目标内换成另一把 Key** 重试，
    /// 换 Key 次数受 [`MAX_KEY_SWITCHES_PER_TARGET`] 限制（§4.2.1）。
    async fn run(
        &mut self,
        candidate: &routing::Candidate,
        admission: health::Admission,
        sticky_key: &Option<(sticky::Key, sticky::Origin)>,
        pinned: Option<&str>,
        preferred: Option<&str>,
    ) -> Flow {
        if let Err((code, message)) = self.recheck_multiplier(candidate) {
            // 倍率终检发生在准入之后；终检失败时本次请求从未发给上游，
            // 预扣的 RPM/TPM 必须完整退回，且不能留下半开试运行占用。
            admission.cancel_before_upstream();
            self.last = Some((code, message));
            return Flow::Continue;
        }
        // 这次调用用哪把 Key。选到之后**整个请求内不再改变**，除非换 Key 重试
        // 显式发生——这就是"一次下游调用只使用一把 Key"（不变量 A）。
        //
        // 所有 Key 都只是"忙"时把名额还回去：调用方（`try_candidates` /
        // `wait_and_run`）会把这个候选放进排队集合，等到有名额再回来。在这里
        // 直接放弃会让"两把 Key 都满"变成一个立刻失败的账号，而不是等一会儿
        // 就能用的账号（§13.6）。
        let first = match self.select_credential(candidate, pinned, preferred, None) {
            Picked::Ready(credential) => credential,
            Picked::Busy => {
                admission.cancel_before_upstream();
                return Flow::Continue;
            }
            Picked::Unavailable(message) => {
                admission.cancel_before_upstream();
                self.last = Some((ErrorCode::NoEligibleTarget, message));
                return Flow::Continue;
            }
        };
        // 粘性/钉住命中的那把要跨重试保留：换 Key 只发生在凭据真的被拒绝之后。
        let mut digest = first.as_ref().map(|key| key.credential_digest.clone());
        let mut credential = first;
        let mut admission = Some(admission);
        let mut switches = 0usize;

        loop {
            let Some(current_admission) = admission.take() else {
                break;
            };
            match self
                .run_attempt(
                    candidate,
                    current_admission,
                    sticky_key,
                    credential.clone(),
                    digest.clone(),
                )
                .await
            {
                Attempted::Done(flow) => return flow,
                Attempted::CredentialFailed(failed) => {
                    // 先把这次失败结算到**那一把** Key 上，否则下一轮抽签会
                    // 再次选中它，换 Key 就变成了空转。
                    failed.settle(bad_key_outcome(self.last_retry_after), None);
                    if switches >= MAX_KEY_SWITCHES_PER_TARGET {
                        // 换 Key 次数用尽：当作这个目标不可用，交给上层换目标。
                        return Flow::Continue;
                    }
                    switches += 1;
                    let Some(next) = self.choose_next_credential(candidate, digest.as_deref())
                    else {
                        return Flow::Continue;
                    };
                    match self.try_admit(candidate, Some(&next)) {
                        Ok(next_admission) => {
                            digest = Some(next.credential_digest.clone());
                            credential = Some(next);
                            admission = Some(next_admission);
                        }
                        // 新 Key 在忙：这次请求换目标，不原地排队——已经打过一次
                        // 上游了，再等下去不如让别的账号接。
                        Err(_) => return Flow::Continue,
                    }
                }
            }
        }
        Flow::Continue
    }

    /// 换 Key 时挑下一把。
    ///
    /// 刚刚失败的那把已经被结算成"失效"或"额度冷却"，因此会被
    /// [`Self::select_credential`] 自然跳过——不需要在这里显式排除。
    fn choose_next_credential(
        &self,
        candidate: &routing::Candidate,
        just_failed: Option<&str>,
    ) -> Option<Arc<crate::credential::Credential>> {
        // 把刚刚失败的那把显式排除掉：它可能还没被标记成"坏"（上游的拒绝方式
        // 不构成 KeyInvalid），但这一次调用已经用它打失败过（§4.2.1）。
        let picked = self.select_credential(candidate, None, None, just_failed);
        match picked {
            Picked::Ready(Some(next)) => Some(next),
            _ => None,
        }
    }

    /// 带着一把已经选定（且已准入）的 Key 发出一次尝试。
    async fn run_attempt(
        &mut self,
        candidate: &routing::Candidate,
        admission: health::Admission,
        sticky_key: &Option<(sticky::Key, sticky::Origin)>,
        credential: Option<Arc<crate::credential::Credential>>,
        digest: Option<String>,
    ) -> Attempted {
        self.attempted
            .push((candidate.target.target.id.clone(), digest.clone()));
        self.telemetry.attempts += 1;
        self.telemetry.effective = Some(candidate.multiplier);
        self.telemetry.key_id = credential.as_ref().map(|key| key.id.clone());
        self.telemetry.key_label = credential.as_ref().map(|key| key.label.clone());

        let started = Instant::now();
        let result = self.walk_endpoints(candidate, credential.as_ref()).await;

        let dimension = score::Dimension {
            protocol: self.forward.endpoint.protocol(),
            streaming: self.streaming,
        };
        match result {
            Ok(success) => {
                if let Some((key, _)) = sticky_key {
                    // 绑定记的是"目标 + Key"：只绑目标会在下次请求时重新抽签，
                    // 把上游按凭据隔离的前缀缓存打碎（§4.2.1 的不变量 B）。
                    self.forward.state.runtime.sticky.bind(
                        key.clone(),
                        &self.forward.group.group.id,
                        &self.forward.logical_model,
                        &candidate.target.target.id,
                        digest.as_deref(),
                        self.now_unix,
                    );
                }
                if self.streaming {
                    // 流式：TPM 回补、性能 EWMA、熔断结果、请求记录与 Responses
                    // 状态链全都等流真正结束再结算（§26.3）。提交时记的那份
                    // 只是"首字延迟"，用它冒充总耗时会系统性高估吞吐。
                    self.note_attempt(candidate, started, "ok", None, false);
                    let record = self.record_for(
                        Some(candidate),
                        success.status,
                        None,
                        Some(success.status),
                    );
                    let responses = success
                        .stream_state
                        .map(|seed| settle::ResponsesCompletion {
                            state: self.forward.state.clone(),
                            chain: self.forward.chain.clone(),
                            pending: responses::PendingState {
                                gateway_id: seed.gateway_id,
                                upstream_id: seed.upstream_id,
                                account_id: Some(candidate.target.account.id.clone()),
                                target_id: Some(candidate.target.target.id.clone()),
                                endpoint: Some(seed.endpoint),
                            },
                            entry_body: self.forward.body.clone(),
                            entry_protocol: self.forward.endpoint.protocol(),
                            retention_days: self.forward.state.settings.get().response_state_days,
                        });
                    Attempted::Done(Flow::Done(settle::settle_stream(
                        success.response,
                        settle::StreamSettlement {
                            state: self.forward.state.clone(),
                            protocol: self.forward.endpoint.protocol(),
                            target_id: candidate.target.target.id.clone(),
                            dimension,
                            started,
                            request_started: self.forward.started_at,
                            first_token: success.first_token,
                            // 首字节：排队的 20 秒也会体现在评分里，而不是只记
                            // 上游吐第一个事件用掉的那 1 毫秒（§9.3）。
                            // 一个字节都没送出去时留空，让记录如实显示"流中断"
                            // 而不是"0 毫秒就出字了"。
                            first_byte: success.first_byte.or(success.first_token),
                            record,
                            admission: Some(admission),
                            responses,
                            degraded: success.degraded.unwrap_or_else(translate::degradation_sink),
                        },
                    )))
                } else {
                    self.usage_parts = success.usage_parts;
                    self.usage_detail = success.usage_detail;
                    self.note_attempt(candidate, started, "ok", None, false);
                    admission.settle(health::Outcome::Success, success.usage_tokens);
                    self.forward.state.runtime.perf.observe(
                        &candidate.target.target.id,
                        dimension,
                        &score::Sample {
                            success: true,
                            // 走到这里的都是真正成功的非流式响应，必须进统计。
                            counts: true,
                            // 用客户端体感的首字节时间，而不是"响应头之后到首个
                            // 语义事件"那一段：上游先憋响应头时后者接近 0（§9.3）。
                            // 非流式没有首字，退回 None，由总耗时代言。
                            first_token: success.first_byte.or(success.first_token),
                            total: started.elapsed(),
                            output_tokens: success.output_tokens,
                        },
                        self.now_unix,
                    );
                    Attempted::Done(Flow::Done(self.finish(
                        Some(candidate),
                        success.status,
                        None,
                        Some(success.status),
                        success.response,
                    )))
                }
            }
            Err(AttemptFailure::Terminal(response)) => {
                admission.settle(health::Outcome::Neutral, None);
                let status = response.status();
                self.note_attempt(candidate, started, "failed", None, true);
                Attempted::Done(Flow::Done(self.finish(
                    Some(candidate),
                    status,
                    None,
                    None,
                    *response,
                )))
            }
            // `walk_endpoints` 已经把端点耗尽翻译成了可切换失败。
            Err(AttemptFailure::MissingEndpoint) => {
                admission.settle(health::Outcome::Neutral, None);
                // 走错门是廉价失败：不计入尝试预算（§13.1）。
                self.note_attempt(candidate, started, "missing_endpoint", None, false);
                Attempted::Done(Flow::Continue)
            }
            Err(AttemptFailure::Switchable {
                code,
                message,
                upstream_status,
                retry_after,
            }) => {
                let outcome = classify_outcome(code, upstream_status, retry_after);
                // 凭据级失败：账号内还有别的 Key 时应当换一把重试，而不是
                // 立刻放弃这个账号（§4.2.1）。这时的 admission 原样交回调用方，
                // 由它把失败结算到**那一把** Key 上——在这里结算会把状态记到
                // 一个已经失败、即将被丢弃的准入上。
                if matches!(outcome, health::Outcome::KeyInvalid)
                    || matches!(outcome, health::Outcome::QuotaExhausted { .. })
                {
                    self.last_retry_after = retry_after;
                    self.last = Some((code, message));
                    tracing::warn!(
                        request_id = self.forward.request_id,
                        target = candidate.target.target.id,
                        account = candidate.target.account.name,
                        key = self.telemetry.key_label.as_deref().unwrap_or("-"),
                        upstream_status = upstream_status.map(|s| s.as_u16()),
                        error_code = code.as_str(),
                        "凭据被上游拒绝，尝试账号内换一把 Key"
                    );
                    return Attempted::CredentialFailed(admission);
                }
                admission.settle(outcome, None);
                self.last_retry_after = retry_after;
                // 连接都没建立、上游没给状态码的 exhausted 属于廉价失败，
                // 不计入尝试预算（§13.1）。
                let cheap = upstream_status.is_none() && code == ErrorCode::UpstreamExhausted;
                self.note_attempt(candidate, started, "failed", Some(code), !cheap);
                // 慢到超时不熔断，但可靠性得分必须反映它（§12.1）。
                let counts_for_perf = !matches!(outcome, health::Outcome::Neutral)
                    || code == ErrorCode::UpstreamTimeout;
                if counts_for_perf {
                    self.forward.state.runtime.perf.observe(
                        &candidate.target.target.id,
                        dimension,
                        &score::Sample {
                            success: false,
                            // 走到这里已经排除了"与目标健康无关"的中性失败，
                            // 所以这一次必须反映在可靠性上（§12.1）。
                            counts: true,
                            first_token: None,
                            total: started.elapsed(),
                            output_tokens: None,
                        },
                        self.now_unix,
                    );
                }
                tracing::warn!(
                    request_id = self.forward.request_id,
                    target = candidate.target.target.id,
                    account = candidate.target.account.name,
                    upstream_status = upstream_status.map(|s| s.as_u16()),
                    error_code = code.as_str(),
                    "目标尝试失败，切换到下一个候选"
                );
                self.last = Some((code, message));
                Attempted::Done(Flow::Continue)
            }
        }
    }

    /// 按 §14.3 的顺序试这个目标的端点。
    ///
    /// 路由不存在只是"走错了门"，不是这个目标坏了：记下证据，重新排一次端点
    /// 顺序继续试同一个账号。全部端点都不存在时才把它当作这个目标的失败。
    async fn walk_endpoints(
        &mut self,
        candidate: &routing::Candidate,
        credential: Option<&Arc<crate::credential::Credential>>,
    ) -> Result<Success, AttemptFailure> {
        let mut plan = candidate.endpoints.clone();

        for _ in 0..endpoints::MAX_ENDPOINTS_PER_TARGET {
            let Some(choice) = plan.first().cloned() else {
                break;
            };
            let prepared = self.prepare(candidate, &choice).await?;
            let timeout = self.deadline.saturating_duration_since(Instant::now());
            self.telemetry.endpoint = Some(prepared.endpoint);
            self.telemetry.degraded = prepared.degraded.clone();

            match attempt(
                self.forward,
                &candidate.target,
                credential,
                &prepared,
                self.streaming,
                timeout,
            )
            .await
            {
                Err(AttemptFailure::MissingEndpoint) => {
                    self.forward.state.runtime.evidence.note_unsupported(
                        &candidate.target.account.id,
                        prepared.endpoint,
                        Instant::now(),
                    );
                    tracing::info!(
                        account = candidate.target.account.name,
                        endpoint = prepared.endpoint.as_str(),
                        "上游没有这个端点，改走转换后的端点"
                    );
                    // 辅助端点（count_tokens / compact / input_tokens）没有
                    // 转换备胎：上游确实没有这条路由时，按 §15.4 明确告诉
                    // 客户端不支持，而不是报一个可重试的 503 让它反复重试。
                    if self.forward.endpoint.is_native_only() {
                        return Err(AttemptFailure::Terminal(Box::new(
                            GatewayError::new(
                                ErrorCode::UnsupportedParameter,
                                format!(
                                    "账号「{}」没有 /{} 端点",
                                    candidate.target.account.name,
                                    self.forward.endpoint.path()
                                ),
                            )
                            .with_protocol(self.forward.endpoint.protocol())
                            .with_request_id(self.forward.request_id)
                            .into_response(),
                        )));
                    }
                    // 证据变了，端点顺序要重排：刚证实缺失的那个会被排除掉。
                    plan = self.endpoint_plan(candidate).unwrap_or_default();
                }
                other => return other,
            }
            if Instant::now() >= self.deadline {
                break;
            }
        }

        Err(AttemptFailure::switchable(
            ErrorCode::UpstreamExhausted,
            format!(
                "账号「{}」没有可用于本次请求的端点",
                candidate.target.account.name
            ),
        ))
    }

    /// 用最新的能力证据重排这个目标的端点顺序。
    fn endpoint_plan(&self, candidate: &routing::Candidate) -> Option<Vec<Choice>> {
        endpoints::choices(
            &candidate.target.account,
            self.forward.endpoint,
            self.translation,
            &self.forward.state.runtime.evidence,
            self.forward.group.group.allow_degrade,
            Instant::now(),
        )
        .ok()
    }

    /// 为一次尝试准备请求体：状态链收尾 + 跨协议转换 + 模型名改写。
    ///
    /// 原生续链（回到原账号的原生 Responses 端点）把引用改写成上游 ID；
    /// 其余候选走可重放的合并体。既续不了链又没有合并体，说明这次引用
    /// 无法被该候选满足——换下一个（§15.2）。
    async fn prepare(
        &self,
        candidate: &routing::Candidate,
        choice: &Choice,
    ) -> Result<Prepared, AttemptFailure> {
        let target_protocol = choice.endpoint.protocol();
        let downstream = self.forward.endpoint.protocol();
        let chain = &self.forward.chain;

        // multipart 请求没有可用的 JSON 转换路径：只替换当前目标的 model
        // part，其余 boundary、文件头与文件字节全部保持不动。
        if let Some(raw) = self.forward.raw.as_ref() {
            let rewritten =
                rewrite_multipart_model(&raw.bytes, &candidate.target.target.upstream_model)
                    .map_err(|error| {
                        let message = match error {
                            MultipartModelError::Missing => "multipart 请求体缺少 model 字段",
                            MultipartModelError::UnsafeReplacement => {
                                "上游模型名包含 multipart 不安全字符"
                            }
                        };
                        AttemptFailure::Terminal(Box::new(
                            GatewayError::new(ErrorCode::UnsupportedParameter, message)
                                .with_protocol(self.forward.endpoint.protocol())
                                .with_request_id(self.forward.request_id)
                                .into_response(),
                        ))
                    })?;
            return Ok(Prepared {
                endpoint: choice.endpoint,
                body: self.forward.body.clone(),
                raw: Some(rewritten),
                degraded: Vec::new(),
            });
        }

        let native_continuation = matches!(&chain.pinned, Some(pinned)
            if pinned.account_id == candidate.target.account.id
                && downstream == target_protocol);
        let mut degraded: Vec<String> = Vec::new();
        let expired = || {
            AttemptFailure::switchable(
                ErrorCode::ResponseStateExpired,
                format!(
                    "响应状态 {} 不存在或已过期，无法继续会话",
                    chain.reference.as_deref().unwrap_or("")
                ),
            )
        };

        let mut body = if native_continuation {
            let pinned = chain.pinned.as_ref().expect("native_continuation 已判定");
            let mut body = self.forward.body.clone();
            responses::rewrite_reference(&mut body, &pinned.upstream_id);
            body
        } else if chain.reference.is_some() && chain.merged.is_none() {
            // 带着引用却既不能原生续链也没有正文：不能装作新对话。
            return Err(expired());
        } else {
            let emitted = self.translation.emit(target_protocol).map_err(|reason| {
                // 资格过滤已经排除过这种情况；真走到这里说明请求本身表达不了。
                AttemptFailure::Terminal(Box::new(
                    GatewayError::new(ErrorCode::UnsupportedParameter, reason.to_string())
                        .with_protocol(self.forward.endpoint.protocol())
                        .with_request_id(self.forward.request_id)
                        .into_response(),
                ))
            })?;
            degraded = emitted.degraded.clone();
            emitted.body.clone()
        };
        rewrite_model(&mut body, &candidate.target.target.upstream_model);
        Ok(Prepared {
            endpoint: choice.endpoint,
            body,
            raw: None,
            degraded,
        })
    }

    /// 占一个分组排队名额；容量已满时返回 `None`（§13.6）。
    fn enter_queue(&self) -> Option<queue::QueueTicket> {
        let group = &self.forward.group.group;
        self.forward
            .state
            .runtime
            .queues
            .enter(&group.id, group.queue_capacity)
    }

    /// 最后一次失败若是带 `Retry-After` 的 429，且等得起，就返回要等多久。
    fn retry_after_within(&mut self, budget: Duration, started: Instant) -> Option<Duration> {
        let wait = self.last_retry_after.take()?;
        let remaining = routing::clamp_wait(
            budget.saturating_sub(started.elapsed()),
            self.deadline.saturating_duration_since(Instant::now()),
        );
        (wait.saturating_add(RETRY_AFTER_MARGIN) <= remaining).then_some(wait)
    }

    /// 原地等过上游要求的恢复时间；多等一点点，保证冷却确实已经结束。
    async fn wait_out_retry_after(&mut self, wait: Duration) {
        let waited = Instant::now();
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        let sleep_for = wait.saturating_add(RETRY_AFTER_MARGIN).min(remaining);
        tokio::time::sleep(sleep_for).await;
        self.telemetry.queued += waited.elapsed();
    }

    /// 允许一个目标被再试一次。只用于 §10.3 的粘性 429 重试。
    ///
    /// 按**目标**清，不按 (目标, Key) 清：粘性 429 重试的语义是"等一会儿再打
    /// 同一个目标"，那把 Key 若已进入限流冷却，下一轮会自然换到别的 Key
    /// （§4.2.1）。
    fn forget_attempt(&mut self, candidate: &routing::Candidate) {
        self.attempted
            .retain(|(target, _)| target != &candidate.target.target.id);
    }

    /// 这个候选与这把 Key 的组合是否已经试过。
    ///
    /// `credential` 为 `None` 表示"这一轮还没选 Key"，此时只按目标判断——同一个
    /// 目标与**任何** Key 的组合都不再重复尝试。换 Key 重试走的是另一条路径：
    /// 它带着具体的凭据摘要进来，因此 (`target`, 新 Key) 仍然是"没试过"。
    fn already_tried(&self, candidate: &routing::Candidate, credential: Option<&str>) -> bool {
        self.attempted.iter().any(|(target, key)| {
            target == &candidate.target.target.id
                && (credential.is_none() || key.as_deref() == credential)
        })
    }

    fn try_admit(
        &self,
        candidate: &routing::Candidate,
        credential: Option<&Arc<crate::credential::Credential>>,
    ) -> Result<health::Admission, health::Unavailable> {
        let caller = admission_caller(candidate, credential);
        self.forward.state.runtime.health.try_admit(
            caller,
            admission_limits(candidate, credential),
            self.estimated_tokens,
        )
    }

    fn admit_with_permit(
        &self,
        candidate: &routing::Candidate,
        credential: Option<&Arc<crate::credential::Credential>>,
        permit: health::CapacityPermit,
    ) -> Result<health::Admission, health::Unavailable> {
        let caller = admission_caller(candidate, credential);
        self.forward.state.runtime.health.admit_with_permit(
            caller,
            admission_limits(candidate, credential),
            self.estimated_tokens,
            permit,
        )
    }

    fn check(
        &self,
        candidate: &routing::Candidate,
        credential: Option<&Arc<crate::credential::Credential>>,
    ) -> Result<(), health::Unavailable> {
        let caller = admission_caller(candidate, credential);
        self.forward
            .state
            .runtime
            .health
            .check(caller, admission_limits(candidate, credential))
    }

    fn note_unavailable(&mut self, candidate: &routing::Candidate, reason: health::Unavailable) {
        self.last = Some((
            unavailable_code(reason),
            format!(
                "账号「{}」当前不可用：{}",
                candidate.target.account.name,
                reason.as_str()
            ),
        ));
    }

    /// 真正发出请求前的倍率终检（§11.5、§13.1）。
    fn recheck_multiplier(
        &self,
        candidate: &routing::Candidate,
    ) -> Result<(), (ErrorCode, String)> {
        let limit = self.forward.group.group.multiplier_limit;
        let effective = self.forward.state.runtime.multipliers.view().effective(
            &candidate.target.account,
            limit,
            crate::storage::now_unix(),
        );
        if !effective.status.is_usable() {
            return Err((
                ErrorCode::MultiplierUnknown,
                format!(
                    "账号「{}」的倍率已超过宽限期",
                    candidate.target.account.name
                ),
            ));
        }
        if effective.value > limit {
            return Err((
                ErrorCode::MultiplierExceeded,
                format!(
                    "账号「{}」的有效倍率已超过分组上限",
                    candidate.target.account.name
                ),
            ));
        }
        Ok(())
    }

    fn queue_full(&self) -> Response {
        self.fail(
            ErrorCode::QueueFull,
            format!("分组「{}」的排队总容量已满", self.forward.group.group.name),
        )
    }

    /// 生成网关错误响应并记录元数据。
    fn fail(&self, code: ErrorCode, message: String) -> Response {
        let error = GatewayError::new(code, message)
            .with_protocol(self.forward.endpoint.protocol())
            .with_request_id(self.forward.request_id);
        let status = error.code.status();
        self.finish(None, status, Some(code), None, error.into_response())
    }

    /// 统一出口：写一条请求元数据后返回响应。
    fn finish(
        &self,
        candidate: Option<&routing::Candidate>,
        status: StatusCode,
        error_code: Option<ErrorCode>,
        upstream_status: Option<StatusCode>,
        response: Response,
    ) -> Response {
        self.forward.state.recorder.record(self.record_for(
            candidate,
            status,
            error_code,
            upstream_status,
        ));
        response
    }

    /// 组装一条请求记录。流式请求由 [`settle::StreamSettlement`] 在流结束后
    /// 调用同一个构造函数，再补上真实耗时与流内错误码。
    fn record_for(
        &self,
        candidate: Option<&routing::Candidate>,
        status: StatusCode,
        error_code: Option<ErrorCode>,
        upstream_status: Option<StatusCode>,
    ) -> RequestRecord {
        let forward = self.forward;
        let target = candidate.map(|c| &c.target);
        RequestRecord {
            request_id: forward.request_id.to_string(),
            started_at: forward.started_unix,
            duration_ms: forward.started_at.elapsed().as_millis() as i64,
            protocol: forward.endpoint.protocol(),
            streaming: self.streaming,
            group_id: Some(forward.group.group.id.clone()),
            logical_model: Some(forward.logical_model.clone()),
            target_id: target.map(|t| t.target.id.clone()),
            account_id: target.map(|t| t.account.id.clone()),
            upstream_model: target.map(|t| t.target.upstream_model.clone()),
            request_bytes: forward.request_bytes as i64,
            upstream_status: upstream_status.map(|s| s.as_u16() as i64),
            http_status: status.as_u16() as i64,
            error_code: error_code.map(|c| c.as_str().to_string()),
            endpoint: self.telemetry.endpoint.map(|e| e.as_str().to_string()),
            degraded: (!self.telemetry.degraded.is_empty())
                .then(|| degrade::header_value(&self.telemetry.degraded)),
            effective_multiplier: candidate.map(|c| c.multiplier).or(self.telemetry.effective),
            cheapest_multiplier: self.telemetry.cheapest,
            dearest_multiplier: self.telemetry.dearest,
            attempts: self.telemetry.attempts,
            queued_ms: self.telemetry.queued.as_millis() as i64,
            sticky_hit: self.telemetry.sticky_hit,
            first_token_ms: None,
            input_tokens: self.usage_parts.0.map(|value| value as i64),
            output_tokens: self.usage_parts.1.map(|value| value as i64),
            config_version: Some(forward.state.config.current().version as i64),
            sticky_wait_ms: self.telemetry.sticky_wait.map(|d| d.as_millis() as i64),
            sticky_freshness: self.telemetry.sticky_freshness,
            output_tps: self.output_tps(),
            // Token 细分只有拿到 usage 才知道；流式在结算时补写（§11.6）。
            cache_read_tokens: self
                .usage_detail
                .and_then(|usage| usage.cache_read)
                .map(|value| value as i64),
            cache_write_tokens: self
                .usage_detail
                .and_then(|usage| usage.cache_write)
                .map(|value| value as i64),
            reasoning_tokens: self
                .usage_detail
                .and_then(|usage| usage.reasoning)
                .map(|value| value as i64),
            multiplier_source: self.telemetry.multiplier_source.map(str::to_string),
            // 额度状态直接读当时的健康注册表：它解释"这次为什么被拦或放行"（§24.1）。
            quota_status: self.quota_status(candidate),
            filter_summary: self.telemetry.filter_summary.clone(),
            selected_layer: self.telemetry.selected_layer,
            attempts_detail: self.attempt_log.clone(),
        }
    }

    /// 本次请求涉及的账号/目标在记录时刻的额度状态（§24.1）。
    ///
    /// 没有候选（例如在鉴权或模型解析阶段就失败）时给 `None`，不编造状态。
    fn quota_status(&self, candidate: Option<&routing::Candidate>) -> Option<String> {
        let target = candidate?.target.as_ref();
        let account = self
            .forward
            .state
            .runtime
            .health
            .account(&target.account.id);
        let state = self.forward.state.runtime.health.target(&target.target.id);
        // 带上这次实际使用的那把 Key 的状态：多 Key 账号里"账号正常但某把
        // Key 额度耗尽"是最常见的一种，只看目标状态会把原因丢掉（§24.1）。
        let pool = self.forward.state.runtime.credentials.current();
        let key = self.telemetry.key_id.as_deref().and_then(|key_id| {
            let credential = pool.by_id(&target.account.id, key_id)?;
            Some(
                self.forward
                    .state
                    .runtime
                    .health
                    .key(&crate::credential::credential_id(
                        &credential.account_id,
                        &credential.credential_digest,
                    )),
            )
        });
        Some(state.status(&account, key.as_deref()).as_str().to_string())
    }

    /// 输出速度（token/秒）。只有拿到输出 Token 与真实总耗时才算得出；
    /// 拿不到就给 `None`，绝不估算（§6.8 的同一口径）。
    fn output_tps(&self) -> Option<f64> {
        let output = self.usage_parts.1? as f64;
        let elapsed = self.forward.started_at.elapsed().as_secs_f64();
        (elapsed > 0.0 && output > 0.0).then(|| output / elapsed)
    }

    /// 记一次上游尝试的明细（§6.6）。`endpoint` 取当前尝试真正用到的端点。
    fn note_attempt(
        &mut self,
        candidate: &routing::Candidate,
        started: Instant,
        outcome: &str,
        error_code: Option<ErrorCode>,
        counts_against_budget: bool,
    ) {
        self.attempt_log.push(AttemptRecord {
            seq: self.attempt_log.len() as i64 + 1,
            target_id: Some(candidate.target.target.id.clone()),
            account_id: Some(candidate.target.account.id.clone()),
            upstream_model: Some(candidate.target.target.upstream_model.clone()),
            endpoint: self.telemetry.endpoint.map(|e| e.as_str().to_string()),
            started_at: crate::storage::now_unix(),
            duration_ms: started.elapsed().as_millis() as i64,
            outcome: outcome.to_string(),
            error_code: error_code.map(|code| code.as_str().to_string()),
            counts_against_budget,
        });
        // 成功的尝试就是这个端点"已确认支持"的证据（§14.3 第 1、4 档）。
        // 只认真正服务过的：发出去被拒绝不构成支持证据。
        if outcome == "ok"
            && let Some(endpoint) = self.telemetry.endpoint
        {
            self.forward.state.runtime.evidence.note_supported(
                &candidate.target.account.id,
                endpoint,
                Instant::now(),
            );
        }
    }
}

/// 一次成功尝试的产物。
struct Success {
    status: StatusCode,
    response: Response,
    /// 首个**语义**事件的距离，用于评分（§9.3）。
    first_token: Option<Duration>,
    /// 客户端收到第一个字节前实际等掉的整段时间（§6.6、§24.1）。
    ///
    /// `first_token` 只覆盖"响应头之后到首个语义事件"，对一次高并发下排了
    /// 20 秒队、然后首个事件立刻到达的请求，它会记成 1 毫秒。评分要的是用户
    /// 体感，因此性能样本改用这一项；记录里两项都保留，诊断时能分清"上游慢"
    /// 与"网关排队慢"。
    first_byte: Option<Duration>,
    output_tokens: Option<u64>,
    usage_tokens: Option<u64>,
    /// (输入, 输出) Token；流式在结算时才拿得到，这里为 None（§6.6）。
    usage_parts: (Option<u64>, Option<u64>),
    /// 完整 Token 细分（含缓存与思考）；流式为 None，由结算补（§11.6）。
    usage_detail: Option<stream::UsageBreakdown>,
    /// 流式 Responses：提交时先落骨架状态，流结束后据此补写完整历史。
    stream_state: Option<StreamStateSeed>,
    /// 流式过程中解析阶段丢掉的能力（§14.8）。非流式为 None。
    degraded: Option<translate::DegradationSink>,
}

/// 流式 Responses 在提交时留下的状态种子。
struct StreamStateSeed {
    gateway_id: String,
    upstream_id: Option<String>,
    endpoint: String,
}

/// 将健康准入绑定到流式响应的完整生命周期。
/// 把网关错误码与上游状态码翻译成健康状态机的事件（§12.3）。
fn classify_outcome(
    code: ErrorCode,
    upstream: Option<StatusCode>,
    retry_after: Option<Duration>,
) -> health::Outcome {
    match upstream {
        Some(StatusCode::UNAUTHORIZED) | Some(StatusCode::FORBIDDEN) => health::Outcome::KeyInvalid,
        Some(StatusCode::PAYMENT_REQUIRED) => health::Outcome::QuotaExhausted { retry_after },
        Some(StatusCode::TOO_MANY_REQUESTS) => health::Outcome::RateLimited { retry_after },
        // 上游 408 / 504 是它自己承认的超时，按故障计；没有状态码的超时则是
        // 我们等不下去了——"单纯变慢"只降评分，不熔断（§12.1）。
        Some(_) => health::Outcome::Fault,
        None => match code {
            ErrorCode::UpstreamTimeout => health::Outcome::Neutral,
            // 目标本身不可用（熔断、限流）不该再算一次故障：状态机刚刚才因为
            // 它拒绝过这次请求，重复计数只会让冷却无谓地翻倍。
            ErrorCode::RateLimited | ErrorCode::MultiplierExceeded => health::Outcome::Neutral,
            _ => health::Outcome::Fault,
        },
    }
}

fn unavailable_code(reason: health::Unavailable) -> ErrorCode {
    match reason {
        health::Unavailable::RateLimited | health::Unavailable::ConcurrencyFull => {
            ErrorCode::RateLimited
        }
        _ => ErrorCode::UpstreamExhausted,
    }
}

/// 向单个目标的一个端点发起一次尝试。
///
/// `credential` 是本次调用选定的那把 Key（§4.2.1 的不变量 A）：整个请求处理
/// 期间它不变。为 `None` 时退回账号的第一把 Key——这只可能发生在配置被外部
/// 改动而快照还没重建的窗口里，属于兜底而不是常规路径。
async fn attempt(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    credential: Option<&Arc<crate::credential::Credential>>,
    prepared: &Prepared,
    streaming: bool,
    timeout: Duration,
) -> Result<Success, AttemptFailure> {
    let account = &target.account;
    if timeout.is_zero() {
        return Err(AttemptFailure::switchable(
            ErrorCode::UpstreamTimeout,
            "请求已达到总超时".into(),
        ));
    }

    let url = upstream::build_url(&account.base_url, prepared.endpoint).map_err(|error| {
        AttemptFailure::switchable(
            ErrorCode::InternalError,
            format!("账号「{}」的 Base URL 无法构造端点：{error}", account.name),
        )
    })?;

    // DNS Rebinding 防护：每次请求前复查解析结果（§23.3）。
    crate::security::url_guard::assert_resolvable(&url, account.allow_private_network)
        .await
        .map_err(|error| {
            AttemptFailure::switchable(
                ErrorCode::InternalError,
                format!("账号「{}」的目标地址被拒绝：{error}", account.name),
            )
        })?;

    let api_key = match credential {
        Some(credential) => credential.secret.to_string(),
        None => load_api_key(forward.state, &account.id)
            .await
            .map_err(|message| AttemptFailure::switchable(ErrorCode::InternalError, message))?,
    };
    // 请求头按**上游端点**的协议构造，与下游用哪个协议进来无关（§14.7）。
    // 原始 multipart 路径必须保留客户端的 Content-Type（尤其是 boundary），
    // 不能让普通 JSON 头覆盖它。
    let headers = if let Some(raw) = forward.raw.as_ref() {
        upstream::build_headers_with_content_type(
            prepared.endpoint,
            &api_key,
            forward.downstream_headers,
            raw.content_type.clone(),
        )
    } else {
        upstream::build_headers(prepared.endpoint, &api_key, forward.downstream_headers)
    }
    .map_err(|error| {
        AttemptFailure::switchable(
            ErrorCode::InternalError,
            format!("账号「{}」的请求头构造失败：{error}", account.name),
        )
    })?;

    let payload = match (forward.raw.as_ref(), prepared.raw.as_ref()) {
        (Some(_), Some(raw)) => raw.clone(),
        (Some(_), None) => {
            return Err(AttemptFailure::switchable(
                ErrorCode::InternalError,
                "原始请求体准备结果缺失".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(AttemptFailure::switchable(
                ErrorCode::InternalError,
                "原始请求体状态不一致".into(),
            ));
        }
        (None, None) => serde_json::to_vec(&prepared.body).map_err(|error| {
            AttemptFailure::switchable(
                ErrorCode::InternalError,
                format!("序列化上游请求体失败：{error}"),
            )
        })?,
    };

    // 从发出上游请求到**收到响应头**的等待。它必须单独计时：上游"先回 200、
    // 再憋很久才吐第一个事件"是常见故障，此时首字延迟接近 0，只有把这段等待
    // 一并计入，评分才看得见这次卡顿（§6.6、§9.3）。
    let sent_at = Instant::now();
    let response = forward
        .state
        .upstream
        .http_for(account.allow_private_network)
        .post(url)
        .headers(headers)
        .body(payload)
        .timeout(timeout)
        .send()
        .await;

    let response = match response {
        Ok(response) => response,
        Err(error) => {
            // 连接失败、TLS 失败与超时都属于廉价失败，允许换目标（§13.1）。
            // 连不上是"坏"，连上了但慢是"慢"：前者计入熔断，后者只降评分
            // （§12.1）。连接阶段的超时归入前者。
            let code = if error.is_timeout() && !error.is_connect() {
                ErrorCode::UpstreamTimeout
            } else {
                ErrorCode::UpstreamExhausted
            };
            return Err(AttemptFailure::switchable(
                code,
                format!(
                    "账号「{}」的上游请求失败：{}",
                    account.name,
                    safe_reason(&error)
                ),
            ));
        }
    };

    let status = response.status();
    if !status.is_success() {
        // 推测端点上的 404 / 405 证明这条路由不存在：换个端点，不算目标失败。
        if endpoints::proves_missing_endpoint(account, prepared.endpoint, status.as_u16()) {
            return Err(AttemptFailure::MissingEndpoint);
        }
        return classify_upstream_error(forward, target, prepared, response, status).await;
    }
    // 响应头之前已经等掉的时间，之后所有"首字延迟"都必须从这一刻起算。
    let headers_wait = sent_at.elapsed();
    if streaming {
        commit_stream(forward, target, prepared, response, status, headers_wait).await
    } else {
        commit_body(forward, target, prepared, response, status).await
    }
}

/// 流式响应：在第一个**有语义**的事件之前仍可切换（§13.4）。
///
/// 同协议时字节原样转发，一次都不重新编码；跨协议时交给
/// [`translate::commit_stream`]，判据从"上游的字节"换成"中间事件"。
async fn commit_stream(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    prepared: &Prepared,
    response: reqwest::Response,
    status: StatusCode,
    headers_wait: Duration,
) -> Result<Success, AttemptFailure> {
    let downstream = forward.endpoint.protocol();
    let upstream_protocol = prepared.endpoint.protocol();
    let headers = response.headers().clone();

    if upstream_protocol != downstream {
        let include_usage = forward
            .body
            .get("stream_options")
            .and_then(|options| options.get("include_usage"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(downstream != Protocol::OpenAiChat);
        // 跨协议进入 Responses 时，客户端引用的 ID 同样必须是网关 ID：
        // 上游是 Chat/Messages，根本没有可复用的 Responses ID（§15.1）。
        let stream_state = if downstream == Protocol::OpenAiResponses {
            let gateway = responses::gateway_id();
            record_response_state(
                forward,
                target,
                prepared,
                &gateway,
                &serde_json::Value::Null,
                None,
                upstream_protocol,
            )
            .await;
            Some(StreamStateSeed {
                gateway_id: gateway,
                upstream_id: None,
                endpoint: prepared.endpoint.as_str().to_string(),
            })
        } else {
            None
        };
        let responses_id = stream_state.as_ref().map(|seed| seed.gateway_id.clone());
        // 先建好收集器：它一路走到结算，记录里就能看见"这次流丢了什么"（§14.8）。
        let degraded = translate::degradation_sink();
        return match translate::commit_stream(translate::StreamRequest {
            upstream: upstream_protocol,
            downstream,
            include_usage,
            account: &target.account.name,
            response,
            responses_id,
            request_id: forward.request_id,
            // 解析阶段丢掉的能力（如 Anthropic 签名）通过它汇总到结算（§14.8）。
            degraded: degraded.clone(),
        })
        .await
        {
            Ok(committed) => Ok(Success {
                status,
                response: build_response(forward, status, &headers, prepared, committed.body),
                first_token: Some(committed.first_token),
                first_byte: Some(headers_wait + committed.first_token),
                output_tokens: None,
                usage_tokens: None,
                usage_parts: (None, None),
                usage_detail: None,
                stream_state,
                degraded: Some(degraded),
            }),
            Err(failure) => {
                Err(AttemptFailure::switchable(failure.code, failure.message).with_status(status))
            }
        };
    }

    // Responses 入口的流式响应：网关 ID 必须在第一个字节下发前就定下来，
    // `response.created` 里的 id 就是客户端此后引用的地址（§15.1）。上游 ID
    // 从已缓冲的前缀里取出来登记，之后的字节流里把它替换成网关 ID。
    if downstream == Protocol::OpenAiResponses {
        let started = Instant::now();
        let mut response = response;
        let mut sniffer = stream::Sniffer::new(downstream);
        let mut upstream_id: Option<String> = None;
        let gateway;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if upstream_id.is_none() {
                        upstream_id = stream::first_response_id(&chunk);
                    }
                    match sniffer.push(&chunk) {
                        stream::Verdict::Pending => continue,
                        stream::Verdict::Semantic => {
                            let prefix = sniffer.take_buffer();
                            if upstream_id.is_none() {
                                upstream_id = stream::first_response_id(&prefix);
                            }
                            gateway = responses::gateway_id();
                            // 流式输出项逐帧出现，保存不了完整输出；正文以
                            // 入口请求体为准记录（重建时以输入为骨架）。
                            record_response_state(
                                forward,
                                target,
                                prepared,
                                &gateway,
                                upstream_id
                                    .as_deref()
                                    .map(|id| serde_json::json!({"id": id}))
                                    .as_ref()
                                    .unwrap_or(&serde_json::Value::Null),
                                None,
                                upstream_protocol,
                            )
                            .await;
                            let body = translate::passthrough_responses_stream(
                                prefix,
                                response,
                                &gateway,
                                forward.request_id,
                            );
                            return Ok(Success {
                                status,
                                response: build_response(forward, status, &headers, prepared, body),
                                first_token: first_token_of(started.elapsed()),
                                first_byte: Some(headers_wait + started.elapsed()),
                                output_tokens: None,
                                usage_tokens: None,
                                usage_parts: (None, None),
                                usage_detail: None,
                                stream_state: Some(StreamStateSeed {
                                    gateway_id: gateway.clone(),
                                    upstream_id: upstream_id.clone(),
                                    endpoint: prepared.endpoint.as_str().to_string(),
                                }),
                                // 同协议原样透传：没有解析，也就没有解析侧降级。
                                degraded: None,
                            });
                        }
                        stream::Verdict::Error(message) => {
                            return Err(AttemptFailure::switchable(
                                ErrorCode::UpstreamProtocolError,
                                format!("账号「{}」的流式响应报错：{message}", target.account.name),
                            )
                            .with_status(status));
                        }
                    }
                }
                Ok(None) => {
                    return Err(AttemptFailure::switchable(
                        ErrorCode::UpstreamProtocolError,
                        format!(
                            "账号「{}」的流式响应在产生内容前就结束",
                            target.account.name
                        ),
                    )
                    .with_status(status));
                }
                Err(error) => {
                    return Err(AttemptFailure::switchable(
                        if error.is_timeout() {
                            ErrorCode::UpstreamTimeout
                        } else {
                            ErrorCode::UpstreamExhausted
                        },
                        format!(
                            "账号「{}」的流式响应中断：{}",
                            target.account.name,
                            safe_reason(&error)
                        ),
                    )
                    .with_status(status));
                }
            }
        }
    }

    let started = Instant::now();
    let mut response = response;
    let mut sniffer = stream::Sniffer::new(downstream);

    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => match sniffer.push(&chunk) {
                stream::Verdict::Pending => continue,
                stream::Verdict::Semantic => {
                    let prefix = sniffer.take_buffer();
                    let body = translate::passthrough_stream(
                        prefix,
                        response,
                        downstream,
                        forward.request_id,
                    );
                    return Ok(Success {
                        status,
                        response: build_response(forward, status, &headers, prepared, body),
                        first_token: first_token_of(started.elapsed()),
                        first_byte: Some(headers_wait + started.elapsed()),
                        output_tokens: None,
                        usage_tokens: None,
                        usage_parts: (None, None),
                        usage_detail: None,
                        stream_state: None,
                        degraded: None,
                    });
                }
                // 语义内容出现前的明确错误事件：还没花钱，可以换号。
                stream::Verdict::Error(message) => {
                    return Err(AttemptFailure::switchable(
                        ErrorCode::UpstreamProtocolError,
                        format!("账号「{}」的流式响应报错：{message}", target.account.name),
                    )
                    .with_status(status));
                }
            },
            Ok(None) => {
                // 流在产生任何语义内容前就结束：这是损坏响应（§13.2）。
                return Err(AttemptFailure::switchable(
                    ErrorCode::UpstreamProtocolError,
                    format!(
                        "账号「{}」的流式响应在产生内容前就结束",
                        target.account.name
                    ),
                )
                .with_status(status));
            }
            Err(error) => {
                return Err(AttemptFailure::switchable(
                    if error.is_timeout() {
                        ErrorCode::UpstreamTimeout
                    } else {
                        ErrorCode::UpstreamExhausted
                    },
                    format!(
                        "账号「{}」的流式响应中断：{}",
                        target.account.name,
                        safe_reason(&error)
                    ),
                )
                .with_status(status));
            }
        }
    }
}

/// 非流式响应整体缓冲。
///
/// 非流式响应本来就是一个 JSON 对象，逐块转发没有意义；整体读回来反而能识别
/// "违反所选协议的损坏响应"（§13.2），并取出 `usage` 用于 TPM 归还与输出速度
/// 统计。跨协议时在这里把响应体翻译回下游协议。
async fn commit_body(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    prepared: &Prepared,
    response: reqwest::Response,
    status: StatusCode,
) -> Result<Success, AttemptFailure> {
    let headers = response.headers().clone();
    let bytes = read_upstream_body(response, MAX_UPSTREAM_BODY_BYTES)
        .await
        .map_err(|reason| {
            AttemptFailure::switchable(
                ErrorCode::UpstreamExhausted,
                format!("账号「{}」的响应读取失败：{reason}", target.account.name),
            )
            .with_status(status)
        })?;

    let parsed: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        AttemptFailure::switchable(
            ErrorCode::UpstreamProtocolError,
            format!("账号「{}」返回了无法解析的响应体", target.account.name),
        )
        .with_status(status)
    })?;

    let downstream = forward.endpoint.protocol();
    let upstream_protocol = prepared.endpoint.protocol();
    let body = if upstream_protocol == downstream {
        // Responses 入口：把上游的响应 ID 换成网关 ID，客户端从此只见到
        // `resp_akh_*`（§15.1）。其余协议的 ID 没有续链语义，原样透传。
        if downstream == Protocol::OpenAiResponses {
            let mut value = parsed.clone();
            let gateway = responses::gateway_id();
            // 先换成网关 ID 再落库：密封正文里不出现上游 ID，查询接口也能
            // 直接把这份真实响应对象回放给客户端（§15.1）。
            if let Some(object) = value.as_object_mut() {
                object.insert("id".into(), serde_json::json!(gateway));
            }
            record_response_state(
                forward,
                target,
                prepared,
                &gateway,
                &parsed,
                Some(&value),
                upstream_protocol,
            )
            .await;
            Body::from(value.to_string())
        } else {
            Body::from(bytes)
        }
    } else {
        // 转换失败说明上游的响应不符合它自己声明的协议：损坏响应（§13.2）。
        let converted = crate::protocol::convert_response(upstream_protocol, downstream, &parsed)
            .map_err(|reason| {
            AttemptFailure::switchable(
                ErrorCode::UpstreamProtocolError,
                format!(
                    "账号「{}」的响应无法转换回下游协议：{reason}",
                    target.account.name
                ),
            )
            .with_status(status)
        })?;
        if downstream == Protocol::OpenAiResponses {
            let mut value = converted;
            let gateway = responses::gateway_id();
            // 落库的是**转换后**的下游响应对象：查询回放才不会把上游
            // Chat/Messages 的形状原样端出去（§15）。
            if let Some(object) = value.as_object_mut() {
                object.insert("id".into(), serde_json::json!(gateway));
            }
            record_response_state(
                forward,
                target,
                prepared,
                &gateway,
                &value,
                Some(&value),
                upstream_protocol,
            )
            .await;
            Body::from(value.to_string())
        } else {
            Body::from(converted.to_string())
        }
    };

    Ok(Success {
        status,
        response: build_response(forward, status, &headers, prepared, body),
        first_token: None,
        // 非流式没有"首字"，整段等待就是用户体感（§6.6）。
        first_byte: None,
        output_tokens: stream::output_tokens(&parsed),
        usage_tokens: stream::usage_tokens(&parsed),
        usage_parts: stream::usage_parts(&parsed),
        usage_detail: Some(stream::usage_breakdown(&parsed)),
        stream_state: None,
        degraded: None,
    })
}

/// 保存一次 Responses 响应的状态链记录（§15.2）。
///
/// `upstream_body` 是上游的原始响应：定位映射取它的 ID，可重放正文取入口
/// 请求体加输出项。**在返回响应之前同步落库**：客户端拿到响应 ID 后可能
/// 立刻引用它，晚一步写入就会出现竞态。写失败只记日志——状态链是增强
/// 能力，不能让它影响主流程。
async fn record_response_state(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    prepared: &Prepared,
    gateway_id: &str,
    upstream_body: &serde_json::Value,
    final_response: Option<&serde_json::Value>,
    upstream_protocol: Protocol,
) {
    // 只有上游本身是 Responses 时才有可续链的上游 ID；跨协议转来的响应
    // 没有原生 Responses ID，续链只能靠保存的正文。
    let upstream_id = (upstream_protocol == Protocol::OpenAiResponses)
        .then(|| {
            upstream_body
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .flatten();
    let output_items = responses::output_items_of(upstream_body);
    // 保存正文用入口请求体的形状：跨协议进入时按入口协议解析重建。
    let entry_body = forward.body.clone();
    let entry_protocol = forward.endpoint.protocol();
    let state = forward.state.clone();
    let chain = forward.chain.clone();
    let gateway = gateway_id.to_string();
    let account_id = target.account.id.clone();
    let target_id = target.target.id.clone();
    let endpoint = prepared.endpoint.as_str().to_string();
    responses::record_state(
        &state,
        &chain,
        responses::PendingState {
            gateway_id: gateway,
            upstream_id,
            account_id: Some(account_id),
            target_id: Some(target_id),
            endpoint: Some(endpoint),
        },
        &entry_body,
        entry_protocol,
        output_items.as_ref(),
        final_response,
        state.settings.get().response_state_days,
    )
    .await;
}

/// 把上游的非 2xx 响应分成"可切换"与"必须直接返回下游"两类。
async fn classify_upstream_error(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    prepared: &Prepared,
    response: reqwest::Response,
    status: StatusCode,
) -> Result<Success, AttemptFailure> {
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    if is_switchable_status(status) {
        let code = match status {
            StatusCode::TOO_MANY_REQUESTS => ErrorCode::RateLimited,
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => ErrorCode::UpstreamTimeout,
            _ => ErrorCode::UpstreamExhausted,
        };
        return Err(AttemptFailure::Switchable {
            code,
            message: format!(
                "账号「{}」返回 {}{}",
                target.account.name,
                status.as_u16(),
                retry_after
                    .map(|s| format!("，建议 {s} 秒后重试"))
                    .unwrap_or_default()
            ),
            upstream_status: Some(status),
            retry_after: retry_after.map(Duration::from_secs),
        });
    }

    // 400、413、422 这类错误换个目标结果一样，直接把上游的判断转达给客户端
    // （§13.3）。同协议时原样透传上游的错误体，它本身是有用的诊断信息；跨
    // 协议时上游的错误体是另一套形状，改用网关自己的错误对象，客户端的 SDK
    // 才解析得了（§18.2）。
    let headers = response.headers().clone();
    // 错误体同样有上限：异常上游不能靠一个超大错误体把网关拖垮。
    let bytes = read_upstream_body(response, MAX_UPSTREAM_BODY_BYTES)
        .await
        .unwrap_or_else(|reason| {
            tracing::warn!(%reason, "读取上游错误响应体失败，按空体处理");
            axum::body::Bytes::new()
        });
    learn_capability_limitation(forward, target, status, &bytes);
    if prepared.endpoint.protocol() == forward.endpoint.protocol() {
        return Err(AttemptFailure::Terminal(Box::new(build_response(
            forward,
            status,
            &headers,
            prepared,
            Body::from(bytes),
        ))));
    }

    let message =
        upstream_error_message(&bytes).unwrap_or_else(|| format!("上游返回 {}", status.as_u16()));
    let mut error = GatewayError::new(ErrorCode::UnsupportedParameter, message)
        .with_protocol(forward.endpoint.protocol())
        .with_request_id(forward.request_id);
    if let Some(seconds) = retry_after {
        error = error.with_retry_after(seconds);
    }
    let mut response = error.into_response();
    *response.status_mut() = status;
    Err(AttemptFailure::Terminal(Box::new(response)))
}

/// 从上游错误体里取出可以安全转达的文本。绝不返回完整 URL、Key 或堆栈。
pub(crate) fn upstream_error_message(bytes: &[u8]) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let message = parsed
        .get("error")
        .and_then(|error| error.get("message"))
        .or_else(|| parsed.get("message"))
        .and_then(serde_json::Value::as_str)?;
    Some(
        crate::security::redact::text(message)
            .chars()
            .take(400)
            .collect(),
    )
}

/// 能力学习（§16.7）：上游明确拒绝某能力时记入限制缓存。
///
/// 只认 `error_proves_unsupported` 判定过的错误形状；普通 400、5xx、超时和
/// 网络错误绝不进入缓存。同一能力的第二次请求会因此改选其它目标，而不是
/// 再撞一次同一堵墙。
fn learn_capability_limitation(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    status: StatusCode,
    bytes: &[u8],
) {
    let parsed: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(_) => return,
    };
    let Some(capability) = capability::unsupported_from_error(status.as_u16(), &parsed) else {
        return;
    };
    forward.state.runtime.capabilities.note_unsupported(
        &target.account.id,
        &target.target.upstream_model,
        capability,
        Instant::now(),
    );
    tracing::info!(
        account = target.account.name,
        model = target.target.upstream_model,
        capability,
        "上游明确拒绝该能力，24 小时内调度避开这个组合"
    );
}

/// 该上游状态码是否意味着"换个目标可能就成了"（§13.2）。
fn is_switchable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::PAYMENT_REQUIRED
    ) || status.is_server_error()
}

fn build_response(
    forward: &Forward<'_>,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    prepared: &Prepared,
    body: Body,
) -> Response {
    let mut builder = Response::builder().status(status);
    copy_response_headers(headers, &mut builder);
    // 发生降级时显式标记，绝不修改响应体去掩盖它（§14.7、§14.8）。
    if !prepared.degraded.is_empty()
        && let Ok(value) = HeaderValue::from_str(&degrade::header_value(&prepared.degraded))
    {
        builder
            .headers_mut()
            .map(|headers| headers.insert("x-akhub-degraded", value));
    }
    builder
        .header("x-akhub-request-id", forward.request_id)
        .body(body)
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// 首个语义事件是否真的送达过客户端（§6.6、§9.3）。
///
/// 流在产出任何语义内容之前就结束（上游断流、或客户端提前断开）时没有
/// 这一项：那种情况下记录里的"首字延迟 0 毫秒"是一句谎话，用量的缺失也
/// 就无从解释。
fn first_token_of(elapsed: Duration) -> Option<Duration> {
    (!elapsed.is_zero()).then_some(elapsed)
}

fn copy_response_headers(
    from: &reqwest::header::HeaderMap,
    to: &mut axum::http::response::Builder,
) {
    for name in FORWARDED_RESPONSE_HEADERS {
        if let Some(value) = from.get(*name)
            && let (Ok(name), Ok(value)) = (
                HeaderName::try_from(*name),
                HeaderValue::from_bytes(value.as_bytes()),
            )
        {
            to.headers_mut().map(|headers| headers.insert(name, value));
        }
    }
}

/// 把请求体中的逻辑模型名替换为该目标的真实上游模型名。
///
/// 只改这一个字段，其余字节保持原样——下游看到的永远是逻辑模型名，上游
/// 看到的永远是它自己的模型名（§2.1）。
fn rewrite_model(body: &mut serde_json::Value, upstream_model: &str) {
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "model".into(),
            serde_json::Value::String(upstream_model.to_string()),
        );
    }
}

/// 没有 tokenizer 时的保守 Token 估算（§17.2）。
fn estimate_tokens(request_bytes: usize, body: &serde_json::Value) -> u64 {
    let input = (request_bytes / BYTES_PER_TOKEN) as u64;
    let output = body
        .get("max_tokens")
        .or_else(|| body.get("max_output_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    input.saturating_add(output)
}

/// 兜底凭据来源：数据库里账号凭据信封的第一把 Key。
///
/// 常规路径不用它——凭据由 [`crate::credential::CredentialPool`] 在内存里
/// 提供，这里只覆盖"快照尚未重建"的短暂窗口。错误信息里不能出现任何密钥材料。
async fn load_api_key(state: &SharedState, account_id: &str) -> Result<String, String> {
    let sealed = state
        .store
        .account_sealed_key(account_id)
        .await
        .map_err(|error| format!("读取账号凭据失败：{error}"))?
        .ok_or_else(|| "账号缺少凭据记录".to_string())?;
    let plaintext = state
        .cipher
        .open(&sealed)
        .map_err(|error| format!("解密账号凭据失败：{error}"))?;
    String::from_utf8(plaintext.to_vec()).map_err(|_| "账号凭据不是合法的 UTF-8 文本".to_string())
}

/// 从 reqwest 错误中提取安全描述，绝不包含完整 URL 或凭据。
fn safe_reason(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "上游超时"
    } else if error.is_connect() {
        "连接建立失败"
    } else if error.is_body() || error.is_decode() {
        "响应无法解析"
    } else {
        "网络错误"
    }
}

/// 从请求体中取出下游声明的逻辑模型名。
pub fn extract_model(body: &serde_json::Value, protocol: Protocol) -> Result<String, GatewayError> {
    body.get("model")
        .and_then(serde_json::Value::as_str)
        .filter(|m| !m.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            GatewayError::new(ErrorCode::UnsupportedParameter, "请求体缺少 model 字段")
                .with_protocol(protocol)
        })
}

/// multipart `model` part 的解析错误。图片编辑只需要识别这个字段，
/// 不试图实现完整 multipart 语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultipartModelError {
    Missing,
    UnsafeReplacement,
}

/// 从完整 multipart 正文中提取 `Content-Disposition` 的 `name="model"` 字段。
///
/// 解析按 boundary 行与 CRLF/LF 分隔进行，调用方已经把请求体完整读入内存，
/// 因而不会受网络分块边界影响。返回值会去掉字段值两端的空白。
pub fn extract_multipart_model(body: &[u8]) -> Result<String, MultipartModelError> {
    let range = multipart_model_range(body)?;
    let value = std::str::from_utf8(&body[range])
        .map_err(|_| MultipartModelError::Missing)?
        .trim();
    if value.is_empty() {
        return Err(MultipartModelError::Missing);
    }
    Ok(value.to_string())
}

/// 只替换 multipart `model` part 的内容，保留其它字节与 boundary 原样。
///
/// 上游模型名若含控制字符会改变 multipart 的结构（例如注入换行或新的
/// boundary），因此直接拒绝，而不是把不安全字节写进正文。
pub fn rewrite_multipart_model(
    body: &[u8],
    upstream_model: &str,
) -> Result<Vec<u8>, MultipartModelError> {
    if upstream_model.is_empty() || upstream_model.chars().any(char::is_control) {
        return Err(MultipartModelError::UnsafeReplacement);
    }
    let range = multipart_model_range(body)?;
    let mut rewritten = Vec::with_capacity(
        body.len()
            .saturating_sub(range.end.saturating_sub(range.start))
            .saturating_add(upstream_model.len()),
    );
    rewritten.extend_from_slice(&body[..range.start]);
    rewritten.extend_from_slice(upstream_model.as_bytes());
    rewritten.extend_from_slice(&body[range.end..]);
    Ok(rewritten)
}

/// 找到 `model` part 的值范围（不包含值前后的 multipart 分隔换行）。
fn multipart_model_range(body: &[u8]) -> Result<Range<usize>, MultipartModelError> {
    let (boundary, mut cursor) = multipart_first_boundary(body)?;

    loop {
        if cursor >= body.len() || !body[cursor..].starts_with(&boundary) {
            return Err(MultipartModelError::Missing);
        }
        let after_boundary = cursor + boundary.len();
        if body[after_boundary..].starts_with(b"--") {
            return Err(MultipartModelError::Missing);
        }

        // 当前 boundary 行后面必须紧跟换行，之后才是 part headers。
        let (_, headers_start) =
            multipart_line(body, cursor).ok_or(MultipartModelError::Missing)?;
        let mut line_start = headers_start;
        let mut is_model = false;
        let value_start = loop {
            let (line_end, next) =
                multipart_line(body, line_start).ok_or(MultipartModelError::Missing)?;
            if line_end == line_start {
                break next;
            }
            if multipart_is_model_disposition(&body[line_start..line_end]) {
                is_model = true;
            }
            line_start = next;
        };

        let next_boundary = multipart_find_boundary(body, value_start, &boundary)
            .ok_or(MultipartModelError::Missing)?;
        let value_end = multipart_trim_delimiter_newline(body, value_start, next_boundary);
        if is_model {
            return Ok(value_start..value_end);
        }
        cursor = next_boundary;
    }
}

/// 读取一行，返回不含换行的结束位置与下一行起点。兼容 CRLF 与 LF。
fn multipart_line(body: &[u8], start: usize) -> Option<(usize, usize)> {
    let newline = body[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| start + offset)?;
    let end = if newline > start && body[newline - 1] == b'\r' {
        newline - 1
    } else {
        newline
    };
    Some((end, newline + 1))
}

/// 取出正文首行 boundary 与首个 part 的起点。
fn multipart_first_boundary(body: &[u8]) -> Result<(Vec<u8>, usize), MultipartModelError> {
    if !body.starts_with(b"--") {
        return Err(MultipartModelError::Missing);
    }
    let (line_end, _) = multipart_line(body, 0).ok_or(MultipartModelError::Missing)?;
    if line_end <= 2 {
        return Err(MultipartModelError::Missing);
    }
    // 从首个 delimiter 行开始处理；循环会从该行读取 headers 起点。
    Ok((body[..line_end].to_vec(), 0))
}

/// 在行首查找下一个 boundary delimiter。
fn multipart_find_boundary(body: &[u8], start: usize, boundary: &[u8]) -> Option<usize> {
    let mut search = start;
    while search <= body.len() {
        let relative = body[search..]
            .windows(boundary.len())
            .position(|window| window == boundary)?;
        let position = search + relative;
        let at_line_start = position == 0 || body[position - 1] == b'\n';
        let after = position + boundary.len();
        let valid_suffix = after == body.len() || matches!(body[after], b'-' | b'\r' | b'\n');
        if at_line_start && valid_suffix {
            return Some(position);
        }
        search = position.saturating_add(1);
    }
    None
}

/// 去掉 boundary 前用于分隔 part 的最后一个 CRLF/LF。
fn multipart_trim_delimiter_newline(body: &[u8], start: usize, boundary: usize) -> usize {
    let mut end = boundary;
    if end > start && body[end - 1] == b'\n' {
        end -= 1;
        if end > start && body[end - 1] == b'\r' {
            end -= 1;
        }
    }
    end
}

/// 判断一个 header 行是否声明了 `name="model"` part。
fn multipart_is_model_disposition(line: &[u8]) -> bool {
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return false;
    };
    if !trim_ascii(&line[..colon]).eq_ignore_ascii_case(b"content-disposition") {
        return false;
    }
    for segment in line[colon + 1..].split(|byte| *byte == b';') {
        let Some(equal) = segment.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        if !trim_ascii(&segment[..equal]).eq_ignore_ascii_case(b"name") {
            continue;
        }
        let mut value = trim_ascii(&segment[equal + 1..]);
        if value.len() >= 2 && value[0] == b'"' && value[value.len() - 1] == b'"' {
            value = &value[1..value.len() - 1];
        }
        return value == b"model";
    }
    false
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |position| position + 1);
    &bytes[start..end]
}

/// 请求体在内存里的上限；超过后落临时文件（§17.3、§19.4）。
pub const IN_MEMORY_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// 非流式上游响应体的硬上限（§17.3）。正常推理响应远小于这个数；设置上限
/// 是为了让异常或恶意上游不能把网关内存撑爆。
pub const MAX_UPSTREAM_BODY_BYTES: usize = 64 * 1024 * 1024;

/// 读取并解析请求体，同时施加大小上限（§17.3）。
///
/// 8 MiB 以内直接在内存里解析；超过后先把原始字节写进数据目录下的临时
/// 文件，再用 `serde_json::from_reader` 流式解析——避免同时持有一份完整的
/// 原始字节和解析后的 `Value`。临时文件随 `NamedTempFile` 在本函数返回或
/// 出错时自动删除（§1291）。
pub async fn read_body(
    body: Body,
    max_bytes: usize,
    protocol: Protocol,
    temp_dir: &std::path::Path,
) -> Result<(serde_json::Value, usize), GatewayError> {
    use std::io::{Seek as _, Write as _};

    let too_large = || {
        GatewayError::new(
            ErrorCode::RequestTooLarge,
            format!("请求体超过上限 {max_bytes} 字节，或读取中断"),
        )
        .with_protocol(protocol)
    };
    let disk_error = |error: std::io::Error| {
        tracing::warn!(%error, "临时请求体读写失败");
        GatewayError::new(ErrorCode::InternalError, "内部错误，详见服务端日志")
            .with_protocol(protocol)
    };

    let mut stream = body.into_data_stream();
    let mut memory: Vec<u8> = Vec::new();
    let mut file: Option<tempfile::NamedTempFile> = None;
    let mut size = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| too_large())?;
        size = size.saturating_add(chunk.len());
        if size > max_bytes {
            return Err(too_large());
        }
        if file.is_none() && memory.len() + chunk.len() > IN_MEMORY_BODY_LIMIT {
            let mut created = tempfile::Builder::new()
                .prefix("body-")
                .tempfile_in(temp_dir)
                .map_err(disk_error)?;
            created
                .write_all(&memory)
                .and_then(|()| created.flush())
                .map_err(disk_error)?;
            memory = Vec::new();
            file = Some(created);
        }
        match &mut file {
            Some(file) => file.write_all(&chunk).map_err(disk_error)?,
            None => memory.extend_from_slice(&chunk),
        }
    }

    let parsed = match &mut file {
        Some(file) => {
            file.flush().map_err(disk_error)?;
            file.as_file_mut().rewind().map_err(disk_error)?;
            serde_json::from_reader(std::io::BufReader::new(file.as_file_mut()))
        }
        None => serde_json::from_slice(&memory),
    };
    let value = parsed.map_err(|error| {
        // 下游 JSON 非法不切换目标，直接快速失败（§13.3）。
        GatewayError::new(
            ErrorCode::UnsupportedParameter,
            format!("请求体不是合法 JSON：{error}"),
        )
        .with_protocol(protocol)
    })?;
    // `file` 在这里析构，临时文件随之删除。
    Ok((value, size))
}

/// 读取原始请求体并施加大小上限。multipart 需要保留完整字节以便只改写
/// `model` part，因此不走 JSON 解析或临时文件路径。
pub async fn read_raw_body(
    body: Body,
    max_bytes: usize,
    protocol: Protocol,
) -> Result<(Vec<u8>, usize), GatewayError> {
    let too_large = || {
        GatewayError::new(
            ErrorCode::RequestTooLarge,
            format!("请求体超过上限 {max_bytes} 字节，或读取中断"),
        )
        .with_protocol(protocol)
    };

    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    let mut size = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| too_large())?;
        size = size.saturating_add(chunk.len());
        if size > max_bytes {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, size))
}

/// 读取非流式上游响应体，并施加硬上限（§17.3）。
pub async fn read_upstream_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<axum::body::Bytes, String> {
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "读取上游响应失败".to_string())?;
        append_capped(&mut buffer, &chunk, max_bytes)?;
    }
    Ok(axum::body::Bytes::from(buffer))
}

/// 追加一块响应字节；超限时整块拒绝，不写半块。
fn append_capped(buffer: &mut Vec<u8>, chunk: &[u8], max_bytes: usize) -> Result<(), String> {
    if buffer.len().saturating_add(chunk.len()) > max_bytes {
        return Err(format!("上游响应体超过 {max_bytes} 字节上限"));
    }
    buffer.extend_from_slice(chunk);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_rewrite_touches_only_the_model_field() {
        let body = json!({
            "model": "claude-sonnet-4-5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "厂商扩展": {"保留": true}
        });
        let mut rewritten = body.clone();
        rewrite_model(&mut rewritten, "claude-sonnet-4-5-20250929");

        assert_eq!(rewritten["model"], "claude-sonnet-4-5-20250929");
        assert_eq!(rewritten["stream"], true);
        assert_eq!(rewritten["messages"], body["messages"]);
        assert_eq!(rewritten["厂商扩展"]["保留"], true, "未知字段必须原样保留");
    }

    #[test]
    fn switchable_statuses_match_the_failover_rules() {
        // §13.2 可以切换
        for code in [401, 402, 403, 404, 408, 429, 500, 502, 503, 504] {
            assert!(
                is_switchable_status(StatusCode::from_u16(code).unwrap()),
                "{code} 应当允许切换"
            );
        }
        // §13.3 不切换：这些是下游请求本身的问题
        for code in [400, 413, 422] {
            assert!(
                !is_switchable_status(StatusCode::from_u16(code).unwrap()),
                "{code} 不应当切换"
            );
        }
    }

    #[test]
    fn upstream_statuses_map_onto_the_health_state_machine() {
        // 401/403 是"Key 坏了"，影响整个账号；429 只影响账号 + 模型（§12.1）。
        assert!(matches!(
            classify_outcome(
                ErrorCode::UpstreamExhausted,
                Some(StatusCode::UNAUTHORIZED),
                None
            ),
            health::Outcome::KeyInvalid
        ));
        assert!(matches!(
            classify_outcome(
                ErrorCode::RateLimited,
                Some(StatusCode::TOO_MANY_REQUESTS),
                None
            ),
            health::Outcome::RateLimited { .. }
        ));
        assert!(matches!(
            classify_outcome(
                ErrorCode::UpstreamExhausted,
                Some(StatusCode::INTERNAL_SERVER_ERROR),
                None
            ),
            health::Outcome::Fault
        ));
        // 没能发出去的请求不该算目标的故障。
        assert!(matches!(
            classify_outcome(ErrorCode::RateLimited, None, None),
            health::Outcome::Neutral
        ));
        // 慢到我们等不下去只是"慢"，不是"坏"（§12.1）；上游自己报 504 才是坏。
        assert!(matches!(
            classify_outcome(ErrorCode::UpstreamTimeout, None, None),
            health::Outcome::Neutral
        ));
        assert!(matches!(
            classify_outcome(
                ErrorCode::UpstreamTimeout,
                Some(StatusCode::GATEWAY_TIMEOUT),
                None
            ),
            health::Outcome::Fault
        ));
    }

    #[test]
    fn token_estimates_stay_on_the_conservative_side() {
        // 宁可高估把自己挡在限流外，也不要低估越过上游的 TPM。
        let body = json!({"max_tokens": 4096});
        let estimate = estimate_tokens(30_000, &body);
        assert_eq!(estimate, 10_000 + 4096);
        // 没声明最大输出时只算输入。
        assert_eq!(estimate_tokens(3_000, &json!({})), 1_000);
    }

    #[test]
    fn model_extraction_rejects_missing_and_blank_names() {
        let body = json!({"model": "glm-4.6"});
        assert_eq!(
            extract_model(&body, Protocol::OpenAiChat).unwrap(),
            "glm-4.6"
        );

        for body in [
            json!({}),
            json!({"model": ""}),
            json!({"model": "  "}),
            json!({"model": 7}),
        ] {
            let error = extract_model(&body, Protocol::OpenAiChat).unwrap_err();
            assert_eq!(error.code, ErrorCode::UnsupportedParameter);
        }
    }

    #[tokio::test]
    async fn oversized_bodies_are_rejected_with_413() {
        let dir = tempfile::tempdir().unwrap();
        let body = Body::from(vec![b'x'; 4096]);
        let error = read_body(body, 1024, Protocol::OpenAiChat, dir.path())
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::RequestTooLarge);
        assert_eq!(error.code.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "失败路径也不能留下临时文件"
        );
    }

    #[tokio::test]
    async fn malformed_json_fails_fast_without_failover() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_body(
            Body::from("{不是 JSON"),
            1024,
            Protocol::AnthropicMessages,
            dir.path(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedParameter);
        assert!(!error.code.is_retryable(), "下游请求本身非法，重试没有意义");
    }

    /// 没等到首个语义事件时，首字延迟必须是"未知"而不是 0（§6.6）。
    ///
    /// 线上真实故障：客户端断开后记录里 `first_token_ms = 0`，看起来像
    /// "立刻出字了"，实际上是从来没等到。
    #[test]
    fn an_unreached_first_event_is_unknown_not_zero() {
        assert_eq!(first_token_of(Duration::ZERO), None);
        assert_eq!(
            first_token_of(Duration::from_millis(920)),
            Some(Duration::from_millis(920))
        );
    }

    #[tokio::test]
    async fn body_size_is_reported_for_the_stickiness_budget() {
        let dir = tempfile::tempdir().unwrap();
        let payload = json!({"model": "glm-4.6"});
        let raw = serde_json::to_vec(&payload).unwrap();
        let expected = raw.len();
        let (value, size) = read_body(Body::from(raw), 1024, Protocol::OpenAiChat, dir.path())
            .await
            .unwrap();
        assert_eq!(size, expected);
        assert_eq!(value["model"], "glm-4.6");
    }

    #[tokio::test]
    async fn raw_bodies_keep_bytes_and_share_the_request_size_error() {
        let raw = b"--b\r\nmodel\r\n--b--\r\n".to_vec();
        let (read, size) = read_raw_body(Body::from(raw.clone()), 1024, Protocol::OpenAiChat)
            .await
            .unwrap();
        assert_eq!(read, raw);
        assert_eq!(size, read.len());

        let error = read_raw_body(Body::from(vec![0_u8; 4096]), 1024, Protocol::OpenAiChat)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::RequestTooLarge);
        assert_eq!(error.code.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// 超过 8 MiB 的请求体必须落临时文件解析，且返回前把文件清理掉（§1291）。
    #[tokio::test]
    async fn large_bodies_are_spooled_to_disk_and_still_parse() {
        let dir = tempfile::tempdir().unwrap();
        let blob = "x".repeat(9 * 1024 * 1024);
        let raw = serde_json::to_vec(&json!({"model": "m", "blob": blob})).unwrap();
        let expected = raw.len();
        let (value, size) = read_body(
            Body::from(raw),
            64 * 1024 * 1024,
            Protocol::OpenAiChat,
            dir.path(),
        )
        .await
        .unwrap();
        assert!(size > IN_MEMORY_BODY_LIMIT, "必须走落盘路径");
        assert_eq!(size, expected);
        assert_eq!(value["blob"].as_str().unwrap().len(), 9 * 1024 * 1024);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "临时文件必须随请求结束删除"
        );
    }

    /// 上游响应体上限必须真的生效（§17.3）。
    #[test]
    fn upstream_bodies_stop_at_the_cap() {
        let mut buffer = Vec::new();
        assert!(append_capped(&mut buffer, &[0u8; 1024], 2048).is_ok());
        assert_eq!(buffer.len(), 1024);
        let error = append_capped(&mut buffer, &[0u8; 2048], 2048).unwrap_err();
        assert!(error.contains("上限"), "{error}");
        assert_eq!(buffer.len(), 1024, "超限的块不得部分写入");
    }

    #[test]
    fn multipart_model_is_found_and_only_its_value_is_rewritten() {
        let body = b"--boundary\r\n\
Content-Disposition: form-data; name=\"model\"\r\n\
\r\n\
logical-model\r\n\
--boundary\r\n\
Content-Disposition: form-data; name=\"image\"; filename=\"a.bin\"\r\n\
Content-Type: application/octet-stream\r\n\
\r\n\
\x00\x01same-bytes\r\n\
--boundary--\r\n";
        assert_eq!(extract_multipart_model(body).unwrap(), "logical-model");
        let rewritten = rewrite_multipart_model(body, "upstream-model").unwrap();
        assert!(rewritten.starts_with(b"--boundary\r\n"));
        assert!(
            rewritten
                .windows(b"upstream-model".len())
                .any(|window| { window == b"upstream-model" })
        );
        assert!(rewritten.ends_with(b"\x00\x01same-bytes\r\n--boundary--\r\n"));
    }

    #[test]
    fn multipart_parser_accepts_lf_and_rejects_missing_model() {
        let body = b"--b\nContent-Disposition: form-data; name=\"image\"\n\nbytes\n--b--\n";
        assert_eq!(
            extract_multipart_model(body),
            Err(MultipartModelError::Missing)
        );
        let body = b"--b\nContent-Disposition: form-data; name=\"model\"\n\n m \n--b--\n";
        assert_eq!(extract_multipart_model(body).unwrap(), "m");
    }

    #[test]
    fn multipart_rewrite_rejects_control_characters_in_upstream_model() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n--b--\r\n";
        assert_eq!(
            rewrite_multipart_model(body, "bad\r\nvalue"),
            Err(MultipartModelError::UnsafeReplacement)
        );
    }
}
