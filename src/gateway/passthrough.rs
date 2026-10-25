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
use crate::config::{GroupView, TargetView};
use crate::domain::{Multiplier, Protocol};
use crate::gateway::error::{ErrorCode, GatewayError};
use crate::gateway::{responses, settle, stream, translate};
use crate::health;
use crate::multiplier;
use crate::protocol::canonical::Effort;
use crate::protocol::degrade;
use crate::protocol::translate::Translation;
use crate::routing::{self, queue, score, sticky};
use crate::storage::store::{AttemptRecord, RequestRecord};
use crate::upstream::endpoints::{self, Choice};
use crate::upstream::evidence::EffortFacts;
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

/// 同一个端点最多为"思考档位被上游拒绝"重发几次（§14.6）。
///
/// 6 次是这条阶梯的全部长度：从最低的 minimal 出发，逐档往上试到 max，最后
/// 再试一次"不指定"，正好 6 步。实际几乎走不满——循环真正的刹车是"这个形状
/// 本轮已经发过就不再发"，而试探结果会被记住，所以这条阶梯在
/// （账号，上游模型，档位）上**只会走一次**，之后直接跳到结论。
///
/// 有界是硬要求：没有界就是拿上游当重试靶子。
///
/// 走完仍被拒不再直接失败，而是交给外层换渠道（`AttemptFailure::EffortExhausted`）：
/// 上游那句 400 有可能来自中转站内部一条实现不同的渠道，换一个目标就不复现
/// （2026-10-15 现场）。
const MAX_EFFORT_RETRIES: u8 = 6;

/// 上游返回"无法归因的 400"时，最多再换几个目标重试（§13.7）。
///
/// 400 意味着上游在**开始生成之前**就拒了这次请求，一个 token 都没消耗——这是
/// 唯一一种"重试不花钱"的失败，所以值得换个渠道再试。但请求体要重传一次（现场
/// 是 1.6 MB），仍然要有上限：2 个额外目标足以跨过某个域名的瞬时抖动，又不至于
/// 把一份大请求在每个目标上各传一遍。
const MAX_OPAQUE_REJECTION_RETRIES: u8 = 2;

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
    /// 绑定/钉住的那把 Key 只是忙：应当**等它**，而不是换一把别的。
    ///
    /// 与 `Busy` 分开是必要的：`Busy` 表示"任意一把的名额都能唤醒我"，
    /// 而这里必须只等**这一把**——否则唤醒之后重新抽签会落到别的 Key 上，
    /// 上游按凭据隔离的前缀缓存照样被作废（§2.2 的不变量 B）。
    BoundBusy(Arc<crate::credential::Credential>),
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

/// 上游的 403 响应体里有没有**明确说凭据本身不对**（§12.3）。
///
/// 上游对 403 的用法很杂：分组被停用/删除、站点权限不足、WAF 拦截、被封 IP，
/// 以及真正的"Key 无效"。只看状态码分不出这几种，稍不留神就把一把好 Key 判死。
/// 只有响应体自己说出 invalid / expired / unauthorized / 无效 这类词时，才认为
/// 它证明了凭据失效；其余 403 按**目标级权限问题**处理，只触发本次切换。
///
/// 判据故意保守：宁可把一次真的失效当成临时故障（下一轮换目标），也不要把
/// "分组被停用"这种一次性事件变成这把 Key 的永久硬停。
fn auth_proves_key_invalid(status: StatusCode, body: &[u8]) -> bool {
    if status != StatusCode::FORBIDDEN {
        // 401 是状态码本身就说明了问题，不看正文。
        return status == StatusCode::UNAUTHORIZED;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    if text.is_empty() {
        // 空体的 403 什么都没证明。放行换目标，不做失效判定。
        return false;
    }
    const HINTS: [&str; 12] = [
        "invalid api key",
        "invalid_api_key",
        "invalid key",
        "invalid token",
        "api key is invalid",
        "unauthorized",
        "authentication_error",
        "key 无效",
        "无效的 key",
        "密钥无效",
        "凭据无效",
        "认证失败",
    ];
    HINTS.iter().any(|hint| text.contains(hint))
}

/// 上游用 401/403 拒绝一次凭据级的尝试时，这次失败该记成什么（§12.3）。
///
/// 判据只作用于这两个状态码：402/429 有自己的判定，且它们的 `Retry-After`
/// 正等着被消费，拿正文去改写只会把"额度耗尽"与"限流"降级成普通故障。
fn credential_outcome(
    status: StatusCode,
    retry_after: Option<Duration>,
    body: &[u8],
) -> health::Outcome {
    match retry_after {
        // 上游给了恢复时间，就是"过一会儿再来"而不是"这把 Key 废了"：
        // 按额度冷却处理，绝不做硬停。
        Some(wait) => health::Outcome::QuotaExhausted {
            retry_after: Some(wait),
        },
        // 没给恢复时间：只有正文**明确说凭据不对**才算失效，其余 403
        //（分组被停用、权限不足、WAF 拦截）按故障切换。
        None if auth_proves_key_invalid(status, body) => health::Outcome::KeyInvalid,
        None => health::Outcome::Fault,
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
        /// 上游的错误响应体（有上限）。403 的真假凭据失效判定要看它（§12.3）。
        upstream_body: Option<Vec<u8>>,
    },
    /// 明确属于下游请求本身的问题，切换到别的目标也是同样结果。
    /// 装箱是因为 `Response` 比其余变体大一个数量级，而这是**失败**分支：
    /// 让成功路径为它多搬 128 字节不划算。
    ///
    /// 第二个字段是**上游**给出的状态码（网关自己判定的参数错误是 `None`）：
    /// 请求记录靠它区分"上游拒绝"与"网关拒绝"，否则失败记录里这一列恒为空
    /// （2026-10-15 生产库：821 条 400 全部没有上游状态码）。
    Terminal(Box<Response>, Option<StatusCode>),
    /// 已接单的异步图片任务失败：不得换号或再次下单，但必须记为失败。
    AcceptedJob(GatewayError),
    /// 上游拒绝了自适应思考的档位，但形状是对的（§14.6）。
    ///
    /// 这是**同一个目标**上的一次廉价失败：上游没有开始生成，而我们知道该改成
    /// 什么——换一个这个账号能接受的档位再发一次，客户端不该为"这个中转站少
    /// 实现了两档"看见 400。
    ///
    /// 带原料而不是 `Response`：升无可升时这个变体被翻译成 `EffortExhausted`
    /// 交给外层换渠道，最终响应到那时才组装（与 `OpaqueRejection` 同理）。
    EffortRejected {
        rejected: crate::protocol::canonical::Effort,
        status: StatusCode,
        message: String,
        public: Option<crate::security::redact::PublicMessage>,
    },
    /// 本次请求可尝试其他端点。404/405 不证明整个账号缺少接口，
    /// 保留真实错误，转换不可行时仍能解释失败原因。
    EndpointRejected { status: StatusCode, body: Vec<u8> },
    /// 上游返回了一个**无法归因**的 400（§13.7）。
    ///
    /// 现场形态：`The request could not be processed. Please check the request
    /// parameters.`——它既没说是哪个参数，也没说是形状问题。这种 400 有两种
    /// 来源：客户端的请求真的有问题，或者上游此刻自己抽了。区分不了，但**代价
    /// 不对称**：400 意味着上游在开始生成之前就拒了，一个 token 都没消耗，
    /// 换一个渠道重试几乎免费；而直接把 400 透给客户端，真实故障就变成用户可见
    /// 的失败。所以换目标再试有限次，全都不行才把最后一次的原样报出去。
    ///
    /// 与 `Terminal` 的区别就在这里：`Terminal` 是"已经能归因到请求本身"
    /// （如 `prompt is too long`、`ReasoningEffort invalid`），换谁都是同样结果。
    ///
    /// 带原料而不是 `Response`：`Response` 的 body 不是 `Sync`，放进
    /// `AttemptFailure` 会让 `Walk` 失去 `Sync`，而后台任务要求它跨线程。
    OpaqueRejection {
        status: StatusCode,
        message: String,
        public: Option<crate::security::redact::PublicMessage>,
    },
    /// 上游的内容审核拒绝了这份输入（§13.3）。
    ///
    /// 现场形态（2026-10-23）：`Request was blocked by upstream content filtering`。
    /// 它与 `OpaqueRejection` 一样是"上游没开始生成就拒了"，但性质完全不同：
    /// 同一份请求在别的账号上**能过**（现场 8858B 的请求体在华强北kiro 被拦、
    /// 3 秒后在 supeai.cc-awsclaude 上 200）。
    ///
    /// 处置与其他可切换失败一致，只是**不计入目标故障**：账号没有被证明有问题，
    /// 只是它不肯处理这份输入，罚分会把一个好好的账号冷却掉。
    /// 另外要显式清掉粘性绑定——不换渠道的话下一跳还是它，同一段会话会被反复拒。
    ContentFiltered {
        status: StatusCode,
        message: String,
        public: Option<crate::security::redact::PublicMessage>,
    },
    /// 上游把这个会话钉在了别的渠道上，换账号可能就过（§13.3）。
    ///
    /// 与 `ContentFiltered` 同为"换渠道可能解决"的失败，处置相同：不计目标故障、
    /// 清掉粘性绑定、换一个目标重试。区别在换的**次数**——审核策略是确定的，
    /// 所以只换一次；渠道挂载关系是随机的（下一跳可能落回原渠道），可以多试几个。
    SessionUnroutable {
        status: StatusCode,
        message: String,
        public: Option<crate::security::redact::PublicMessage>,
    },
    /// 思考档位阶梯在本目标上走完仍被上游拒绝（§14.6）。
    ///
    /// 现场形态（2026-10-15）：同一个中转站在 `claude-opus-5-5` 这个模型名下接了
    /// 多条上游渠道，其中一条把 `thinking` 改写成旧的 `type:enabled` 形状——
    /// 无论 Akhub 发哪个档位、甚至完全不指定档位，它都回同一句
    /// `requires adaptive thinking`。这句话与请求本身无关，是那条渠道自己的问题。
    ///
    /// 处置与 `OpaqueRejection` 完全一致：上游没开始生成、没消耗 token，换一个
    /// 渠道再试几乎免费；全都不行才把上游那句原样报出去（§13.7）。
    EffortExhausted {
        status: StatusCode,
        message: String,
        public: Option<crate::security::redact::PublicMessage>,
    },
}

impl AttemptFailure {
    fn switchable(code: ErrorCode, message: String) -> Self {
        Self::Switchable {
            code,
            message,
            upstream_status: None,
            retry_after: None,
            upstream_body: None,
        }
    }

    fn with_status(self, status: StatusCode) -> Self {
        match self {
            Self::Switchable {
                code,
                message,
                retry_after,
                upstream_body,
                ..
            } => Self::Switchable {
                code,
                upstream_body,
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
    /// 这次命中的粘性键来自哪一级（§24.1）。硬粘性、软亲和与守门放行在请求
    /// 记录里只差这一个字段，没有它就无法按级统计守门放行的比例。
    sticky_origin: Option<&'static str>,
    sticky_hit: bool,
    /// 绑定里的目标是不是**真的被这次请求用上了**（§24.1）。
    ///
    /// `sticky_hit` 只说明"找到了一条绑定"，在软亲和路径下它会在**重新抽签之前**
    /// 就置位——所以它长期高估粘性：现场（AwsClaude / opus-5-5）`sticky_hit` 看着
    /// 有 82.8%，实测相邻请求却有 35% 换了账号。两个字段合起来才回答得了
    /// "到底粘住了没有"：`hit=true, kept=false` 就是"有绑定但被抽签抽走了"。
    sticky_kept: Option<bool>,
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
    /// 候选过滤原因的**具体**说明（§24.1）：哪个账号、哪项能力、哪个端点。
    filter_details: Option<String>,
    /// 最终选中的层；粘性命中时是绑定目标所在的层（§24.1）。
    selected_layer: Option<i64>,
    /// 这次实际发出去的思考档位；成功后用它记下"这一档在这个账号上可用"（§14.6）。
    sent_effort: Option<crate::protocol::canonical::Effort>,
    /// 这一次尝试真正从上游拿到的状态码（§6.6）。网关自己判定的失败保持 None。
    upstream_status: Option<StatusCode>,
    /// 这一次尝试实际发出去的思考档位（`None` = 不指定）（§14.6）。
    ///
    /// 与 `sent_effort` 分开：那个字段是"最后一次发出去的是什么"，成功后拿它
    /// 记"这一档可用"；这个字段跟着尝试明细走，重发的那几跳各记各的。
    attempt_effort: Option<Effort>,
    /// 因为"上游不接受这个思考档位"而在**同一个目标上**重发了几次（§14.6）。
    ///
    /// `attempts` 数的是目标个数，数不到同一目标内的重发：现场一次请求打了
    /// 4 次上游，记录里却是 `attempts=1`。重发不是免费的——每一次都是一轮完整
    /// 的上游往返延迟。
    effort_retries: i64,
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
    last_public: Option<crate::security::redact::PublicMessage>,
    /// 最后一次失败附带的 `Retry-After` 原文（秒），粘性路径据此决定是否
    /// 原地等待（§10.3）。
    ///
    /// 存原文而不是解析结果：换 Key 重试时要用**同一份**上游恢复时间给额度熔断
    /// 定时，而 `Duration` 在解析失败时与"没有恢复时间"无法区分。
    last_retry_after: Option<Duration>,
    /// 已经为"不归因的 400"换过几个目标（§13.7）。
    opaque_rejection_retries: u8,
    /// 内容审核拒绝过之后，是否已经换过渠道。
    ///
    /// 与 `opaque_rejection_retries` 分开计：两者的上游代价一样（都没开始生成），
    /// 但审核拒绝是**确定会复现**的（同一份输入、同一套策略），所以只换一次渠道
    /// 探一下别的账号，不在这条路上多花请求体；合成一个预算则会让它挤掉
    /// "无法归因的 400"的名额。
    content_filtered: bool,
    /// 思考档位阶梯走完仍被拒后，已经换过几个目标（§14.6）。
    ///
    /// 与 `opaque_rejection_retries` 分开计：两者的上游代价一样（都没开始生成），
    /// 但合成一个预算会让"档位被拒"和"无法归因的 400"互相挤掉名额。
    effort_exhausted_retries: u8,
    /// 最后一次内容审核拒绝的原料：(状态码, 上游原文, 已脱敏的公开说法)。
    ///
    /// 与 `last_opaque` 分开存：候选全部用尽时两句话要分别报出去，混在一起会
    /// 把"内容被拦"说成"请求无法处理"，用户就不知道该改哪一边。
    last_filtered: Option<(
        StatusCode,
        String,
        Option<crate::security::redact::PublicMessage>,
    )>,
    /// 最后一次"不归因的 400"的原料：(状态码, 上游原文, 已脱敏的公开说法)。
    ///
    /// 存原料而不是 `Response`：`Response` 的 body 不是 `Sync`，放进 `Walk`
    /// 会让整个结构失去 `Sync`，而后台任务要求它跨线程（§13.7）。
    /// 候选全部用尽时用这三样重建响应——上游那句才是真实原因，比网关自己拼一句
    /// "目标都失败了"有用得多。
    last_opaque: Option<(
        StatusCode,
        String,
        Option<crate::security::redact::PublicMessage>,
    )>,
    /// 最后一次凭据级失败的**最终健康判定**，由 `run()` 结算到那把 Key 上。
    ///
    /// 判定只做一次：换 Key 重试时若重新推导，就会丢掉"到底是 401 还是 403"
    /// 这个前提，把 401 也按"正文没提凭据"降级成普通故障（§12.3）。
    last_credential_outcome: Option<health::Outcome>,
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
    /// 这次请求命中的绑定指向哪个目标（§24.1）。
    ///
    /// 用来回答"绑定目标有没有真的被用上"——与 `soft_affinity`（偏的是 Key）
    /// 分开：目标是抽签决定的，Key 是绑定保住的，两者不是同一件事。
    bound_target_id: Option<String>,
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
        translation: &translation,
        endpoint: forward.endpoint,
        allow_degrade: forward.group.group.allow_degrade,
        protocol: forward.endpoint.protocol(),
        streaming,
        now_unix,
        now: Instant::now(),
        // 粘性绑定的凭据在下面查到之后才知道；计划本身不需要它。
        bound_credential: None,
        // 请求体大小参与软亲和的留存率：请求越大，换号重建缓存越贵（§10.3）。
        request_bytes: forward.request_bytes,
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
        estimated_tokens: estimate_tokens(
            forward.endpoint,
            forward.request_bytes,
            &forward.body,
            forward.raw.as_ref(),
        ),
        deadline,
        queue_deadline,
        now_unix,
        attempted: Vec::new(),
        last: None,
        last_public: None,
        last_retry_after: None,
        opaque_rejection_retries: 0,
        content_filtered: false,
        effort_exhausted_retries: 0,
        last_opaque: None,
        last_filtered: None,
        last_credential_outcome: None,
        usage_parts: (None, None),
        usage_detail: None,
        attempt_log: Vec::new(),
        // 软亲和偏好哪把 Key 由绑定决定，填充在下面（弱身份才有）。
        soft_affinity: None,
        // 绑定指向哪个目标同样在下面填充，用于判断"到底粘住了没有"（§24.1）。
        bound_target_id: None,
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
    walk.telemetry.sticky_origin = sticky_key.as_ref().map(|(_, origin)| origin.as_str());
    let soft = sticky_key
        .as_ref()
        .is_some_and(|(_, origin)| *origin == sticky::Origin::StablePrefix);
    let existing = sticky_key
        .as_ref()
        .and_then(|(key, _)| forward.state.runtime.sticky.get(key, now_unix));
    // 绑定指向哪个目标：供 `run()` 判断这次到底粘住了没有（§24.1）。
    walk.bound_target_id = existing.as_ref().map(|binding| binding.target_id.clone());

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
        Err(failure) => {
            // 这里没有 `Plan` 可读，但"为什么一个目标都不合格"恰恰是这次请求
            // 唯一有价值的诊断：不写下去，库里就只剩一句"没有可用的调度目标"，
            // 与"网关挂了"在事后看起来一模一样（§24.1）。
            walk.telemetry.filter_summary = failure.filter_summary();
            walk.telemetry.filter_details = failure.filter_details();
            return walk.fail(failure.code, failure.message);
        }
    };
    walk.telemetry.cheapest = plan.cheapest;
    walk.telemetry.dearest = plan.dearest;
    walk.telemetry.filter_summary = Some(plan.filter_summary());
    walk.telemetry.filter_details = plan.filter_details();
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
            // ② 上下文刚被压缩过（输入体积掉了一半以上）：上游那份前缀缓存整段
            // 失效，这一刻换号是免费的，所以不再让迁移冷却挡着。
            let rewritten =
                sticky::context_was_rewritten(binding.context_bytes, forward.request_bytes);
            let cooling = sticky::migrate_cooling_down(now_unix, binding.migrated_at);
            let mut margin =
                sticky::migrate_margin(forward.request_bytes, now_unix - binding.last_used_at);
            // 冷却只限制"多久能搬一次"，不阻止"搬回去"。刚迁移过的键把门槛抬高
            // 三倍，让这段窗口只容得下真正悬殊的差距，避免两个接近的目标隔一个
            // 冷却期就互相翻盘、反复重建前缀缓存。
            if sticky::recently_migrated(now_unix, binding.migrated_at) {
                margin *= sticky::SETTLE_MARGIN_FACTOR;
            }
            (rewritten || !cooling) && routing::pin_is_outpaced(&plan, &binding.target_id, margin)
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

    // 候选全部用尽。最后一次失败是内容审核拒绝时按它报，别掉进下面那句泛化的
    // "目标都失败了"：用户需要知道内容被拦了，而不是以为网关挂了（§13.3）。
    // 记录里的 `upstream_status` 记上游真实给的那个（可能是 400，也可能是 403），
    // 响应状态码则由 `content_filtered` 决定成 422。
    if let Some((upstream, _, _)) = walk.last_filtered.clone()
        && walk
            .last
            .as_ref()
            .is_some_and(|(code, _)| *code == ErrorCode::ContentFiltered)
    {
        let response = walk.filtered_response();
        let status = response.status();
        return walk.finish(
            None,
            status,
            Some(ErrorCode::ContentFiltered),
            Some(upstream),
            response,
        );
    }
    // 候选全部用尽。换过渠道仍是"无法归因的 400"时，把上游那句原样交回——
    // 它才是真实原因，比网关拼一句"目标都失败了"有用（§13.7）。
    if walk.last_opaque.is_some() {
        let response = walk.opaque_response();
        let status = response.status();
        // 兜底响应里的状态码就是上游给的那个（`opaque_response` 用它重建响应），
        // 记下来才不会让这条失败在库里看起来像"网关自己拒绝了请求"。
        return walk.finish(None, status, None, Some(status), response);
    }
    // 否则用最后一次失败的性质决定错误码，让客户端的重试行为正确。
    let (code, message) = walk.last.clone().unwrap_or((
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
                        match self.run(candidate, admission, sticky_key, credential).await {
                            Flow::Continue => {}
                            other => return Err(other),
                        }
                    }
                    Err(reason) if reason.is_queueable() => busy.push(candidate),
                    Err(reason) => self.note_unavailable(candidate, reason),
                },
                // 选不出 Key（全忙或一把能用的都没有）在本层排队等名额：
                // 与 §13.6 同一条规则，忙不等于坏。绑定那把忙也走同一条路——
                // 排队阶段会**只为它**等名额（§2.2 的不变量 B）。
                Picked::Busy | Picked::BoundBusy(_) => busy.push(candidate),
                // 没有可用 Key 是"坏"：这个候选本次彻底不可用。
                Picked::Unavailable(message) => {
                    self.last = Some((ErrorCode::NoEligibleTarget, message));
                    self.last_public = None;
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
                    // 忙：留在 pending 里，稍后按信号量唤醒再试。绑定那把忙
                    // 同样留在这里——下面的名额计算会只为它等。
                    Picked::Busy | Picked::BoundBusy(_) => {
                        index += 1;
                        continue;
                    }
                    Picked::Unavailable(message) => {
                        self.last = Some((ErrorCode::NoEligibleTarget, message));
                        self.last_public = None;
                        pending.remove(index);
                        continue;
                    }
                };
                match self.try_admit(candidate, credential.as_ref()) {
                    Ok(admission) => {
                        pending.remove(index);
                        match self.run(candidate, admission, sticky_key, credential).await {
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
                    // 用短轮询比干等到底更早恢复。绑定的那把忙时它自己就是
                    // 判断对象：它的窗口何时释放决定了要短轮询还是干等。
                    Picked::BoundBusy(key) => Some(key),
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
                                // 必须是**凭据摘要**，不是行 ID：`Registry::keyed`
                                // 用 `credential_id(account_id, digest)` 归类 Key 状态，
                                // 传行 ID 会另开一份永远匹配不上的"影子状态"——
                                // 排队等的是影子，真正那把 Key 的名额照样被占着。
                                key_id: credential
                                    .as_ref()
                                    .map(|key| key.credential_digest.as_str()),
                                ..caller
                            };
                            self.forward
                                .state
                                .runtime
                                .health
                                .capacity(caller, admission_limits(candidate, credential.as_ref()))
                        }
                        // 绑定的那把忙：**只等它**。等"任意一把"会在唤醒后
                        // 重新抽签落到别的 Key 上，上游按凭据隔离的前缀缓存
                        // 照样作废——这正是要修的病（§2.2 的不变量 B）。
                        Picked::BoundBusy(key) => {
                            let caller = health::Caller {
                                key_id: Some(key.credential_digest.as_str()),
                                ..caller
                            };
                            self.forward
                                .state
                                .runtime
                                .health
                                .capacity(caller, admission_limits(candidate, Some(&key)))
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
                    // 忙：名额已经到手，先还回去，回到等待循环。绑定那把忙也
                    // 一样——唤醒的是**它的**名额，拿到手就能直接用它。
                    Picked::Busy | Picked::BoundBusy(_) => continue,
                    Picked::Unavailable(message) => {
                        self.last = Some((ErrorCode::NoEligibleTarget, message));
                        self.last_public = None;
                        pending.remove(index);
                        continue;
                    }
                };
                match self.admit_with_permit(candidate, credential.as_ref(), permit) {
                    Ok(admission) => {
                        pending.remove(index);
                        match self.run(candidate, admission, sticky_key, credential).await {
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
            // 绑定的那把忙：等它，不换 Key（§2.2 的不变量 B）。
            crate::credential::KeyChoice::BoundBusy(key) => Picked::BoundBusy(Arc::clone(key)),
        }
    }

    /// 发起一次尝试并把结果同时喂给健康状态与性能统计。
    ///
    /// 只有 `Continue` 表示"可以换下一个"；其余情况都已经有了最终响应。
    ///
    /// 凭据级失败（401/403、额度耗尽）会在**同一个目标内换成另一把 Key** 重试，
    /// 换 Key 次数受 [`MAX_KEY_SWITCHES_PER_TARGET`] 限制（§4.2.1）。
    ///
    /// `credential` 是**调用方已经选定并据此完成准入**的那把 Key。这里绝不能再
    /// 选一次：调用方（`try_candidates` / `wait_and_run`）选完之后立刻用**那一把**
    /// 的额度做了准入，占的是它的并发名额。重选不仅可能选出另一把（"名额记在 A
    /// 上、请求发给 B"，不变量 A 被破坏），还会让刚占下名额的那把 Key 在自己的
    /// 准入检查里显示"已满"——它会把自己挡在门外。
    async fn run(
        &mut self,
        candidate: &routing::Candidate,
        admission: health::Admission,
        sticky_key: &Option<(sticky::Key, sticky::Origin)>,
        mut credential: Option<Arc<crate::credential::Credential>>,
    ) -> Flow {
        if let Err((code, message)) = self.recheck_multiplier(candidate) {
            // 倍率终检发生在准入之后；终检失败时本次请求从未发给上游，
            // 预扣的 RPM/TPM 必须完整退回，且不能留下半开试运行占用。
            admission.cancel_before_upstream();
            self.last = Some((code, message));
            self.last_public = None;
            // 同 `run_attempt`：这次根本没发出去，"最后一次失败"已被改写。
            self.last_opaque = None;
            return Flow::Continue;
        }
        // 记录"绑定目标到底有没有被用上"（§24.1）：过了倍率终检就一定会发上游，
        // 这是判断粘性是否真的保住的唯一可靠时点。`sticky_hit` 只说明找到过绑定，
        // 软亲和下它在抽签之前就置位，所以会高估粘性。
        // 放在终检之后：终检失败意味着这次根本没发出去，不该记成"做了选择"。
        if self.telemetry.sticky_hit {
            self.telemetry.sticky_kept =
                Some(self.bound_target_id.as_deref() == Some(candidate.target.target.id.as_str()));
        }
        // 粘性/钉住命中的那把要跨重试保留：换 Key 只发生在凭据真的被拒绝之后。
        let mut digest = credential.as_ref().map(|key| key.credential_digest.clone());
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
                    // 用这次尝试已经定下来的判定，而不是在这里重新推导：只有
                    // 那一刻还知道上游给的是 401 还是 403（§12.3）。
                    failed.settle(
                        self.last_credential_outcome
                            .unwrap_or(health::Outcome::KeyInvalid),
                        None,
                    );
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
        // 这一次尝试的上游状态码由 walk_endpoints 内部逐次填写：一个目标可能打
        // 好几次上游（档位重发），每次的拒绝码都不同，最后一次才算这次尝试的。
        // 必须在这里清零，否则上一次尝试的码会粘到下一次的明细上（§6.6）。
        self.telemetry.upstream_status = None;
        self.telemetry.attempt_effort = None;

        let started = Instant::now();
        let result = self.walk_endpoints(candidate, credential.as_ref()).await;

        // 图片端点可能按上游响应形状改了投递方式（见 `attempt`）：成功路径的
        // 结算、评分维度与请求记录都必须用**实际**那一种，否则记录会写"流式"
        // 却按非流式投递，记录页的"首字"也跟着解释不通（§9.3）。
        if let Ok(success) = &result {
            self.streaming = success.streamed;
        }
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
                        self.forward.request_bytes,
                    );
                }
                if self.streaming {
                    // 流式：TPM 回补、性能 EWMA、熔断结果、请求记录与 Responses
                    // 状态链全都等流真正结束再结算（§26.3）。提交时记的那份
                    // 只是"首字延迟"，用它冒充总耗时会系统性高估吞吐。
                    self.telemetry.upstream_status = Some(success.status);
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
                                // 创建这条响应用的那把 Key：续写必须回到它
                                // （§15.1，凭据隔离的资源）。
                                key_id: credential.as_ref().map(|key| key.id.clone()),
                                endpoint: Some(seed.endpoint),
                            },
                            entry_body: self.forward.body.clone(),
                            entry_protocol: self.forward.endpoint.protocol(),
                            retention_days: self.forward.state.settings.get().response_state_days,
                        });
                    // 只有 Chat 的 usage 是「客户端可选」的：网关照例向上游索取
                    // （见 prepare 的 ensure_upstream_usage），客户端没要就不能白给。
                    // Responses / Messages 的用量天然带在正常事件里，不存在额外帧。
                    let strip = self.forward.endpoint.protocol() == Protocol::OpenAiChat
                        && !self.forward.endpoint.is_image()
                        && !forward_body_requests_usage(&self.forward.body);
                    Attempted::Done(Flow::Done(settle::settle_stream(
                        success.response,
                        settle::StreamSettlement {
                            state: self.forward.state.clone(),
                            protocol: self.forward.endpoint.protocol(),
                            idle_timeout: (!self.forward.endpoint.is_image())
                                .then(|| self.forward.state.settings.get().stream_idle_timeout),
                            target_id: candidate.target.target.id.clone(),
                            dimension,
                            started,
                            request_started: self.forward.started_at,
                            first_token: success.first_token,
                            // 心跳路径下由响应体记时（§13.4 修订）；其余路径为空。
                            first_content: success.first_content.clone(),
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
                        strip,
                    )))
                } else {
                    self.usage_parts = success.usage_parts;
                    self.usage_detail = success.usage_detail;
                    self.telemetry.upstream_status = Some(success.status);
                    self.note_attempt(candidate, started, "ok", None, false);
                    admission.settle(health::Outcome::Success, success.usage_tokens);
                    self.forward.state.runtime.perf.observe(
                        &candidate.target.target.id,
                        dimension,
                        &score::Sample {
                            kind: score::SampleKind::Rated,
                            success: true,
                            // 用客户端体感的首字节时间，而不是"响应头之后到首个
                            // 语义事件"那一段：上游先憋响应头时后者接近 0（§9.3）。
                            // 非流式没有首字，退回 None，由总耗时代言。
                            first_token: success.first_byte.or(success.first_token),
                            total: started.elapsed(),
                            output_tokens: success.output_tokens,
                            // 缓存命中率只有非流式才知道——流式在结算时补。
                            cache_hit: success.usage_detail.and_then(stream::cache_hit_rate),
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
            Err(AttemptFailure::AcceptedJob(error)) => {
                tracing::warn!(request_id = self.forward.request_id, account = candidate.target.account.name,
                    detail = %error.message, "已接单的图片任务未交付，不重新下单");
                let code = error.code;
                admission.settle(
                    if code == ErrorCode::UpstreamTimeout {
                        health::Outcome::Neutral
                    } else {
                        health::Outcome::Fault
                    },
                    None,
                );
                self.telemetry.upstream_status = Some(StatusCode::ACCEPTED);
                self.note_attempt(candidate, started, "failed", Some(code), true);
                Attempted::Done(Flow::Done(self.finish(
                    Some(candidate),
                    code.status(),
                    Some(code),
                    Some(StatusCode::ACCEPTED),
                    error.into_response(),
                )))
            }
            Err(AttemptFailure::Terminal(response, upstream_status)) => {
                admission.settle(health::Outcome::Neutral, None);
                let status = response.status();
                self.telemetry.upstream_status = upstream_status;
                self.note_attempt(candidate, started, "failed", None, true);
                Attempted::Done(Flow::Done(self.finish(
                    Some(candidate),
                    status,
                    None,
                    upstream_status,
                    *response,
                )))
            }
            // 内容审核拒绝（§13.3）：上游没开始生成、一个 token 都没消耗，而
            // 同一份请求在别的账号上**能过**——现场 8858B 的请求体在华强北kiro
            // 被拦，3 秒后在 supeai.cc-awsclaude 上 200（2026-10-23）。
            //
            // 所以处置与无法归因的 400 一致：换渠道。三点不同：
            // 1. **不计目标故障**（`Neutral`）：账号没坏，只是不肯处理这份输入，
            //    按 5xx 罚分会把一个健康账号冷却掉，而且冷却了也没用——它还是会拒。
            // 2. **清掉粘性绑定**：现场 9/17 次命中粘在同一个账号上，那段会话每次
            //    重发都被它拒。不清的话"换渠道"只对本次请求生效，下一跳又回去。
            // 3. 只换一次渠道：审核策略是确定的，同一份输入在别的账号上要么过要么
            //    不过，多试几个只是把这几十万字节的请求体再传几遍。
            Err(AttemptFailure::ContentFiltered {
                status,
                message,
                public,
            }) => {
                admission.settle(health::Outcome::Neutral, None);
                self.telemetry.upstream_status = Some(status);
                self.note_attempt(candidate, started, "content_filtered", None, false);
                self.last = Some((
                    ErrorCode::ContentFiltered,
                    format!(
                        "账号「{}」返回 {}：{}",
                        candidate.target.account.name,
                        status.as_u16(),
                        message
                    ),
                ));
                self.last_filtered = Some((status, message, public));
                // 只换一次渠道：审核策略是**确定**的，同一份输入换个账号要么过
                // 要么不过，多试几个只是把几十万字节的请求体再传几遍（§13.3）。
                if self.content_filtered {
                    tracing::warn!(
                        request_id = self.forward.request_id,
                        account = candidate.target.account.name,
                        "换渠道后仍被内容审核拒绝，如实返回"
                    );
                    self.last_opaque = None;
                    return Attempted::Done(Flow::Done(self.filtered_response()));
                }
                self.content_filtered = true;
                if let Some((key, _)) = sticky_key {
                    self.forward.state.runtime.sticky.clear(key);
                }
                tracing::warn!(
                    request_id = self.forward.request_id,
                    account = candidate.target.account.name,
                    model = candidate.target.target.upstream_model,
                    upstream_status = status.as_u16(),
                    "上游内容审核拒绝，清掉粘性并换渠道重试"
                );
                // 后面又发生了别的失败：这次审核拒绝不再是本次请求的最终原因。
                self.last_opaque = None;
                Attempted::Done(Flow::Continue)
            }
            // 上游把会话钉在别的渠道上（§13.3）：换账号可能就过。
            //
            // 与内容审核同为"换个来路可能就过"，所以处置一致：**不计目标故障**
            // （账号没坏，罚分会冷却一个健康账号，而且冷却了也没用）、**清掉粘性
            // 绑定**（不清的话下一跳又回到同一个账号，同一段会话被反复拒）。
            //
            // 与审核不同的是**换几次**：审核策略是确定的，同一份输入换个账号要么
            // 过要么不过，试一次就够；而这里赌的是"下一条渠道恰好认得这个会话"，
            // 是概率问题，多试几个目标才有意义。
            Err(AttemptFailure::SessionUnroutable {
                status,
                message,
                public,
            }) => {
                admission.settle(health::Outcome::Neutral, None);
                self.telemetry.upstream_status = Some(status);
                self.note_attempt(candidate, started, "session_unroutable", None, false);
                self.last = Some((
                    ErrorCode::SessionUnroutable,
                    format!(
                        "账号「{}」返回 {}：{}",
                        candidate.target.account.name,
                        status.as_u16(),
                        message
                    ),
                ));
                // 脱敏后的上游原文放进 `last_public`——`fail()` 会读它，
                // 客户端因此能看到"是哪个账号、上游原话是什么"，而不是一句泛化的
                // "目标都失败了"。
                // **不要**写 `last_filtered`：那个字段是内容审核专用的恢复槽，
                // 只被"最后一次失败是 ContentFiltered"的分支消费，写进来也读不到，
                // 反而会在换渠道后残留、干扰后续判定。
                self.last_public = public;
                // 清掉绑定：留着只会让下一跳又回到这个账号（§10.2）。
                if let Some((key, _)) = sticky_key {
                    self.forward.state.runtime.sticky.clear(key);
                }
                tracing::warn!(
                    request_id = self.forward.request_id,
                    account = candidate.target.account.name,
                    model = candidate.target.target.upstream_model,
                    upstream_status = status.as_u16(),
                    "上游无法把会话路由回原渠道，清掉粘性并换渠道重试"
                );
                self.last_opaque = None;
                Attempted::Done(Flow::Continue)
            }
            // 无法归因的 400：上游没开始生成、没消耗 token，换一个目标再试
            // （§13.7）。`attempted` 里已经记下这个目标，所以外层不会重复选它。
            //
            // 健康判定用 `Neutral`：这种 400 分不清是客户端请求真的有问题还是
            // 上游此刻抽了，**不能**把账算到目标头上——若是客户端的问题，连累
            // 三个健康目标一起降分正是 §12.3 要避免的那类误判。
            Err(AttemptFailure::OpaqueRejection {
                status,
                message,
                public,
            }) => {
                admission.settle(health::Outcome::Neutral, None);
                self.telemetry.upstream_status = Some(status);
                self.note_attempt(candidate, started, "opaque_rejection", None, true);
                // 无论还有没有重试预算，都先记下这一次：它是候选耗尽时的兜底说法。
                self.last = Some((
                    ErrorCode::UnsupportedParameter,
                    format!(
                        "账号「{}」返回 {}：{}",
                        candidate.target.account.name,
                        status.as_u16(),
                        message
                    ),
                ));
                self.last_opaque = Some((status, message, public));
                if self.opaque_rejection_retries >= MAX_OPAQUE_REJECTION_RETRIES {
                    tracing::warn!(
                        request_id = self.forward.request_id,
                        account = candidate.target.account.name,
                        retries = self.opaque_rejection_retries,
                        "换渠道重试后仍是无法归因的 400，如实返回"
                    );
                    return Attempted::Done(Flow::Done(self.opaque_response()));
                }
                self.opaque_rejection_retries += 1;
                tracing::info!(
                    request_id = self.forward.request_id,
                    account = candidate.target.account.name,
                    attempt = self.opaque_rejection_retries,
                    "上游 400 未消耗 token，换渠道重试"
                );
                Attempted::Done(Flow::Continue)
            }
            // 档位阶梯在本目标上走完仍被拒：与无法归因的 400 同处置——上游没
            // 开始生成，换一个渠道再试几乎免费（§14.6、§13.7）。
            //
            // `EffortRejected` 一并接住是防御性的：`walk_endpoints` 现在把
            // 所有"升无可升、试无可试"都翻译成了 `EffortExhausted`，但将来若有人
            // 让这个变体逃出内层循环，行为仍然是"换渠道重试"而不是直接失败。
            Err(
                AttemptFailure::EffortExhausted {
                    status,
                    message,
                    public,
                }
                | AttemptFailure::EffortRejected {
                    status,
                    message,
                    public,
                    ..
                },
            ) => {
                admission.settle(health::Outcome::Neutral, None);
                self.telemetry.upstream_status = Some(status);
                self.note_attempt(candidate, started, "effort_exhausted", None, true);
                // 无论还有没有重试预算，都先记下这一次：它是候选耗尽时的兜底说法。
                self.last = Some((
                    ErrorCode::UnsupportedParameter,
                    format!(
                        "账号「{}」返回 {}：{}",
                        candidate.target.account.name,
                        status.as_u16(),
                        message
                    ),
                ));
                self.last_opaque = Some((status, message.clone(), public));
                if self.effort_exhausted_retries >= MAX_OPAQUE_REJECTION_RETRIES {
                    tracing::warn!(
                        request_id = self.forward.request_id,
                        account = candidate.target.account.name,
                        retries = self.effort_exhausted_retries,
                        upstream_status = status.as_u16(),
                        detail = %message,
                        "换渠道重试后仍是这个思考档位错误，如实返回"
                    );
                    return Attempted::Done(Flow::Done(self.opaque_response()));
                }
                self.effort_exhausted_retries += 1;
                tracing::info!(
                    request_id = self.forward.request_id,
                    account = candidate.target.account.name,
                    model = candidate.target.target.upstream_model,
                    attempt = self.effort_exhausted_retries,
                    upstream_status = status.as_u16(),
                    "上游 400 未消耗 token，换渠道重试"
                );
                Attempted::Done(Flow::Continue)
            }
            // `walk_endpoints` 已经把端点耗尽翻译成了可切换失败。
            Err(AttemptFailure::EndpointRejected { .. }) => {
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
                upstream_body,
            }) => {
                let key: &str = credential.as_ref().map(|c| c.secret.as_ref()).unwrap_or("");
                let redactor = crate::security::redact::ErrorRedactor::new(
                    &candidate.target.account.name,
                    &candidate.target.account.base_url,
                    key,
                );
                self.last_public = Some(
                    upstream_body
                        .as_deref()
                        .and_then(|bytes| serde_json::from_slice(bytes).ok())
                        .map(|value| redactor.payload(&value))
                        .unwrap_or_else(|| redactor.message(&message)),
                );
                let mut outcome = classify_outcome(code, upstream_status, retry_after);
                // 401 与 403 分开看（§12.3）。判据只作用于这两个状态码：402/429
                // 根本不是"凭据不对"的信号，拿它们去查正文会把已经判定的额度耗尽
                // 与限流降级成普通故障，而它们的 `Retry-After` 正等着被消费。
                if let Some(status) = upstream_status
                    && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                {
                    outcome = credential_outcome(
                        status,
                        retry_after,
                        upstream_body.as_deref().unwrap_or(&[]),
                    );
                }
                // 凭据级失败：账号内还有别的 Key 时应当换一把重试，而不是
                // 立刻放弃这个账号（§4.2.1）。这时的 admission 原样交回调用方，
                // 由它把失败结算到**那一把** Key 上——在这里结算会把状态记到
                // 一个已经失败、即将被丢弃的准入上。
                if matches!(outcome, health::Outcome::KeyInvalid)
                    || matches!(outcome, health::Outcome::QuotaExhausted { .. })
                {
                    self.last_retry_after = retry_after;
                    self.last_credential_outcome = Some(outcome);
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
                self.telemetry.upstream_status = upstream_status;
                self.note_attempt(candidate, started, "failed", Some(code), !cheap);
                // 慢到超时不熔断，但可靠性得分必须反映它（§12.1）。
                let counts_for_perf = !matches!(outcome, health::Outcome::Neutral)
                    || code == ErrorCode::UpstreamTimeout;
                if counts_for_perf {
                    self.forward.state.runtime.perf.observe(
                        &candidate.target.target.id,
                        dimension,
                        &score::Sample {
                            kind: score::SampleKind::Rated,
                            success: false,
                            // 走到这里已经排除了"与目标健康无关"的中性失败，
                            // 所以这一次必须反映在可靠性上（§12.1）。
                            first_token: None,
                            total: started.elapsed(),
                            output_tokens: None,
                            // 失败请求没有用例可算命中率。
                            cache_hit: None,
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
                    detail = %crate::security::redact::text(&message),
                    "目标尝试失败，切换到下一个候选"
                );
                self.last = Some((code, message));
                // 后面又发生了**别的**失败：之前那个无法归因的 400 不再是本次
                // 请求的最终原因，留着会在候选耗尽时把它当成结论报出去，把
                // 真正的原因（限流、超时……）吞掉，客户端的重试行为也跟着错。
                self.last_opaque = None;
                Attempted::Done(Flow::Continue)
            }
        }
    }

    /// 按 §14.3 的顺序试这个目标的端点。
    ///
    /// 404/405 只影响本次端点尝试，不能据此屏蔽后续请求或其他模型。
    /// 每个端点最多尝试一次；没有可用转换时保留上游原始错误。
    async fn walk_endpoints(
        &mut self,
        candidate: &routing::Candidate,
        credential: Option<&Arc<crate::credential::Credential>>,
    ) -> Result<Success, AttemptFailure> {
        let mut plan = candidate.endpoints.clone();
        let mut rejected = Vec::new();
        let mut last_rejection = None;

        for _ in 0..endpoints::MAX_ENDPOINTS_PER_TARGET {
            let Some(choice) = plan.first().cloned() else {
                break;
            };
            let mut prepared = self.prepare(candidate, &choice).await?;
            let mut effort_retries = MAX_EFFORT_RETRIES;
            // 本轮已经发出去过的形状。只比「和上一次不同」挡不住 A→B→A 的往复
            // （2026-10-15 现场：high → xhigh → max → xhigh，第三次重发回到一个
            // 已经被拒的档位，白撞一次上游）。
            let mut sent_shapes = vec![adaptive_shape(&prepared.body)];
            loop {
                let timeout = self.deadline.saturating_duration_since(Instant::now());
                // 这一跳的起点：档位重发时用它给被拒的那一次上游调用留一条
                // 明细（§14.6），不然重发的延迟在库里查不到。
                let hop_started = Instant::now();
                self.telemetry.endpoint = Some(prepared.endpoint);
                self.telemetry.degraded = prepared.degraded.clone();
                self.telemetry.sent_effort = adaptive_shape(&prepared.body).effort;
                self.telemetry.attempt_effort = self.telemetry.sent_effort;

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
                    Err(AttemptFailure::EffortRejected {
                        rejected,
                        status,
                        message,
                        public,
                    }) => {
                        // 记下这一档被拒，再按新的事实重发**一次**。
                        //
                        // 重发的内容一定与上一次不同：被拒的那一档在解析器里
                        // 已经变成"升到已确认可用的更高档"或"不指定"。若换算
                        // 结果还是原来那一档（说明没有任何新事实可用），就没有
                        // 理由再打一次上游——但也不该直接失败：同一个模型名下
                        // 可能还有实现不同的渠道（§14.6）。
                        let exhausted = AttemptFailure::EffortExhausted {
                            status,
                            message,
                            public,
                        };
                        if effort_retries == 0 || Instant::now() >= self.deadline {
                            return Err(exhausted);
                        }
                        effort_retries -= 1;
                        // 重新准备失败时**报上游这次的拒绝**，而不是把准备阶段的
                        // 错误抛出去：上游的 400 才是客户端这次请求真正遇到的
                        // 问题，准备阶段的失败只是"没能替它换一档重试"。
                        let Ok(next) = self
                            .prepare_with_reject(candidate, &choice, Some(rejected))
                            .await
                        else {
                            return Err(exhausted);
                        };
                        // 这个形状本轮已经发过就不重发：拿同一个请求再撞一次是
                        // 白费上游。比的是**本轮全部**已发形状，不是相邻两次——
                        // A→B→A 的往复每一跳都与上一次不同，旧写法放它过去。
                        let next_shape = adaptive_shape(&next.body);
                        if sent_shapes.contains(&next_shape) {
                            return Err(exhausted);
                        }
                        sent_shapes.push(next_shape);
                        prepared = next;
                        // 这一次重发是**白烧的一轮上游往返**：记下来，请求记录与
                        // 请求页才看得出"一次请求打了 4 次上游"（§14.6）。
                        self.telemetry.effort_retries += 1;
                        // 被拒的那一次上游调用也要在尝试明细里留痕：只记最终
                        // 那次的话，重发的延迟与上游状态码在库里就查不到了。
                        self.telemetry.upstream_status = Some(status);
                        self.note_attempt(candidate, hop_started, "effort_retry", None, false);
                        self.telemetry.upstream_status = None;
                        tracing::info!(
                            request_id = self.forward.request_id,
                            account = candidate.target.account.name,
                            model = candidate.target.target.upstream_model,
                            rejected_effort = rejected.as_str(),
                            sent_effort = adaptive_shape(&prepared.body)
                                .effort
                                .map(Effort::as_str)
                                .unwrap_or("默认"),
                            "上游不接受这个思考档位，改按可用档位重发"
                        );
                        continue;
                    }
                    Err(AttemptFailure::EndpointRejected { status, body }) => {
                        rejected.push(prepared.endpoint);
                        tracing::info!(
                            request_id = self.forward.request_id,
                            account = candidate.target.account.name,
                            model = candidate.target.target.upstream_model,
                            endpoint = prepared.endpoint.as_str(),
                            upstream_status = status.as_u16(),
                            "本次请求的端点被拒绝，尝试其他可表达的端点"
                        );
                        // 辅助端点（count_tokens / compact / input_tokens）没有
                        // 转换备胎：上游确实没有这条路由时，按 §15.4 明确告诉
                        // 客户端不支持，而不是报一个可重试的 503 让它反复重试。
                        if self.forward.endpoint.is_native_only() {
                            // 端点走错了是**本次尝试**的失败，不是请求本身的：
                            // 但辅助端点没有转换备胎，只能如实告诉客户端。
                            tracing::warn!(
                                request_id = self.forward.request_id,
                                account = candidate.target.account.name,
                                model = candidate.target.target.upstream_model,
                                endpoint = self.forward.endpoint.path(),
                                upstream_status = status.as_u16(),
                                "辅助端点在当前目标上不存在，且没有可转换的备胎"
                            );
                            return Err(AttemptFailure::Terminal(
                                Box::new(
                                    GatewayError::new(
                                        ErrorCode::UnsupportedParameter,
                                        format!(
                                            "当前模型不支持 /{} 端点",
                                            self.forward.endpoint.path()
                                        ),
                                    )
                                    .with_protocol(self.forward.endpoint.protocol())
                                    .with_request_id(self.forward.request_id)
                                    .into_response(),
                                ),
                                Some(status),
                            ));
                        }
                        let detail = upstream_error_message(&body)
                            .map(|message| format!("：{message}"))
                            .unwrap_or_default();
                        last_rejection = Some(AttemptFailure::Switchable {
                            code: ErrorCode::UpstreamExhausted,
                            message: format!(
                                "账号「{}」的 {} 返回 {}{}；本次请求没有其他可用端点",
                                candidate.target.account.name,
                                prepared.endpoint.as_str(),
                                status.as_u16(),
                                detail,
                            ),
                            upstream_status: Some(status),
                            retry_after: None,
                            upstream_body: Some(body),
                        });
                        plan = self.endpoint_plan(candidate, &rejected).unwrap_or_default();
                    }
                    other => return other,
                }
                break;
            }
            if Instant::now() >= self.deadline {
                break;
            }
        }

        Err(last_rejection.unwrap_or_else(|| {
            AttemptFailure::switchable(
                ErrorCode::UpstreamExhausted,
                format!(
                    "账号「{}」没有可用于本次请求的端点",
                    candidate.target.account.name
                ),
            )
        }))
    }

    /// 只排除当前请求已经拒绝过的端点。
    fn endpoint_plan(
        &self,
        candidate: &routing::Candidate,
        rejected: &[Endpoint],
    ) -> Option<Vec<Choice>> {
        endpoints::choices(
            &candidate.target.account,
            self.forward.endpoint,
            self.translation,
            &self.forward.state.runtime.evidence,
            self.forward.group.group.allow_degrade,
            Instant::now(),
            rejected,
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
        self.prepare_with_reject(candidate, choice, None).await
    }

    /// 准备一次尝试；`just_rejected` 是刚刚被上游拒掉的档位（如果有）。
    ///
    /// 档位换算需要知道**客户端要的是哪一档**，而那一档只有发射时才从请求体里
    /// 读得出来，所以这里把"账号上这个模型能接受什么"包成一个解析器交给
    /// [`align_thinking`]，由它在拿到客户端意图之后再定这次该发哪一档（§14.6）。
    async fn prepare_with_reject(
        &self,
        candidate: &routing::Candidate,
        choice: &Choice,
        just_rejected: Option<Effort>,
    ) -> Result<Prepared, AttemptFailure> {
        let evidence = &self.forward.state.runtime.evidence;
        let account_id = &candidate.target.account.id;
        let upstream_model = &candidate.target.target.upstream_model;
        let now = Instant::now();
        // 刚被拒的那一档要立刻算进决策：这次重发不能原样再发一遍。
        if let Some(rejected) = just_rejected {
            evidence.note_effort_rejected(account_id, upstream_model, rejected, now);
        }
        // 快照按值带进发射：它 `Copy` 且与锁无关，闭包因此能安全跨 `await`。
        let facts = evidence.effort_facts(account_id, upstream_model, now);
        self.prepare_inner(candidate, choice, Some(facts)).await
    }

    async fn prepare_inner(
        &self,
        candidate: &routing::Candidate,
        choice: &Choice,
        facts: Option<EffortFacts>,
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
                            MultipartFieldError::Missing => "multipart 请求体缺少 model 字段",
                            MultipartFieldError::UnsafeReplacement => {
                                "上游模型名包含 multipart 不安全字符"
                            }
                        };
                        tracing::warn!(
                            request_id = self.forward.request_id,
                            model = candidate.target.target.upstream_model,
                            detail = message,
                            "multipart 请求体无法改写上游模型名"
                        );
                        AttemptFailure::Terminal(
                            Box::new(
                                GatewayError::new(ErrorCode::UnsupportedParameter, message)
                                    .with_protocol(self.forward.endpoint.protocol())
                                    .with_request_id(self.forward.request_id)
                                    .into_response(),
                            ),
                            None,
                        )
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
                tracing::warn!(
                    request_id = self.forward.request_id,
                    model = candidate.target.target.upstream_model,
                    downstream = ?self.forward.endpoint.protocol(),
                    upstream = ?target_protocol,
                    detail = %reason,
                    "请求无法转换成目标协议的表达"
                );
                AttemptFailure::Terminal(
                    Box::new(
                        GatewayError::new(ErrorCode::UnsupportedParameter, reason.to_string())
                            .with_protocol(self.forward.endpoint.protocol())
                            .with_request_id(self.forward.request_id)
                            .into_response(),
                    ),
                    None,
                )
            })?;
            degraded = emitted.degraded.clone();
            emitted.body.clone()
        };
        rewrite_model(&mut body, &candidate.target.target.upstream_model);
        // 逻辑模型名常常只是一个别名，发射时按它猜出来的思考契约可能与真正要
        // 发出去的上游模型不一致。这里按上游模型名再对齐一次：新模型不接受
        // 手工预算形状，老模型也不认识 adaptive（§14.6）。
        if target_protocol == Protocol::AnthropicMessages
            && let Some(object) = body.as_object_mut()
        {
            let mut extra = degrade::Degradations::default();
            crate::protocol::anthropic::align_thinking(
                object,
                &candidate.target.target.upstream_model,
                downstream,
                self.translation.thinking().as_ref(),
                facts,
                &mut extra,
            );
            for name in extra.into_list() {
                if !degraded.contains(&name) {
                    degraded.push(name);
                }
            }
        }
        // 同协议快路径不经过 `emit_request`，所以"向上游索取 usage"这件事
        // 必须在这里再补一次。网关自己需要 usage：输出速度评分（默认权重 15）
        // 与 TPM 归还都依赖它，而下游客户端多数不会主动写 stream_options
        // （§9.3、§17.2）。多要到的收尾块由响应侧的过滤器决定要不要给客户端。
        // 图片端点不参与：OpenAI 的图片接口没有 `stream_options`，硬塞进去
        // 只会让严格的上游 400；它也没有 Chat 那种"收尾 usage 块"。
        if self.streaming
            && target_protocol == Protocol::OpenAiChat
            && !self.forward.endpoint.is_image()
        {
            ensure_upstream_usage(&mut body);
        }
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
        let shared = self.account_is_shared(&candidate.target.account.id);
        Ok(self
            .forward
            .state
            .runtime
            .health
            .try_admit(
                caller,
                admission_limits(candidate, credential),
                self.estimated_tokens,
            )?
            .with_shared_key(shared))
    }

    /// 这个账号的 Key 池里是不是有多把启用中的 Key（§4.2.1）。
    ///
    /// 只影响 429 的落点：多 Key 账号里限流要同时记在用的那把 Key 上。
    fn account_is_shared(&self, account_id: &str) -> bool {
        self.forward
            .state
            .runtime
            .credentials
            .current()
            .is_shared(account_id)
    }

    fn admit_with_permit(
        &self,
        candidate: &routing::Candidate,
        credential: Option<&Arc<crate::credential::Credential>>,
        permit: health::CapacityPermit,
    ) -> Result<health::Admission, health::Unavailable> {
        let caller = admission_caller(candidate, credential);
        let shared = self.account_is_shared(&candidate.target.account.id);
        Ok(self
            .forward
            .state
            .runtime
            .health
            .admit_with_permit(
                caller,
                admission_limits(candidate, credential),
                self.estimated_tokens,
                permit,
            )?
            .with_shared_key(shared))
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
        self.last_public = None;
        // 同 `run_attempt` 里那条注释：这个目标根本没能发出去，"最后一次失败"
        // 已经被改写，之前那个无法归因的 400 不能再当结论（§13.7）。
        self.last_opaque = None;
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
    /// 把最后一次"无法归因的 400"重建成响应（§13.7）。
    ///
    /// 上游那句原文才是真实原因；网关只补上协议与 request id，不做二次解释。
    fn opaque_response(&self) -> Response {
        let (status, message, public) = self.last_opaque.clone().unwrap_or((
            StatusCode::BAD_REQUEST,
            "上游返回 400".into(),
            None,
        ));
        let mut error = GatewayError::new(ErrorCode::UnsupportedParameter, &message)
            .with_protocol(self.forward.endpoint.protocol())
            .with_request_id(self.forward.request_id);
        if let Some(public) = public {
            error = error.with_public_message(public);
        }
        let mut response = error.into_response();
        *response.status_mut() = status;
        response
    }

    /// 把最后一次内容审核拒绝重建成响应（§13.3）。
    ///
    /// 错误码用 `content_filtered` 而不是 `unsupported_parameter`：客户端要能看出
    /// "这不是我的参数写错了，是这份内容被那个账号的审核策略拦了"。
    /// 状态码用网关自己那套 422，**不**沿用上游给的那个：中途换过渠道，把
    /// 第一家的 400 原样发出去，客户端会以为这是改参数就能解决的错误。上游
    /// 真实状态码照样进请求记录与尝试明细（`upstream_status`）。
    fn filtered_response(&self) -> Response {
        let (_, message, public) = self.last_filtered.clone().unwrap_or((
            StatusCode::UNPROCESSABLE_ENTITY,
            "上游内容审核拒绝了这份请求".into(),
            None,
        ));
        let mut error = GatewayError::new(ErrorCode::ContentFiltered, &message)
            .with_protocol(self.forward.endpoint.protocol())
            .with_request_id(self.forward.request_id);
        if let Some(public) = public {
            error = error.with_public_message(public);
        }
        error.into_response()
    }

    fn fail(&self, code: ErrorCode, message: String) -> Response {
        let mut error = GatewayError::new(code, message)
            .with_protocol(self.forward.endpoint.protocol())
            .with_request_id(self.forward.request_id);
        if self.last.as_ref().is_some_and(|(last, _)| *last == code)
            && let Some(detail) = self.last_public.clone()
        {
            error = error.with_public_message(detail);
        }
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
            sticky_kept: self.telemetry.sticky_kept,
            first_token_ms: None,
            input_tokens: self.usage_parts.0.map(|value| value as i64),
            output_tokens: self.usage_parts.1.map(|value| value as i64),
            config_version: Some(forward.state.config.current().version as i64),
            sticky_wait_ms: self.telemetry.sticky_wait.map(|d| d.as_millis() as i64),
            sticky_freshness: self.telemetry.sticky_freshness,
            sticky_origin: self.telemetry.sticky_origin.map(str::to_string),
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
            filter_details: self.telemetry.filter_details.clone(),
            selected_layer: self.telemetry.selected_layer,
            effort_retries: self.telemetry.effort_retries,
            // 这次请求最终落在哪把 Key 上（§3.3）。诊断字段，绝不含凭据本身。
            key_id: self.telemetry.key_id.clone(),
            key_label: self.telemetry.key_label.clone(),
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
    ///
    /// `upstream_status` 是这一次尝试真正从上游拿到的状态码；网关自己判定的失败
    /// （翻译不了、multipart 改不了名）传 `None`。没有它，"上游 400"与"网关 400"
    /// 在请求记录里长得一模一样，事后只能靠日志猜（2026-10-15 现场）。
    fn note_attempt(
        &mut self,
        candidate: &routing::Candidate,
        started: Instant,
        outcome: &str,
        error_code: Option<ErrorCode>,
        counts_against_budget: bool,
    ) {
        let upstream_status = self.telemetry.upstream_status.map(|s| s.as_u16() as i64);
        let effort = self
            .telemetry
            .attempt_effort
            .map(|e| e.as_str().to_string());
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
            upstream_status,
            effort,
            counts_against_budget,
            // 这一次尝试用的是哪把 Key：换 Key 重试时同一条请求会留下多条
            // 不同 Key 的明细（§7.3）。
            key_id: self.telemetry.key_id.clone(),
            key_label: self.telemetry.key_label.clone(),
        });
        // 成功的尝试就是这个端点"已确认支持"的证据（§14.3 第 1、4 档）。
        // 只认真正服务过的：发出去被拒绝不构成支持证据。
        if outcome == "ok"
            && let Some(endpoint) = self.telemetry.endpoint
        {
            let now = Instant::now();
            self.forward.state.runtime.evidence.note_supported(
                &candidate.target.account.id,
                endpoint,
                now,
            );
            // 这一档是**这个（账号，上游模型）**真的接受过的：记下来，之后客户端
            // 要一个被拒的档位时，才有"升到已确认可用的档位"这条路可走（§14.6）。
            //
            // 不指定档位时 `sent_effort` 是 `None`，什么都不记——"没发档位"
            // 不构成任何档位可用的证据。
            if let Some(effort) = self.telemetry.sent_effort {
                self.forward.state.runtime.evidence.note_effort_accepted(
                    &candidate.target.account.id,
                    &candidate.target.target.upstream_model,
                    effort,
                    now,
                );
            }
        }
    }
}

/// 一次成功尝试的产物。
struct Success {
    status: StatusCode,
    /// 这次**实际**是按流式投递的还是整体缓冲的。
    ///
    /// 图片端点可能按上游响应形状改变投递方式（客户端要流式、上游却回 JSON，
    /// 或反过来），所以它由提交阶段决定，不能再用请求级标志冒充（§9.3 的口径
    /// 一致性：结算、评分维度与请求记录都要说同一种话）。
    streamed: bool,
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
    /// 心跳路径下"首个语义事件"的时钟（§13.4 修订）。
    ///
    /// 只有走心跳的尝试才有：那时 `first_byte` 量到的是第一个注释帧，
    /// 不能当首字延迟用。结算优先读这个时钟，读不到才退回 `first_byte`。
    first_content: Option<stream::FirstContentClock>,
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
        // 模型、资源或接口不存在不代表账号/Key 故障，不能让其他模型一起冷却。
        Some(StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED) => health::Outcome::Neutral,
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

pub(crate) fn unavailable_code(reason: health::Unavailable) -> ErrorCode {
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
    // 只限制提交前的等待。不能把总超时绑在 reqwest 的响应体上：
    // 长思考即使一直有数据，也会在十分钟整被它掐断。
    tokio::time::timeout(
        timeout,
        attempt_inner(forward, target, credential, prepared, streaming, timeout),
    )
    .await
    .unwrap_or_else(|_| {
        Err(AttemptFailure::switchable(
            ErrorCode::UpstreamTimeout,
            "上游超时：未在请求期限内开始响应或完成普通请求".into(),
        ))
    })
}

async fn attempt_inner(
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
    let request = forward
        .state
        .upstream
        .http_for(account.allow_private_network)
        .post(url)
        .headers(headers)
        .body(payload);
    // 图片任务和普通响应仍有总时长边界；推理流提交后的生命期由
    // settle_stream 的空闲超时负责，不能继承这次尝试的剩余总预算。
    let request = if streaming && !prepared.endpoint.is_image() {
        request
    } else {
        request.timeout(timeout)
    };
    let response = request.send().await;

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
        // 只为本次请求尝试其他端点，保留原始响应供失败诊断。
        if endpoints::may_try_other_endpoint(account, prepared.endpoint, status.as_u16()) {
            let body = read_upstream_body(response, MAX_UPSTREAM_BODY_BYTES)
                .await
                .unwrap_or_else(|reason| {
                    tracing::warn!(%reason, "读取端点拒绝响应失败，保留状态码");
                    axum::body::Bytes::new()
                });
            return Err(AttemptFailure::EndpointRejected {
                status,
                body: body.to_vec(),
            });
        }
        return classify_upstream_error(forward, target, prepared, response, status, &api_key)
            .await;
    }
    if status == StatusCode::ACCEPTED
        && prepared.endpoint.is_image()
        && !forward.endpoint.submits_image_task()
    {
        let parsed = crate::gateway::images::complete_job(
            forward.state,
            target,
            &api_key,
            response,
            timeout.saturating_sub(sent_at.elapsed()),
            forward.request_id,
        )
        .await
        .map_err(AttemptFailure::AcceptedJob)?;
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        return Ok(Success {
            status: StatusCode::OK,
            streamed: false,
            response: build_response(
                forward,
                StatusCode::OK,
                &headers,
                prepared,
                Body::from(parsed.to_string()),
            ),
            first_token: Some(sent_at.elapsed()),
            first_byte: None,
            first_content: None,
            output_tokens: stream::output_tokens(&parsed),
            usage_tokens: stream::usage_tokens(&parsed),
            usage_parts: stream::usage_parts(&parsed),
            usage_detail: Some(stream::usage_breakdown(&parsed)),
            stream_state: None,
            degraded: None,
        });
    }
    // 响应头之前已经等掉的时间，之后所有"首字延迟"都必须从这一刻起算。
    let headers_wait = sent_at.elapsed();
    // 图片端点以**上游实际返回的形状**为准（§14.9）：这类端点没有跨协议转换，
    // 上游给什么就该转发什么。
    //
    // 两个方向都会真实发生，而且都不是"损坏响应"：
    // - 客户端要流式、上游却按普通 JSON 回（New API 这类中转的图片接口就不认
    //   stream）：整段缓冲后按 JSON 解析必然失败，而上游其实已经生成过图片了，
    //   可切换的失败还会换号再生成一次，白花一次钱。
    // - 请求没声明流式、上游回了 SSE：按 JSON 解析同样失败。
    //
    // 判据只看上游的内容类型；推理端点保持原样——它们有跨协议转换与严格的流式
    // 语义，请求要流式却拿到普通 JSON 属于真正的协议违约（§13.2）。
    let streamed = if prepared.endpoint.is_image() {
        upstream_streams(&response)
    } else {
        streaming
    };
    // 异步生图任务要记住"是哪把 Key 接的单"：上游的任务按凭据隔离，
    // 轮询换一把 Key 去问，它只会当作没这个任务（§4.2.1 的不变量 A）。
    if streamed {
        commit_stream(
            forward,
            target,
            prepared,
            response,
            status,
            headers_wait,
            crate::security::redact::ErrorRedactor::new(&account.name, &account.base_url, &api_key),
            credential,
        )
        .await
    } else {
        commit_body(
            forward, target, prepared, response, status, sent_at, credential,
        )
        .await
    }
}

/// 上游这次回的是不是 SSE。
fn upstream_streams(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .get(.."text/event-stream".len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("text/event-stream"))
        })
}

/// 流式响应：在第一个**有语义**的事件之前仍可切换（§13.4）。
///
/// 同协议时字节原样转发，一次都不重新编码；跨协议时交给
/// [`translate::commit_stream`]，判据从"上游的字节"换成"中间事件"。
#[allow(clippy::too_many_arguments)]
async fn commit_stream(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    prepared: &Prepared,
    response: reqwest::Response,
    status: StatusCode,
    headers_wait: Duration,
    redactor: crate::security::redact::ErrorRedactor,
    // 这次尝试用的那把 Key（§4.2.1 的不变量 A）：Responses 状态链要记住它。
    credential: Option<&Arc<crate::credential::Credential>>,
) -> Result<Success, AttemptFailure> {
    let downstream = forward.endpoint.protocol();
    let upstream_protocol = prepared.endpoint.protocol();
    let headers = response.headers().clone();

    if upstream_protocol != downstream {
        // 先给内部结算器完整用量，再由 settle_stream 按下游需求移除
        // Chat 的可选 usage 帧；不能在结算前丢掉真实统计。
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
                credential,
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
            include_usage: true,
            account: &target.account.name,
            response,
            responses_id,
            request_id: forward.request_id,
            // 解析阶段丢掉的能力（如 Anthropic 签名）通过它汇总到结算（§14.8）。
            degraded: degraded.clone(),
            redactor: redactor.clone(),
        })
        .await
        {
            Ok(committed) => Ok(Success {
                status,
                streamed: true,
                response: build_response(
                    forward,
                    status,
                    &headers,
                    prepared,
                    crate::gateway::error::private_stream(
                        committed.body,
                        downstream,
                        forward.request_id,
                        redactor.clone(),
                    ),
                ),
                first_token: Some(committed.first_token),
                first_byte: Some(headers_wait + committed.first_token),
                first_content: None,
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
                                credential,
                            )
                            .await;
                            let body = translate::passthrough_responses_stream(
                                prefix,
                                response,
                                &gateway,
                                forward.request_id,
                            );
                            let body = crate::gateway::error::private_stream(
                                body,
                                downstream,
                                forward.request_id,
                                redactor.clone(),
                            );
                            return Ok(Success {
                                status,
                                streamed: true,
                                response: build_response(forward, status, &headers, prepared, body),
                                first_token: first_token_of(started.elapsed()),
                                first_byte: Some(headers_wait + started.elapsed()),
                                first_content: None,
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
    // 首个语义事件的宽限期（§13.4 修订）：期内维持"可切换"，超时则交出响应头、
    // 改用带心跳的转发。上游首字动辄几十秒到几分钟，继续攥着响应头不放会让
    // CDN 把"零字节"判成源站无响应（Cloudflare 约 100 秒后回 524）。
    let grace = tokio::time::Instant::now() + forward.state.settings.get().first_content_grace;

    loop {
        let chunk = match tokio::time::timeout_at(grace, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            // 流在产生任何语义内容前就结束：这是损坏响应（§13.2）。
            Ok(Ok(None)) => {
                return Err(AttemptFailure::switchable(
                    ErrorCode::UpstreamProtocolError,
                    format!(
                        "账号「{}」的流式响应在产生内容前就结束",
                        target.account.name
                    ),
                )
                .with_status(status));
            }
            Ok(Err(error)) => {
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
            // 宽限期用完还没等到语义事件：已经不能再换目标，但也绝不能继续
            // 零字节——那正是 524/522 的直接来源。
            Err(_elapsed) => {
                let mut emitted = 0usize;
                // 先只交出**收全的帧**：心跳是独立的 SSE 注释帧，只能落在帧
                // 边界上。把一个没收全的帧先发出去，紧随其后的心跳会插进它的
                // data 行里，客户端解析出坏 JSON。
                let prefix = sniffer.flush_complete_frames(&mut emitted);
                // 尾帧（没收全的那个）随响应体一起交给下游继续嗅探：
                // 它的后半段还在上游手里，必须留着上下文才拼得回来。
                // 用 flush_unemitted 而不是 take_buffer：后者会把**已经发过的**
                // 完整帧再取出来一次，交给响应体就是重复下发。
                let carry = sniffer.flush_unemitted(&mut emitted);
                // 时钟从**上游响应头到达之前**起算：走心跳的尝试里，
                // "用户等到内容"= 等响应头 + 等首字 + 等语义事件，
                // 只量最后一段会把前面几十秒的等待整段丢掉（§6.6、§9.3）。
                let clock = stream::first_content_clock();
                let body = translate::passthrough_stream_with_keepalive(
                    prefix,
                    carry,
                    response,
                    downstream,
                    forward.request_id,
                    translate::KeepAlive {
                        clock: clock.clone(),
                        interval: forward.state.settings.get().keep_alive_interval,
                        elapsed_before: headers_wait + started.elapsed(),
                    },
                );
                let body = crate::gateway::error::private_stream(
                    body,
                    downstream,
                    forward.request_id,
                    redactor.clone(),
                );
                return Ok(Success {
                    status,
                    streamed: true,
                    response: build_response(forward, status, &headers, prepared, body),
                    // 首个语义事件此刻还没到：不用心跳的时间冒充首字，
                    // 由响应体在真的看到语义内容时记时（§6.6）。
                    first_token: None,
                    first_byte: Some(headers_wait + started.elapsed()),
                    first_content: Some(clock),
                    output_tokens: None,
                    usage_tokens: None,
                    usage_parts: (None, None),
                    usage_detail: None,
                    stream_state: None,
                    degraded: None,
                });
            }
        };
        match sniffer.push(&chunk) {
            stream::Verdict::Pending => continue,
            stream::Verdict::Semantic => {
                let prefix = sniffer.take_buffer();
                let body =
                    translate::passthrough_stream(prefix, response, downstream, forward.request_id);
                let body = crate::gateway::error::private_stream(
                    body,
                    downstream,
                    forward.request_id,
                    redactor.clone(),
                );
                return Ok(Success {
                    status,
                    streamed: true,
                    response: build_response(forward, status, &headers, prepared, body),
                    first_token: first_token_of(started.elapsed()),
                    first_byte: Some(headers_wait + started.elapsed()),
                    first_content: None,
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
    sent_at: Instant,
    // 这次尝试用的那把 Key（§4.2.1 的不变量 A）：异步生图任务要记住它，
    // Responses 状态链同样要记住它。
    credential: Option<&Arc<crate::credential::Credential>>,
) -> Result<Success, AttemptFailure> {
    let key_digest = credential.map(|credential| credential.credential_digest.as_str());
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

    if parsed.get("error").is_some_and(|error| !error.is_null()) {
        return Err(AttemptFailure::switchable(
            ErrorCode::UpstreamProtocolError,
            "供应商返回了错误对象".into(),
        )
        .with_status(status));
    }

    // Responses 的非流式失败对象：HTTP 200 + `status: "failed"` 是上游表达
    // "这次回答失败"的另一种形状（§13.2 修订）。字节还没发给下游，可以换号；
    // 当成成功透传则下游会收到一份 200 的失败响应，与流式那条是同一种事故。
    if matches!(
        parsed.get("status").and_then(serde_json::Value::as_str),
        Some("failed" | "error")
    ) {
        let message =
            upstream_error_message(&bytes).unwrap_or_else(|| "上游返回了失败响应".to_string());
        return Err(AttemptFailure::switchable(
            ErrorCode::UpstreamProtocolError,
            format!("账号「{}」的响应失败：{message}", target.account.name),
        )
        .with_status(status));
    }

    // 异步生图下单成功：把上游任务 ID 记到接单账号与**那一把 Key** 上，
    // 客户端轮询才找得到人（§14.9、§4.2.1 的不变量 A）。
    if forward.endpoint.submits_image_task() {
        remember_image_task(forward, target, key_digest, &parsed).await;
    }

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
                credential,
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
                credential,
            )
            .await;
            Body::from(value.to_string())
        } else {
            Body::from(converted.to_string())
        }
    };

    Ok(Success {
        status,
        streamed: false,
        response: build_response(forward, status, &headers, prepared, body),
        // 非流式没有"首字事件"，用户等到**完整响应体**才算拿到内容。这一项
        // 必须进样本，否则只要某个模型的样本落在"非流式"维度上，首字维
        // （默认权重 20）就永远取中性分——一个恒定常数，等于白给（§9.3）。
        //
        // 基准与流式的 first_byte 对齐：都从**发出上游请求**算起，不含网关
        // 排队（排队另有 queued_ms 记录），两个维度才可比（§6.6）。
        first_token: Some(sent_at.elapsed()),
        first_byte: None,
        first_content: None,
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
#[allow(clippy::too_many_arguments)]
async fn record_response_state(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    prepared: &Prepared,
    gateway_id: &str,
    upstream_body: &serde_json::Value,
    final_response: Option<&serde_json::Value>,
    upstream_protocol: Protocol,
    // 创建这条响应用的那把 Key（§4.2.1 的不变量 A、§15.1）。上游的
    // `resp_xxx` 按凭据隔离，续写与生命周期代理必须回到同一把 Key。
    credential: Option<&Arc<crate::credential::Credential>>,
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
            key_id: credential.map(|key| key.id.clone()),
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

/// 异步生图任务定位记录的有效期（秒）。
///
/// 上游自己的任务也有过期时间；这里取一天，够客户端把一次 4K 出图轮询完，
/// 又不至于让定位表无限增长。
const IMAGE_TASK_TTL_SECS: i64 = 24 * 60 * 60;

/// 记下"这个上游任务 ID 是哪个账号接的单"（§14.9）。
///
/// 只在成功下单时调用：失败响应里没有可轮询的任务 ID。写失败只记日志——
/// 轮询是增强能力，不能反过来把已经成功的下单变成失败。
async fn remember_image_task(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    key_digest: Option<&str>,
    body: &serde_json::Value,
) {
    let Some(task_id) = body
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        tracing::warn!(
            request_id = forward.request_id,
            account = target.account.name,
            "异步生图下单响应里没有任务 ID，后续轮询将无法定位"
        );
        return;
    };
    let now = crate::storage::now_unix();
    let row = crate::storage::store::ImageTaskRow {
        task_id: task_id.to_string(),
        group_id: forward.group.group.id.clone(),
        account_id: target.account.id.clone(),
        target_id: Some(target.target.id.clone()),
        upstream_model: Some(target.target.upstream_model.clone()),
        key_digest: key_digest.map(str::to_string),
        created_at: now,
        expires_at: now.saturating_add(IMAGE_TASK_TTL_SECS),
    };
    if let Err(error) = forward.state.store.upsert_image_task(&row).await {
        tracing::warn!(
            %error,
            request_id = forward.request_id,
            "写入异步生图任务定位失败"
        );
    }
}

/// 把上游的非 2xx 响应分成"可切换"与"必须直接返回下游"两类。
async fn classify_upstream_error(
    forward: &Forward<'_>,
    target: &Arc<TargetView>,
    prepared: &Prepared,
    response: reqwest::Response,
    status: StatusCode,
    api_key: &str,
) -> Result<Success, AttemptFailure> {
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        // 上游头部不可信：超出时钟可表示范围的等待时间必须视为无效，
        // 否则健康结算中的 Instant + Duration 会 panic，打断故障切换。
        .filter(|seconds| {
            Instant::now()
                .checked_add(Duration::from_secs(*seconds))
                .is_some()
        });

    if is_switchable_status(status) {
        let code = match status {
            StatusCode::TOO_MANY_REQUESTS => ErrorCode::RateLimited,
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => ErrorCode::UpstreamTimeout,
            _ => ErrorCode::UpstreamExhausted,
        };
        // 判据必须看正文：403 到底是"分组被停用"还是"Key 无效"，状态码本身
        // 分不出来（§12.3）。体有上限，异常上游不能靠超大错误体把网关拖垮。
        let body = read_upstream_body(response, MAX_UPSTREAM_BODY_BYTES)
            .await
            .unwrap_or_else(|reason| {
                tracing::warn!(%reason, "读取上游错误响应体失败，按空体处理");
                axum::body::Bytes::new()
            });
        // 403 也可能是内容审核（现场：`Request was blocked by upstream content
        // filtering`）。它必须先于下面的凭据判定：按 403 处理会把一个**没有**坏
        // 的账号判成 Key 失效/故障，冷却之后照样拒（§12.3、§13.3）。
        let text = upstream_error_message(&body)
            .unwrap_or_else(|| format!("上游返回 {}", status.as_u16()));
        // 上游把会话钉在别的渠道上（§13.3）：换账号可能就过。先于凭据判定，
        // 否则会被当成账号级故障，把一个健康账号冷却掉（冷却了也没用）。
        if is_session_unroutable(&text) {
            tracing::warn!(
                request_id = forward.request_id,
                account = target.account.name,
                model = target.target.upstream_model,
                upstream_status = status.as_u16(),
                "上游无法把会话路由回原渠道，换渠道重试"
            );
            let public = crate::security::redact::ErrorRedactor::new(
                &target.account.name,
                &target.account.base_url,
                api_key,
            )
            .payload(&serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null));
            return Err(AttemptFailure::SessionUnroutable {
                status,
                message: text,
                public: Some(public),
            });
        }
        if is_content_filtered(status, &text) {
            tracing::warn!(
                request_id = forward.request_id,
                account = target.account.name,
                model = target.target.upstream_model,
                upstream_status = status.as_u16(),
                "上游内容审核拒绝，换渠道重试"
            );
            let public = crate::security::redact::ErrorRedactor::new(
                &target.account.name,
                &target.account.base_url,
                api_key,
            )
            .payload(&serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null));
            return Err(AttemptFailure::ContentFiltered {
                status,
                message: text,
                public: Some(public),
            });
        }
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
            upstream_body: Some(body.to_vec()),
        });
    }

    // 参数错误保留 HTTP 状态；供应商正文只进入服务端诊断，不向客户透传。
    // 错误体同样有上限：异常上游不能靠一个超大错误体把网关拖垮。
    let bytes = read_upstream_body(response, MAX_UPSTREAM_BODY_BYTES)
        .await
        .unwrap_or_else(|reason| {
            tracing::warn!(%reason, "读取上游错误响应体失败，按空体处理");
            axum::body::Bytes::new()
        });
    let message =
        upstream_error_message(&bytes).unwrap_or_else(|| format!("上游返回 {}", status.as_u16()));
    // 形状已经发对了却还是收到"requires adaptive thinking"，被拒的是**档位**：
    // 这是（账号，上游模型）级的事实，记下来让重试与之后的请求都换一个这个账号
    // 能接受的档位（§14.6）。
    let shape = adaptive_shape(&prepared.body);
    // 没有 `output_config.effort` 说明这次走的正是 `Omit`（不指定档位）。
    // 它也失败了，就必须记下来——否则 `plan()` 永远不会走到"降档"那一步，
    // 会在同一个 `Omit` 上反复重试（§14.6 修订）。
    if adaptive_effort_rejection(status, &message, shape) && shape.effort.is_none() {
        forward.state.runtime.evidence.note_omitted_failed(
            &target.account.id,
            &target.target.upstream_model,
            Instant::now(),
        );
    }
    if adaptive_effort_rejection(status, &message, shape)
        && let Some(effort) = shape.effort
    {
        // 先记下这次拒绝，再由调用方按新事实决定重发什么（§14.6）。
        forward.state.runtime.evidence.note_effort_rejected(
            &target.account.id,
            &target.target.upstream_model,
            effort,
            Instant::now(),
        );
        let public = crate::security::redact::ErrorRedactor::new(
            &target.account.name,
            &target.account.base_url,
            api_key,
        )
        .payload(&serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null));
        let mut response = GatewayError::new(ErrorCode::UnsupportedParameter, &message)
            .with_public_message(public.clone())
            .with_protocol(forward.endpoint.protocol())
            .with_request_id(forward.request_id)
            .into_response();
        *response.status_mut() = status;
        return Err(AttemptFailure::EffortRejected {
            rejected: effort,
            status,
            message,
            public: Some(public),
        });
    }
    // 形状已经对了、档位已经降到"不指定"，上游还是回同一句：这句话就与请求
    // 无关了——是这条渠道自己的实现问题（现场：同一个中转站的一条渠道把
    // thinking 改写成旧的 enabled 形状，无论 Akhub 发什么档位都回这句话）。
    // 换一个目标再试几乎免费（§13.7）。
    if status == StatusCode::BAD_REQUEST
        && shape.adaptive
        && shape.effort.is_none()
        && message.contains("adaptive thinking")
    {
        tracing::warn!(
            request_id = forward.request_id,
            account = target.account.name,
            model = target.target.upstream_model,
            upstream_status = status.as_u16(),
            detail = %message,
            "上游连不指定档位的自适应思考也拒绝，换渠道重试"
        );
        let public = crate::security::redact::ErrorRedactor::new(
            &target.account.name,
            &target.account.base_url,
            api_key,
        )
        .payload(&serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null));
        return Err(AttemptFailure::EffortExhausted {
            status,
            message,
            public: Some(public),
        });
    }
    // 会话被上游钉在别的渠道上（§13.3）：换账号可能就过，且不是账号故障。
    // 必须同样先于 `is_opaque_rejection`：这句话里没有可归因的参数名，
    // 落进 opaque 会被当成"上游抽了"，走不到"换渠道"这条正确路径。
    if is_session_unroutable(&message) {
        tracing::warn!(
            request_id = forward.request_id,
            account = target.account.name,
            model = target.target.upstream_model,
            upstream_status = status.as_u16(),
            "上游无法把会话路由回原渠道，换渠道重试"
        );
        let public = crate::security::redact::ErrorRedactor::new(
            &target.account.name,
            &target.account.base_url,
            api_key,
        )
        .payload(&serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null));
        return Err(AttemptFailure::SessionUnroutable {
            status,
            message,
            public: Some(public),
        });
    }
    // 内容审核拒绝（§13.3）：上游没开始生成、没消耗 token，而这份输入在别的
    // 账号上可能过得了。必须先于 `is_opaque_rejection` 判断——那句英文里既有
    // "blocked"（不在可归因清单里）也不在"泛泛的拒绝"清单里，落进下面任何一条
    // 都会被当成"客户端参数错了"或"上游抽了"。
    if is_content_filtered(status, &message) {
        tracing::warn!(
            request_id = forward.request_id,
            account = target.account.name,
            model = target.target.upstream_model,
            upstream_status = status.as_u16(),
            "上游内容审核拒绝，换渠道重试"
        );
        let public = crate::security::redact::ErrorRedactor::new(
            &target.account.name,
            &target.account.base_url,
            api_key,
        )
        .payload(&serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null));
        return Err(AttemptFailure::ContentFiltered {
            status,
            message,
            public: Some(public),
        });
    }
    // 无法归因的 400：上游在开始生成之前就拒了，**没有消耗任何 token**。
    // 换一个渠道重试几乎免费，而直接透给客户端就把"上游此刻抽了"变成了用户可见
    // 的失败（§13.7）。归因得到请求本身的 400 不走这条路。
    if is_opaque_rejection(status, &message) {
        tracing::warn!(
            request_id = forward.request_id,
            account = target.account.name,
            model = target.target.upstream_model,
            upstream_status = status.as_u16(),
            detail = %message,
            "上游返回无法归因的 400，换渠道重试"
        );
        let public = crate::security::redact::ErrorRedactor::new(
            &target.account.name,
            &target.account.base_url,
            api_key,
        )
        .payload(&serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null));
        return Err(AttemptFailure::OpaqueRejection {
            status,
            message,
            public: Some(public),
        });
    }
    tracing::warn!(request_id = forward.request_id, account = target.account.name,
        upstream_status = status.as_u16(), detail = %message, "上游拒绝请求");
    let mut error = GatewayError::new(ErrorCode::UnsupportedParameter, &message)
        .with_public_message(
            crate::security::redact::ErrorRedactor::new(
                &target.account.name,
                &target.account.base_url,
                api_key,
            )
            .payload(&serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)),
        )
        .with_protocol(forward.endpoint.protocol())
        .with_request_id(forward.request_id);
    if let Some(seconds) = retry_after {
        error = error.with_retry_after(seconds);
    }
    let mut response = error.into_response();
    *response.status_mut() = status;
    Err(AttemptFailure::Terminal(Box::new(response), Some(status)))
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

/// 上游这个 400 是不是**无法归因**到请求本身（§13.7）。
///
/// 判据是"消息里指不出任何具体东西"：没有参数名、没有字段名、没有模型名，只有
/// 一句泛泛的"请求无法处理"。这类 400 分不清是客户端请求真的有问题，还是上游
/// 此刻自己抽了——而 400 发生在开始生成之前、**不消耗 token**，所以换渠道重试
/// 的代价只有一次请求体重传，比把真实故障当参数错误报出去小得多。
///
/// 反过来，只要能归因到请求本身，就绝不重试：`prompt is too long`、
/// `field ReasoningEffort invalid`、`max_tokens must be greater than` 这些
/// 换到哪个目标都是同一个结果，重试只会白传一遍请求体。
/// 上游这句 4xx 是不是**内容审核**拒绝了这份输入（§13.3）。
///
/// 判据取上游各自的原文，不看状态码：中转站有的回 400，有的回 422/403，还有的
/// 回 200 然后在流里报错——状态码不构成判据，那句话才是。
///
/// 这句话的特征是"说得出是谁拦的、说不出改哪个字段"：命中 `CONTENT_FILTER` 里
/// 任一句就是审核拒绝，同时要求它**不**指向具体参数，否则 `invalid content_type`
/// 这类真·参数错误会被误判成审核。
/// 上游把会话钉在某个账号/渠道上了，换账号就路由不过去（§13.3）。
///
/// 现场（2026-10-25，ccmax / opus-5-5 / wishtoapp）：
/// `This conversation can no longer be routed to its original account.`
/// `Start a new Claude Code session.`
///
/// 这是**渠道挂载关系**的问题，不是账号坏了，也不是请求写错了：上游保存了会话
/// 的加密上下文，只有当初接单的那条渠道解得开。换一个账面试，可能就落回它自己
/// 的渠道上（同一账号的不同 target 也可能是不同渠道），所以值得换渠道重试。
fn is_session_unroutable(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    const UNROUTABLE: &[&str] = &[
        "can no longer be routed",
        "routed to its original account",
        "start a new claude code session",
    ];
    UNROUTABLE.iter().any(|needle| lower.contains(needle))
}

fn is_content_filtered(status: StatusCode, message: &str) -> bool {
    if status.is_server_error() {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    const CONTENT_FILTER: &[&str] = &[
        "content filtering",
        "content filter",
        "content_filter",
        "content_policy",
        "content policy",
        "moderation",
        "safety system",
        "blocked by upstream",
        "responsible ai",
        // 国内中转站大多直接用中文回这句：同一个判据。
        "内容审核",
        "敏感词",
        "内容安全",
        "违规内容",
    ];
    if !CONTENT_FILTER.iter().any(|needle| lower.contains(needle)) {
        return false;
    }
    const ATTRIBUTABLE: &[&str] = &[
        "content_type",
        "invalid",
        "must be",
        "required",
        "missing",
        "unsupported",
        "not supported",
        "parameter",
    ];
    !ATTRIBUTABLE.iter().any(|needle| lower.contains(needle))
}
fn is_opaque_rejection(status: StatusCode, message: &str) -> bool {
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    // 指名道姓的字段/参数：上游已经告诉客户端该改什么了，这不是它的抖动。
    const ATTRIBUTABLE: &[&str] = &[
        "prompt is too long",
        "too long",
        "too many tokens",
        "context length",
        "maximum context",
        "invalid",
        "must be",
        "cannot be",
        "unsupported",
        "not supported",
        "exceeds",
        "exceed",
        "required",
        "missing",
        "unknown",
        "unexpected",
        "reasoningeffort",
        "max_tokens",
        "temperature",
        "top_p",
        "top_k",
        "budget_tokens",
        "output_config",
        "thinking",
        "tool",
        "image",
        "message",
    ];
    if ATTRIBUTABLE.iter().any(|needle| lower.contains(needle)) {
        return false;
    }
    // 还要求它确实是"泛泛的拒绝"，而不是某个我们没见过的具体说法。
    // 现场那句是唯一已知的形态；放宽到"任意短消息"会把未知的参数错误也卷进来。
    const OPAQUE: &[&str] = &[
        "could not be processed",
        "could not process",
        "check the request parameters",
        "bad request",
    ];
    OPAQUE.iter().any(|needle| lower.contains(needle))
}

/// 上游这句 400 是不是"自适应思考的档位/形状不被这个账号接受"（§14.6）。
///
/// 现场形态（supeai.top 的 claude-opus-5-5）：同一个上游站的两个域名实现不同，
/// 一个收 `low/medium/high/max`，另一个只收 `low`，其余档位回同一句
/// `claude-opus-5-5 requires adaptive thinking; omit thinking or use
/// thinking.type=adaptive and output_config.effort`。它和"形状不对"共用一句话，
/// 所以只能连形状一起判：形状已经对了还收到这句话，被拒的就是**档位**。
fn adaptive_effort_rejection(status: StatusCode, message: &str, shape: AdaptiveShape) -> bool {
    status == StatusCode::BAD_REQUEST
        && shape.adaptive
        && shape.effort.is_some()
        && message.contains("adaptive thinking")
}

/// 本次发出去的思考形状（判定"被拒的是档位"要看它）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AdaptiveShape {
    adaptive: bool,
    effort: Option<crate::protocol::canonical::Effort>,
}

/// 从已经准备好的请求体里读回这次发射的形状。
fn adaptive_shape(body: &serde_json::Value) -> AdaptiveShape {
    let adaptive = body
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("adaptive");
    let effort = body
        .get("output_config")
        .and_then(|config| config.get("effort"))
        .and_then(serde_json::Value::as_str)
        .and_then(crate::protocol::canonical::Effort::parse);
    AdaptiveShape { adaptive, effort }
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

/// 下游这份请求体有没有主动索取 usage（Chat 的 `stream_options.include_usage`）。
///
/// 决定「上游被我们多要来的那个收尾块要不要转发给客户端」：谁要谁得。
fn forward_body_requests_usage(body: &serde_json::Value) -> bool {
    body.get("stream_options")
        .and_then(|options| options.get("include_usage"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// 向上游索取 usage：置 `stream_options.include_usage = true`。
///
/// 保留 `stream_options` 里已有的其他字段（供应商扩展），只动这一项。
fn ensure_upstream_usage(body: &mut serde_json::Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let options = object
        .entry("stream_options")
        .or_insert_with(|| serde_json::json!({}));
    if !options.is_object() {
        // 客户端把 stream_options 写成了非对象（畸形请求）：整个替换掉，
        // 否则加不进去。这属于修复而不是篡改，网关自己需要这一项。
        *options = serde_json::json!({});
    }
    if let Some(map) = options.as_object_mut() {
        map.insert("include_usage".into(), serde_json::json!(true));
    }
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
///
/// 图片端点是例外：它们的工作量在图片本体里，而图片字节不是 Token。按请求体
/// 字节折算会把一张 4 MB 的图记成 130 万 Token——凡是配了 TPM 的分组，一次
/// 图生图就会被判成"超出 TPM 限额"，而且永远等不到窗口释放（单条请求就超过
/// 整个窗口）。图片请求只按**文本字段**估算：JSON 入口数 prompt 一类的短字符串，
/// multipart 入口只数非文件 part。
fn estimate_tokens(
    endpoint: Endpoint,
    request_bytes: usize,
    body: &serde_json::Value,
    raw: Option<&RawBody>,
) -> u64 {
    let input = if endpoint.is_image() {
        image_text_bytes(body, raw).div_ceil(BYTES_PER_TOKEN) as u64
    } else {
        (request_bytes / BYTES_PER_TOKEN) as u64
    };
    let output = body
        .get("max_tokens")
        .or_else(|| body.get("max_output_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    input.saturating_add(output)
}

/// 超过这个长度的字符串按内联图片（base64/data URL）处理，不计入 Token 估算。
///
/// 提示词不会写到 4 KiB 以上；宁可少算一次提示词，也不能把图片本体当成 token。
const MAX_TOKEN_TEXT_BYTES: usize = 4 * 1024;

/// 图片请求里的文本字节数：内联 data URL 与超长字符串都按图片本体处理。
fn image_text_bytes(body: &serde_json::Value, raw: Option<&RawBody>) -> usize {
    if let Some(raw) = raw {
        return multipart_text_bytes(&raw.bytes);
    }
    let Some(object) = body.as_object() else {
        return 0;
    };
    object
        .values()
        .filter_map(serde_json::Value::as_str)
        .filter(|text| !text.starts_with("data:") && text.len() <= MAX_TOKEN_TEXT_BYTES)
        .map(str::len)
        .sum()
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

/// multipart 字段的解析错误。图片端点只需要识别少数几个字段，
/// 不试图实现完整 multipart 语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultipartFieldError {
    Missing,
    UnsafeReplacement,
}

/// 从完整 multipart 正文中提取 `Content-Disposition` 的 `name="model"` 字段。
///
/// 解析按 boundary 行与 CRLF/LF 分隔进行，调用方已经把请求体完整读入内存，
/// 因而不会受网络分块边界影响。返回值会去掉字段值两端的空白。
pub fn extract_multipart_model(body: &[u8]) -> Result<String, MultipartFieldError> {
    extract_multipart_field(body, "model")
}

/// 提取 multipart 正文里任意一个普通字段（按 `Content-Disposition` 的 name 匹配）。
///
/// 字段顺序无关紧要：图片端点允许 `model` 出现在图片之后，`stream` 之类的
/// 控制字段同样如此。匹配只看 `name`，所以图片 part（`name="image"`）不会
/// 被当成 `model`；只有客户端把文件 part 也命名为同一个字段时才需要额外区分。
pub fn extract_multipart_field(body: &[u8], field: &str) -> Result<String, MultipartFieldError> {
    let range = multipart_field_range(body, field)?;
    let value = std::str::from_utf8(&body[range])
        .map_err(|_| MultipartFieldError::Missing)?
        .trim();
    if value.is_empty() {
        return Err(MultipartFieldError::Missing);
    }
    Ok(value.to_string())
}

/// multipart 里有没有要求流式返回（`stream: true` 这类表单字段）。
///
/// 图片端点必须认这个字段：`gpt-image-1` 的流式改图把 `stream` 放在
/// multipart 正文里，响应是 SSE。不认它就会把 SSE 当 JSON 解析，整条请求
/// 在上游已经生成图片之后才失败（还要白花一次生成的钱）。
pub fn multipart_requests_stream(body: &[u8]) -> bool {
    extract_multipart_field(body, "stream").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        )
    })
}

/// multipart 正文里**非文件**字段的字节数。
///
/// 用于图片端点的工作量估算：提示词、尺寸这些文本字段算数，图片本体不算。
/// 把几 MB 的图片按 3 字节一个 token 折算，一次图生图会被记成上百万 token，
/// 直接把带 TPM 限额的分组挡在门外。
pub fn multipart_text_bytes(body: &[u8]) -> usize {
    let Ok((boundary, mut cursor)) = multipart_first_boundary(body) else {
        return 0;
    };
    let mut total = 0usize;
    loop {
        if cursor >= body.len() || !body[cursor..].starts_with(&boundary) {
            return total;
        }
        let after_boundary = cursor + boundary.len();
        if body[after_boundary..].starts_with(b"--") {
            return total;
        }
        let Some((_, headers_start)) = multipart_line(body, cursor) else {
            return total;
        };
        let mut line_start = headers_start;
        let mut is_file = false;
        let value_start = loop {
            let Some((line_end, next)) = multipart_line(body, line_start) else {
                return total;
            };
            if line_end == line_start {
                break next;
            }
            if multipart_disposition_has_filename(&body[line_start..line_end]) {
                is_file = true;
            }
            line_start = next;
        };
        let Some(next_boundary) = multipart_find_boundary(body, value_start, &boundary) else {
            return total;
        };
        if !is_file {
            let value_end = multipart_trim_delimiter_newline(body, value_start, next_boundary);
            total = total.saturating_add(value_end.saturating_sub(value_start));
        }
        cursor = next_boundary;
    }
}

/// 这个 header 行是不是带 `filename` 的 `Content-Disposition`（文件 part）。
fn multipart_disposition_has_filename(line: &[u8]) -> bool {
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return false;
    };
    if !trim_ascii(&line[..colon]).eq_ignore_ascii_case(b"content-disposition") {
        return false;
    }
    line[colon + 1..]
        .split(|byte| *byte == b';')
        .any(|segment| {
            segment
                .iter()
                .position(|byte| *byte == b'=')
                .is_some_and(|equal| {
                    trim_ascii(&segment[..equal]).eq_ignore_ascii_case(b"filename")
                })
        })
}

/// 只替换 multipart `model` part 的内容，保留其它字节与 boundary 原样。
///
/// 上游模型名若含控制字符会改变 multipart 的结构（例如注入换行或新的
/// boundary），因此直接拒绝，而不是把不安全字节写进正文。
pub fn rewrite_multipart_model(
    body: &[u8],
    upstream_model: &str,
) -> Result<Vec<u8>, MultipartFieldError> {
    if upstream_model.is_empty() || upstream_model.chars().any(char::is_control) {
        return Err(MultipartFieldError::UnsafeReplacement);
    }
    let range = multipart_field_range(body, "model")?;
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

/// 找到某个字段 part 的值范围（不包含值前后的 multipart 分隔换行）。
fn multipart_field_range(body: &[u8], field: &str) -> Result<Range<usize>, MultipartFieldError> {
    let (boundary, mut cursor) = multipart_first_boundary(body)?;

    loop {
        if cursor >= body.len() || !body[cursor..].starts_with(&boundary) {
            return Err(MultipartFieldError::Missing);
        }
        let after_boundary = cursor + boundary.len();
        if body[after_boundary..].starts_with(b"--") {
            return Err(MultipartFieldError::Missing);
        }

        // 当前 boundary 行后面必须紧跟换行，之后才是 part headers。
        let (_, headers_start) =
            multipart_line(body, cursor).ok_or(MultipartFieldError::Missing)?;
        let mut line_start = headers_start;
        let mut is_field = false;
        let value_start = loop {
            let (line_end, next) =
                multipart_line(body, line_start).ok_or(MultipartFieldError::Missing)?;
            if line_end == line_start {
                break next;
            }
            if multipart_field_disposition(&body[line_start..line_end], field) {
                is_field = true;
            }
            line_start = next;
        };

        let next_boundary = multipart_find_boundary(body, value_start, &boundary)
            .ok_or(MultipartFieldError::Missing)?;
        let value_end = multipart_trim_delimiter_newline(body, value_start, next_boundary);
        if is_field {
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
fn multipart_first_boundary(body: &[u8]) -> Result<(Vec<u8>, usize), MultipartFieldError> {
    if !body.starts_with(b"--") {
        return Err(MultipartFieldError::Missing);
    }
    let (line_end, _) = multipart_line(body, 0).ok_or(MultipartFieldError::Missing)?;
    if line_end <= 2 {
        return Err(MultipartFieldError::Missing);
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

/// 判断一个 header 行是否声明了 `name="<field>"` 的 form-data part。
fn multipart_field_disposition(line: &[u8], field: &str) -> bool {
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
        return value == field.as_bytes();
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
    fn an_unattributable_400_is_worth_retrying_on_another_channel() {
        // 现场那句：上游没说是哪个参数，也没说是形状问题。400 发生在开始生成
        // 之前、不消耗 token，所以换渠道重试几乎免费（§13.7）。
        for message in [
            "The request could not be processed. Please check the request parameters.",
            "The request could not be processed",
            "Bad request",
        ] {
            assert!(
                is_opaque_rejection(StatusCode::BAD_REQUEST, message),
                "应当判为可重试：{message}"
            );
        }
    }

    #[test]
    fn an_attributable_400_is_never_retried() {
        // 能归因到请求本身的 400，换到哪个目标都是同一个结果：重试只会白传一遍
        // 请求体，还会把明确的参数错误变成"时好时坏"。
        for message in [
            "prompt is too long: 1520629 tokens > 1000000 maximum",
            "field ReasoningEffort invalid, should be one of: low, medium, high, xhigh, none",
            "max_tokens: must be greater than thinking.budget_tokens",
            "temperature: must be between 0 and 1",
            "invalid x-api-key",
            "messages: at least one message is required",
            "unsupported content type",
            "thinking.type: unknown variant",
            "request exceeds the maximum context length",
        ] {
            assert!(
                !is_opaque_rejection(StatusCode::BAD_REQUEST, message),
                "不该重试：{message}"
            );
        }
    }

    #[test]
    fn upstream_content_moderation_is_recognised_as_such() {
        // 现场那句（2026-10-23，AwsClaude/ccmax）：它既不是"参数写错了"，也不是
        // "上游抽了"，而是这个账号自己的审核策略——必须与那两类分开处置（§13.3）。
        for message in [
            "Request was blocked by upstream content filtering (request id: 202610…)",
            "Request was blocked by upstream content filtering",
            "This request was rejected by our content policy.",
            "Blocked by moderation",
            "Your request was flagged by our safety system",
            // 国内中转站常用中文回同一件事。
            "请求内容未通过内容审核",
            "输入包含敏感词",
        ] {
            assert!(
                is_content_filtered(StatusCode::BAD_REQUEST, message),
                "应当判为内容审核：{message}"
            );
        }
    }

    /// 上游把会话钉在别的渠道上时，必须认出来并换渠道重试（§13.3）。
    ///
    /// 现场（2026-10-25，ccmax / opus-5-5 / wishtoapp）：这句话把 400 透传给了
    /// 用户，而同一段会话在别的账号上是能过的。
    #[test]
    fn a_conversation_pinned_to_another_channel_is_recognized() {
        for message in [
            "This conversation can no longer be routed to its original account. Start a new Claude Code session.",
            "conversation can no longer be routed to its original account",
            "Please Start a new Claude Code session.",
        ] {
            assert!(
                is_session_unroutable(message),
                "应当判为会话不可路由：{message}"
            );
        }
    }

    /// 反过来：普通的参数错误不能被误判成"换渠道就能过"。
    #[test]
    fn a_parameter_error_is_never_mistaken_for_an_unroutable_session() {
        for message in [
            "max_tokens must be greater than 0",
            "prompt is too long",
            "ReasoningEffort invalid",
            "The request could not be processed.",
        ] {
            assert!(
                !is_session_unroutable(message),
                "不该判为会话不可路由：{message}"
            );
        }
    }

    #[test]
    fn a_policy_rejection_is_never_mistaken_for_a_parameter_error() {
        // 反过来也一样：指向具体参数的 4xx 不是审核拒绝，换渠道不会让它变好。
        for message in [
            "content_type is invalid",
            "unsupported content_type: image/x-icon",
            "max_tokens must be greater than 0",
        ] {
            assert!(
                !is_content_filtered(StatusCode::BAD_REQUEST, message),
                "不该判为内容审核：{message}"
            );
        }
    }

    #[test]
    fn a_content_rejection_stays_out_of_the_opaque_bucket() {
        // 两条路径的处置不同（一个清粘性、一个不清），判据重叠会让同一句 400 走错门。
        let message = "Request was blocked by upstream content filtering";
        assert!(!is_opaque_rejection(StatusCode::BAD_REQUEST, message));
    }
    #[test]
    fn only_a_400_can_be_an_opaque_rejection() {
        // 别的状态码各有自己的处置：429 有 Retry-After，5xx 是可切换失败，
        // 401/403 走凭据判定。只有 400 需要这条"分不清就换一个试试"的规则。
        for status in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::NOT_FOUND,
        ] {
            assert!(
                !is_opaque_rejection(
                    status,
                    "The request could not be processed. Please check the request parameters."
                ),
                "{status} 不该走这条路"
            );
        }
    }

    #[test]
    fn the_effort_rejection_message_is_not_treated_as_opaque() {
        // 这两句都带 "thinking"，属于可归因（我们知道自己该改什么），必须先被
        // 档位分支接住，不能落进"换渠道重试"——换个渠道并不会让档位被接受。
        let message = "claude-opus-5-5 requires adaptive thinking; omit thinking or use \
                       thinking.type=adaptive and output_config.effort";
        assert!(!is_opaque_rejection(StatusCode::BAD_REQUEST, message));
    }

    #[test]
    fn only_a_correctly_shaped_adaptive_request_counts_as_an_effort_rejection() {
        use crate::protocol::canonical::Effort;
        let shaped = AdaptiveShape {
            adaptive: true,
            effort: Some(Effort::High),
        };
        let message = "claude-opus-5-5 requires adaptive thinking; omit thinking or use thinking.type=adaptive and output_config.effort";
        assert!(adaptive_effort_rejection(
            StatusCode::BAD_REQUEST,
            message,
            shaped
        ));

        // 形状不对时同一句话说的是形状问题，不是档位问题：换档重发没有意义。
        assert!(!adaptive_effort_rejection(
            StatusCode::BAD_REQUEST,
            message,
            AdaptiveShape {
                adaptive: false,
                effort: None
            }
        ));
        // 没发档位就无从降起。
        assert!(!adaptive_effort_rejection(
            StatusCode::BAD_REQUEST,
            message,
            AdaptiveShape {
                adaptive: true,
                effort: None
            }
        ));
        // 别的 400 不是这件事。
        assert!(!adaptive_effort_rejection(
            StatusCode::BAD_REQUEST,
            "max_tokens: must be greater than thinking.budget_tokens",
            shaped
        ));
        assert!(!adaptive_effort_rejection(
            StatusCode::TOO_MANY_REQUESTS,
            message,
            shaped
        ));
    }

    #[test]
    fn the_emitted_shape_is_read_back_from_the_body() {
        use crate::protocol::canonical::Effort;
        let adaptive = json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "xhigh"}
        });
        assert_eq!(adaptive_shape(&adaptive).effort, Some(Effort::XHigh));
        assert!(adaptive_shape(&adaptive).adaptive);

        let manual = json!({"thinking": {"type": "enabled", "budget_tokens": 4096}});
        assert!(!adaptive_shape(&manual).adaptive);
        assert_eq!(adaptive_shape(&manual).effort, None);
        assert_eq!(adaptive_shape(&json!({})).effort, None);
    }

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

    /// 401 与 403 不是一个结论：403 只有正文点名凭据才算失效（§12.3）。
    ///
    /// 上游用 403 表达"分组被停用/权限不足"时，把它当 Key 失效会把一把好 Key
    /// 永久钉死——失效标记只可能被一次成功清除，而那把 Key 已经被排除出抽签。
    /// 凭据级失败的健康判定：401/403 有 Retry-After 时算"等一会儿"，否则只有
    /// 正文点名凭据才硬停；402/429 的原判定一字不动（§12.3）。
    ///
    /// 这条判据此前写成"任何非 2xx 且正文没提凭据就降级成故障"，会把上游明确
    /// 给出的额度耗尽与限流一起吞掉——开发中实测踩到过，所以整条分支都钉在这。
    #[test]
    fn credential_failures_map_onto_the_health_state_machine() {
        let names_it = br#"{"error":{"message":"Invalid API key provided"}}"#;
        let group_gone = r#"{"error":{"message":"当前分组已被停用"}}"#.as_bytes();

        // 401：状态码本身就说明问题，不看正文。
        assert!(matches!(
            credential_outcome(StatusCode::UNAUTHORIZED, None, names_it),
            health::Outcome::KeyInvalid
        ));
        // 403 + 正文点名：失效。
        assert!(matches!(
            credential_outcome(StatusCode::FORBIDDEN, None, names_it),
            health::Outcome::KeyInvalid
        ));
        // 403 + 分组被停用：只是目标级权限问题。
        assert!(matches!(
            credential_outcome(StatusCode::FORBIDDEN, None, group_gone),
            health::Outcome::Fault
        ));

        // 带 Retry-After：上游说的是"过一会儿再来"，绝不做硬停（§12.3 既有口径）。
        assert!(matches!(
            credential_outcome(
                StatusCode::FORBIDDEN,
                Some(Duration::from_secs(30)),
                names_it
            ),
            health::Outcome::QuotaExhausted { .. }
        ));

        // 402 的额度耗尽与 429 的限流保持原判：它们不是"凭据不对"的信号，
        // 网关也**不会**把它们送进上面那条判据（见 run_attempt 的状态码守卫），
        // 所以这里从真正的入口 `classify_outcome` 断言，而不是从判据本身。
        assert!(matches!(
            classify_outcome(
                ErrorCode::UpstreamExhausted,
                Some(StatusCode::PAYMENT_REQUIRED),
                Some(Duration::from_secs(60))
            ),
            health::Outcome::QuotaExhausted { .. }
        ));
        assert!(matches!(
            classify_outcome(
                ErrorCode::RateLimited,
                Some(StatusCode::TOO_MANY_REQUESTS),
                None
            ),
            health::Outcome::RateLimited { .. }
        ));
    }

    #[test]
    fn only_certain_403s_prove_the_credential_is_bad() {
        assert!(!auth_proves_key_invalid(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"当前分组已被停用"}}"#.as_bytes()
        ));
        // 空体什么都没证明，按临时故障处理。
        assert!(!auth_proves_key_invalid(StatusCode::FORBIDDEN, b""));
        // 正文点名凭据：这才是失效证据，中英文都认。
        assert!(auth_proves_key_invalid(
            StatusCode::FORBIDDEN,
            br#"{"error":{"message":"Invalid API key provided"}}"#
        ));
        assert!(auth_proves_key_invalid(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"Key 无效"}}"#.as_bytes()
        ));
        // 401 不看正文：状态码本身已经说清楚了。
        assert!(auth_proves_key_invalid(
            StatusCode::UNAUTHORIZED,
            br#"{"error":{"message":"Unauthorized"}}"#
        ));
        // 其他状态码不参与这条判据。
        assert!(!auth_proves_key_invalid(
            StatusCode::INTERNAL_SERVER_ERROR,
            b""
        ));
    }

    #[test]
    fn upstream_statuses_map_onto_the_health_state_machine() {
        // 401 是"Key 坏了"；403 还要看正文，这里先钉住 401 这一支（§12.1）。
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
        let estimate = estimate_tokens(Endpoint::ChatCompletions, 30_000, &body, None);
        assert_eq!(estimate, 10_000 + 4096);
        // 没声明最大输出时只算输入。
        assert_eq!(
            estimate_tokens(Endpoint::ChatCompletions, 3_000, &json!({}), None),
            1_000
        );
    }

    /// 图片请求的 token 口径：只数文本字段，绝不把图片字节按 3 字节一个 token
    /// 折算——那会把一次图生图记成上百万 token，带 TPM 的分组直接全灭。
    #[test]
    fn image_token_estimates_ignore_image_bytes() {
        let boundary = "b";
        let mut raw = Vec::new();
        raw.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\n一只猫\r\n"
            )
            .as_bytes(),
        );
        raw.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        raw.extend_from_slice(&vec![7_u8; 4 * 1024 * 1024]);
        raw.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        assert!(
            multipart_text_bytes(&raw) < 1024,
            "图片字节被算成了文本字段"
        );
        let raw_body = RawBody {
            bytes: raw.clone(),
            content_type: HeaderValue::from_static("multipart/form-data; boundary=b"),
        };
        let image = estimate_tokens(
            Endpoint::ImagesEdits,
            raw.len(),
            &json!({"model": "edit-model"}),
            Some(&raw_body),
        );
        assert!(image < 100, "一次图生图被估成了 {image} 个 token");

        // 同一个正文走文本端点仍是老口径：图片端点那次豁免不能顺手放宽别处。
        let text = estimate_tokens(Endpoint::ChatCompletions, raw.len(), &json!({}), None);
        assert!(text > 1_000_000, "文本端点必须继续按体积保守估算");
    }

    /// JSON 入口的内联图片（data URL / 超长 base64）同样不算 token。
    #[test]
    fn image_token_estimates_skip_inline_data_urls() {
        let body = json!({
            "model": "gpt-image-1",
            "prompt": "一只猫",
            "image": format!("data:image/png;base64,{}", "A".repeat(200_000)),
        });
        let estimate = estimate_tokens(Endpoint::ImagesGenerations, 200_000, &body, None);
        assert!(estimate < 64, "内联图片被算成了 token：{estimate}");
    }

    /// multipart 里 `stream` 字段决定走不走流式；字段顺序无关，非真值不算。
    #[test]
    fn multipart_stream_field_is_read_from_form_data() {
        let with_stream = |value: &str, stream_first: bool| {
            let mut body = Vec::new();
            let model = "--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n";
            let stream = format!(
                "--b\r\nContent-Disposition: form-data; name=\"stream\"\r\n\r\n{value}\r\n"
            );
            if stream_first {
                body.extend_from_slice(stream.as_bytes());
                body.extend_from_slice(model.as_bytes());
            } else {
                body.extend_from_slice(model.as_bytes());
                body.extend_from_slice(stream.as_bytes());
            }
            body.extend_from_slice(b"--b--\r\n");
            body
        };

        assert!(multipart_requests_stream(&with_stream("true", false)));
        assert!(multipart_requests_stream(&with_stream("TRUE", true)));
        assert!(multipart_requests_stream(&with_stream("1", true)));
        assert!(!multipart_requests_stream(&with_stream("false", false)));
        assert!(!multipart_requests_stream(b"not multipart"));
        assert_eq!(
            extract_multipart_field(&with_stream("true", true), "stream").unwrap(),
            "true"
        );
        assert_eq!(
            extract_multipart_field(&with_stream("true", false), "model").unwrap(),
            "m"
        );
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
            Err(MultipartFieldError::Missing)
        );
        let body = b"--b\nContent-Disposition: form-data; name=\"model\"\n\n m \n--b--\n";
        assert_eq!(extract_multipart_model(body).unwrap(), "m");
    }

    #[test]
    fn multipart_rewrite_rejects_control_characters_in_upstream_model() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n--b--\r\n";
        assert_eq!(
            rewrite_multipart_model(body, "bad\r\nvalue"),
            Err(MultipartFieldError::UnsafeReplacement)
        );
    }
}
