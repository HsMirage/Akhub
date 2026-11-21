//! 流式响应的统一结算（§13.4、§19.4、§26.3）。
//!
//! 流式请求在 HTTP 头发出之后还没有结束：吞吐、可靠性、TPM 回补与 Responses
//! 状态链都必须等到**流真正结束**才算得准。这里把结算绑定到响应体的生命周期，
//! 正常结束、流内错误与客户端断开三种结局都恰好结算一次。
//!
//! 结算内容与依据：
//!
//! - 健康与熔断：流完好走 `Success`，流内错误走 `Fault`，客户端断开走
//!   `Neutral`（不是上游的错）。
//! - TPM：只在字节流里真的带回了完整 usage 时按实际值回补；拿不到就保持
//!   保守预留，绝不估算（§17.2）。
//! - 性能：首字延迟取提交时刻，总耗时取流结束时刻——首段提交时记的"总耗时"
//!   只是首字时间，用它喂 EWMA 会系统性高估吞吐。客户端断开不进性能样本。
//! - 请求记录：流结束才落库，错误流的 `error_code` 不再伪装成成功；客户端
//!   断开记 `client_gone`，与"上游没上报用量"区分开，记录页才说得清这一次
//!   为什么没有输入/输出。
//! - Responses：完成后用 `response.completed` 的最终对象补写输出项；流失败
//!   或客户端中断时删除状态链，让后续引用得到明确的 `response_state_expired`，
//!   而不是拿到缺失历史后静默继续（§15.2）。

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::Response;
use futures::StreamExt as _;
use serde_json::Value;

use crate::app::SharedState;
use crate::domain::Protocol;
use crate::gateway::responses::{self, ChainPlan, PendingState};
use crate::gateway::stream::StreamAccounting;
use crate::health;
use crate::routing::score;
use crate::storage::store::RequestRecord;

/// 流式请求在结束时才结算的一切。
pub struct StreamSettlement {
    pub state: SharedState,
    /// 下游协议：决定怎么从字节流里读 usage 与最终响应对象。
    pub protocol: Protocol,
    /// 推理流的无数据超时；图片流沿用它自己的总时长限制。
    pub idle_timeout: Option<Duration>,
    pub target_id: String,
    pub dimension: score::Dimension,
    /// 上游尝试开始的时间，用于真实总耗时。
    pub started: Instant,
    /// 请求开始时间，用于请求记录里的端到端耗时。
    pub request_started: Instant,
    /// 首个语义块时间（首字延迟）。
    pub first_token: Option<Duration>,
    /// 心跳路径下首个语义事件的时钟（§13.4 修订）。
    ///
    /// 走心跳的尝试里 `first_byte` 量到的是第一个注释帧，不能当首字延迟。
    /// 响应体看到真正的语义内容时把时刻写进来，结算优先用它。
    pub first_content: Option<crate::gateway::stream::FirstContentClock>,
    /// 客户端体感的首字节时间：排队 + 上游响应头 + 首个语义块（§6.6、§9.3）。
    ///
    /// 一个字节都没送出去（客户端在首个事件前就断开）时为空——那种情况下
    /// "首字延迟 0 毫秒"是一句谎话，而记录页正是靠这一项解释流为什么没有用量。
    pub first_byte: Option<Duration>,
    /// 提交那一刻生成的记录，流结束后才真正落库。
    pub record: RequestRecord,
    /// 健康与限额准入；流结束时释放。
    pub admission: Option<health::Admission>,
    /// Responses 入口才有的状态链补写计划。
    pub responses: Option<ResponsesCompletion>,
    /// 流式过程中解析阶段丢掉的能力（§14.8）。
    ///
    /// 响应头在流开始前就发出去了，中途才知道的降级只能落到请求记录里——
    /// 记录页照样能标红，这是"不静默丢失"的实际落点。
    pub degraded: crate::gateway::translate::DegradationSink,
    /// 客户端断开后继续读完上游的时间上限；0 表示立刻放弃（§26.3 修订）。
    pub drain_budget: Duration,
    /// 异常结束（EOF / 客户端断开）时补发终止帧所需的请求上下文。
    ///
    /// 上游 newapi 流没正常收尾也会补一帧 usage 再结束，下游据此记账；akhub
    /// 此前只发错误事件就断流，sub2api 拿不到用量整条丢弃。这里带上请求体与
    /// 端点，结算时按上游口径（缓存并入输入）补出终止帧，让下游正常计费。
    pub billing: Option<BillingContext>,
}

/// 异常结束时补发计费终止帧所需的请求上下文。
pub struct BillingContext {
    pub endpoint: crate::upstream::Endpoint,
    pub request_bytes: usize,
    pub body: std::sync::Arc<Value>,
    pub raw: Option<crate::gateway::passthrough::RawBody>,
    /// Responses 入口的网关 ID：补发的 `response.completed` 必须用它，
    /// 客户端引用的地址从第一个事件起就固定是网关 ID（§15.1）。
    pub responses_id: Option<String>,
}

/// Responses 流完成后的状态链补写计划（§15.2）。
pub struct ResponsesCompletion {
    pub state: SharedState,
    pub chain: ChainPlan,
    pub pending: PendingState,
    pub entry_body: std::sync::Arc<Value>,
    pub entry_protocol: Protocol,
    pub retention_days: u32,
}

/// 流的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// 上游正常结束，且没有出现错误事件。
    Completed,
    /// 传输中断或流内错误事件。
    Failed(&'static str),
    /// 客户端断开，流被丢弃。
    ///
    /// 这不是上游的故障，重试也救不回来：响应头早就发出去了，正文可能已经
    /// 推了一半，而放弃连接的就是客户端自己。把它与"上游没上报用量"分成两种
    /// 结局，记录页才回答得了"这次为什么没有输入/输出"。
    Aborted,
}

impl Ending {
    /// 落进请求记录 `error_code` 的稳定标识。
    ///
    /// `client_gone` 与 New API / sub2api 的同名字段同义：**下游客户端断开**。
    /// 有了它，记录页不会再把它显示成绿色的 200 成功（§18.1）。
    fn error_code(self) -> Option<&'static str> {
        match self {
            Self::Completed => None,
            Self::Failed(code) => Some(code),
            Self::Aborted => Some(crate::gateway::error::CLIENT_GONE),
        }
    }
}

/// 这次请求"用户真正等到内容"用了多久（§6.6、§9.3）。
///
/// 两条口径：
///
/// - **普通路径**：用 `first_byte`（排队 + 上游响应头 + 首个语义块），
///   它能如实反映"排了 20 秒队"这类等待，而 `first_token` 只有 1 毫秒。
/// - **心跳路径**：`first_byte` 量到的是第一个注释帧——那是网关自己发的，
///   不是上游的内容。此时优先用响应体记下的首个**语义**事件时刻；
///   那个时刻还没出现（客户端在真内容之前就断了）时留空，
///   不把心跳的时间冒充首字。
fn observed_first_content(settlement: &StreamSettlement) -> Option<Duration> {
    if let Some(clock) = settlement.first_content.as_ref()
        && let Some(observed) = crate::gateway::stream::first_content(clock)
    {
        return Some(observed);
    }
    if settlement.first_content.is_some() {
        // 心跳路径但真内容一直没来：不知道就是不知道。
        return None;
    }
    settlement.first_byte
}

/// 把结算绑定到响应体的完整生命周期。
///
/// `strip_unrequested_usage` 为真时，把「客户端没有索取的 usage 收尾块」挡在
/// 下发之前。网关为了自己算输出速度与归还 TPM 会向所有上游索取 usage，但下游
/// 没写 `stream_options.include_usage` 时不该平白多收一帧 `choices: []`。
///
/// 顺序是**先喂 accounting、再过滤**：那一帧正是网关要的用量来源，先丢就
/// 等于白问上游一句。要变的是「网关知不知道用量」，不是「客户端看到什么」。
pub fn settle_stream(
    response: Response,
    settlement: StreamSettlement,
    strip_unrequested_usage: bool,
) -> Response {
    let (parts, body) = response.into_parts();
    let protocol = settlement.protocol;
    let idle_timeout = settlement.idle_timeout;
    let request_id = settlement.record.request_id.clone();
    let drain_budget = settlement.drain_budget;
    let is_image_endpoint = settlement
        .billing
        .as_ref()
        .is_some_and(|ctx| ctx.endpoint.is_image());
    let billing = settlement.billing.as_ref().map(|ctx| {
        let input_estimate = crate::gateway::passthrough::estimate_tokens(
            ctx.endpoint,
            ctx.request_bytes,
            &ctx.body,
            ctx.raw.as_ref(),
        );
        (ctx.responses_id.clone(), input_estimate)
    });
    let guard = SettlementGuard {
        settlement: Some(settlement),
        accounting: StreamAccounting::new(protocol),
        billing,
        unfinished_at_disconnect: false,
    };

    // **读取、累计与结算都在独立任务里，响应体只转发字节。**
    //
    // 客户端断开时 axum 会直接丢弃响应体；若上游读取写在 body 里，它会跟着被丢掉，
    // 而上游这时往往还在正常生成，那部分真实用量我们永远读不到（现场：New API 记
    // 49,517/178，我们只记「未知 / 未知」）。放进独立任务后，断开只是不再往 channel
    // 写，任务继续把上游读完再按 client_gone 结算落库（§26.3 修订）。
    let (sender, receiver) = tokio::sync::mpsc::channel::<UpstreamChunk>(16);
    // 下游是否已经走了。响应体被丢弃时由 Drop 置位，pump 用它区分「客户端断开」
    // 与「上游正常写完」——两者都可能读到完整 usage，但记录页要给不同的结论。
    let client_gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // drain 阶段单次等待上限：客户端走了之后再等这么一小段时间还没新数据，就
    // 认为上游也停下来了，按已读到的用量结算。用完整空闲超时（默认 180 秒）会把
    // 记录拖到几分钟后才落库——记录页看不到这次调用，比少记一点用量更糟。
    let drain_grace = drain_budget.min(CLIENT_GONE_DRAIN_GRACE);
    tokio::spawn(pump_upstream(UpstreamPump {
        guard,
        upstream: Box::pin(body.into_data_stream()),
        sender,
        idle_timeout,
        drain_budget,
        drain_grace,
        client_gone: std::sync::Arc::clone(&client_gone),
        request_id: request_id.clone(),
        is_image: is_image_endpoint,
        protocol,
    }));

    let stream = downstream_stream(receiver, strip_unrequested_usage, client_gone);
    Response::from_parts(parts, Body::from_stream(stream))
}

/// 上游 → 下游之间传的一件东西。
enum UpstreamChunk {
    Data(axum::body::Bytes),
    /// 上游把整条流发完了（字节已经全部转发完）。
    End,
}

/// 读取上游、累计用量并负责结算的那一半。
struct UpstreamPump {
    guard: SettlementGuard,
    upstream: TokioUpstream,
    sender: tokio::sync::mpsc::Sender<UpstreamChunk>,
    idle_timeout: Option<Duration>,
    drain_budget: Duration,
    /// drain 阶段单次等待上限（上游安静下来就收手）。
    drain_grace: Duration,
    /// 下游已经走了（响应体被丢弃）。
    client_gone: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 请求 ID：断流时写进流内错误帧（§18.2）。
    request_id: String,
    /// 图片端点：它的 usage 就挂在 `image_generation.completed` 上，不补终止帧。
    is_image: bool,
    /// 下游协议：补发的终止帧/错误帧要长成它认的样子。
    protocol: Protocol,
}

/// 跑完整条上游流：边读边转发、边累计用量，读完后按结局结算（§26.3）。
async fn pump_upstream(pump: UpstreamPump) {
    let UpstreamPump {
        mut guard,
        mut upstream,
        sender,
        idle_timeout,
        drain_budget,
        drain_grace,
        client_gone,
        request_id,
        is_image,
        protocol,
    } = pump;
    let mut bytes_read = 0usize;
    let mut saw_error_frame = false;
    let mut upstream_broken = false;
    let mut usage_injected = false;
    let mut error_probe = crate::protocol::sse::FrameReader::new();
    // 下游走掉之后继续读的起点（None = 下游还在）。
    let mut gone_at: Option<tokio::time::Instant> = None;
    'pump: loop {
        if gone_at.is_none() && client_gone.load(std::sync::atomic::Ordering::Relaxed) {
            gone_at = Some(tokio::time::Instant::now());
        }
        if let Some(start) = gone_at
            && (drain_budget.is_zero() || start.elapsed() >= drain_budget)
        {
            // 配置关闭了 drain，或已经读够久：按已读到的用量结算。
            break;
        }
        // 同时等「上游下一块」和「下游断开」。两者都只等待、不消费数据，所以
        // 断开分支胜出时不会吃掉任何字节（drain 之后照样从上游读得到）。
        let next = tokio::select! {
            // biased：断开检测优先。上游一直返回"还没数据"时，普通 select 每次都
            // 随机挑分支，"断开"可能长时间轮不到——现场就是记录一直不落库。
            biased;
            _ = sender.closed(), if gone_at.is_none() => {
                gone_at = Some(tokio::time::Instant::now());
                // 断开这一刻上游写完了没有？写完了就是"下游读完才走"（正常完成），
                // 没写完才是真的 client_gone。
                guard.unfinished_at_disconnect = !guard.accounting.answer_completed();
                // 上游一个字节都没送出（纯心跳场景）：drain 也读不到任何用量，
                // 立刻结算，别白等一段 grace（现场：首字百秒级的慢回答）。
                //
                // 判据只看字节数，不看 usage 有没有到：上游常常先把正文吐完、
                // 最后才补 usage——那正是要 drain 的情形。
                let nothing_coming = bytes_read == 0;
                // 注意：这里是 select! 的分支体，`break` 只会跳出 select 内部块，
                // 必须用显式标志退出外层循环。
                if nothing_coming {
                    break 'pump;
                }
                // 下游走了：剩下的只读不发，继续累计用量直到上游收尾或超时。
                continue 'pump;
            }
            next = async {
                // 等待上限按阶段取小的那个：
                //  * 正常阶段——空闲超时（上游卡住就换）；
                //  * drain 阶段——空闲超时与剩余预算取小。上游安静下来就立刻收手，
                //    不能因为"预算还有 60 秒"就干等着，记录会被挂死的上游拖住。
                let limit = match gone_at {
                    // drain 阶段：总时长就按 grace 算，不因为上游还在吐字节而续命。
                    // 上游真要发完，几秒内一定发得完；一直发不完的多半是空转
                    // （hyper 会在等待时吐块），续命只会让记录永远落不了库。
                    Some(start) => drain_grace.saturating_sub(start.elapsed()),
                    None => idle_timeout.unwrap_or(drain_budget),
                };
                tokio::time::timeout(limit, upstream.next()).await
            } => match next {
                Ok(next) => next,
                Err(_) => {
                    // 等待超时：正常阶段是上游卡住（upstream_timeout）；drain 阶段
                    // 是客户端早就走了、上游也安静了——那是 client_gone，不是超时。
                    guard.accounting.finish();
                    let ending = if gone_at.is_some() {
                        Ending::Aborted
                    } else {
                        Ending::Failed("upstream_timeout")
                    };
                    let frames = guard.settle(ending);
                    // 超时也要按上游口径补出带用量的终止帧，否则下游只看到流凭空
                    // 断掉、无从计费（§26.3）。
                    if gone_at.is_none() {
                        if frames.is_empty() {
                            let _ = sender
                                .send(UpstreamChunk::Data(crate::protocol::stream_error(
                                    protocol,
                                    crate::gateway::error::ErrorCode::UpstreamTimeout,
                                    "上游流式响应长时间未返回数据，请重试",
                                    Some(&request_id),
                                )))
                                .await;
                        } else {
                            for frame in frames {
                                let _ = sender.send(UpstreamChunk::Data(frame)).await;
                            }
                        }
                    }
                    let _ = sender.send(UpstreamChunk::End).await;
                    return;
                }
            },
        };
        match next {
            Some(Ok(chunk)) => {
                // 空块（hyper 在等待上游时会吐）不算进展：否则 drain 会被无限
                // 续命，记录永远落不了库。
                if chunk.is_empty() {
                    continue 'pump;
                }
                bytes_read += chunk.len();
                guard.accounting.push(&chunk);
                // 下游走掉就转入 drain：剩下的字节照样读、照样累计，只是不再下发。
                if gone_at.is_none() && client_gone.load(std::sync::atomic::Ordering::Relaxed) {
                    gone_at = Some(tokio::time::Instant::now());
                }
                if gone_at.is_some() {
                    continue;
                }
                // 错误帧之前先补一帧「只有 usage、没有终止信号」的用量帧：下游读到
                // 错误就停止解析后续帧，顺序反了用量永远进不了它的计费统计。
                //
                // 按帧边界切块转发：error_probe 与游标同步消费同一份字节，跨块半帧
                // 由 error_probe 内部缓冲拼回，游标只在完整帧处推进。
                let frames = error_probe.push(&chunk);
                let mut cursor = 0usize;
                for frame in &frames {
                    let Some(end) = crate::protocol::sse::find_frame_end(&chunk, cursor) else {
                        break;
                    };
                    if crate::protocol::frame_error(frame).is_some() {
                        saw_error_frame = true;
                        if !usage_injected && let Some(usage_frame) = guard.usage_only_frame() {
                            push(&sender, UpstreamChunk::Data(usage_frame), &mut gone_at).await;
                            usage_injected = true;
                        }
                    }
                    push(
                        &sender,
                        UpstreamChunk::Data(axum::body::Bytes::copy_from_slice(
                            &chunk[cursor..end],
                        )),
                        &mut gone_at,
                    )
                    .await;
                    cursor = end;
                }
                if cursor < chunk.len() {
                    push(
                        &sender,
                        UpstreamChunk::Data(axum::body::Bytes::copy_from_slice(&chunk[cursor..])),
                        &mut gone_at,
                    )
                    .await;
                }
            }
            // 上游断流与正常读完都走统一收尾：结算后先补计费/终止帧，最后才发
            // End。在这里直接发 End 会让下游提前收尾，把后面那些帧全吞掉。
            Some(Err(_)) => {
                upstream_broken = true;
                break;
            }
            None => break,
        }
    }
    if let Some(frame) = error_probe.finish()
        && crate::protocol::frame_error(&frame).is_some()
    {
        saw_error_frame = true;
    }
    // 上游刚读完那一刻下游可能才断开：收尾前再看一眼，别把 client_gone 记成成功。
    if gone_at.is_none() && client_gone.load(std::sync::atomic::Ordering::Relaxed) {
        gone_at = Some(tokio::time::Instant::now());
    }
    guard.accounting.finish();

    // 结局：错误帧 > 客户端断开 > 上游错误 > 完整写完 > 其余断流。
    //
    // 客户端断开必须排在"写完"之前：这次回答在上游那边确实写完了，但驱动它的
    // 那个客户端已经走了，记录页要如实说这是一次 client_gone（§18.1），
    // 而不是绿色成功——否则「断开后 drain 回来的用量」会被误读成正常完成。
    let ending = if saw_error_frame {
        Ending::Failed("upstream_protocol_error")
    } else if gone_at.is_some() {
        Ending::Aborted
    } else if guard.accounting.error().is_some() {
        Ending::Failed("upstream_protocol_error")
    } else if guard.accounting.answer_completed() {
        Ending::Completed
    } else {
        Ending::Failed("upstream_exhausted")
    };
    let frames = guard.settle(ending);
    // 客户端还在：先告诉它这次断流了（否则它只看到流凭空结束），再补计费终止帧。
    // 顺序照现场要求：用量帧在错误帧之前，收尾帧在错误之后。
    //
    // **图片端点不补**：图片流的 usage 就挂在 `image_generation.completed` 事件上，
    // 那一帧是图片本体，再补一帧 Chat 形状的用量/收尾只会污染下游的事件序列。
    if is_image {
        let _ = sender.send(UpstreamChunk::End).await;
        return;
    }
    if upstream_broken {
        let _ = sender
            .send(UpstreamChunk::Data(crate::protocol::stream_error(
                Protocol::OpenAiChat,
                crate::gateway::error::ErrorCode::UpstreamExhausted,
                "上游流式响应中断",
                Some(&request_id),
            )))
            .await;
    }
    for frame in frames {
        let _ = sender.send(UpstreamChunk::Data(frame)).await;
    }
    let _ = sender.send(UpstreamChunk::End).await;
}

/// 把一帧交给下游；发送失败说明下游已经走了，记下这个时刻。
///
/// 用**发送失败**而不是 channel 的 `is_closed()`：后者要等发送端真去写才发现，
/// 而上游还没吐新字节时我们根本不会写——断开就会被漏掉，结果 client_gone 被误记
/// 成成功。
async fn push(
    sender: &tokio::sync::mpsc::Sender<UpstreamChunk>,
    item: UpstreamChunk,
    gone_at: &mut Option<tokio::time::Instant>,
) {
    if sender.send(item).await.is_err() && gone_at.is_none() {
        *gone_at = Some(tokio::time::Instant::now());
    }
}

/// 下游那一半：把 pump 送来的字节转出去，必要时剥掉没索取的 usage 帧。
fn downstream_stream(
    mut receiver: tokio::sync::mpsc::Receiver<UpstreamChunk>,
    strip_unrequested_usage: bool,
    client_gone: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> impl futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> {
    async_stream::stream! {
        // 这个生成器被丢弃 = 客户端断开。置位后 pump 会继续读完上游（drain），
        // 再把这次请求记成 client_gone。
        let _gone = ClientGoneOnDrop(client_gone);
        let mut filter = crate::gateway::translate::UnrequestedUsageFilter::new(strip_unrequested_usage);
        while let Some(item) = receiver.recv().await {
            match item {
                UpstreamChunk::Data(chunk) => {
                    if strip_unrequested_usage {
                        let bytes = filter.push(&chunk);
                        if !bytes.is_empty() {
                            yield Ok::<_, axum::Error>(bytes);
                        }
                    } else {
                        yield Ok::<_, axum::Error>(chunk);
                    }
                }
                UpstreamChunk::End => break,

            }
        }
        if let Some(tail) = filter.finish() {
            yield Ok::<_, axum::Error>(tail);
        }
    }
}

/// 响应体被丢弃时把「客户端已断开」置位。
struct ClientGoneOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for ClientGoneOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 客户端断开后，drain 阶段"上游安静多久就收手"。
///
/// 断开那一刻上游通常还在吐字节，读到收尾只要几秒；但如果上游也停了，再等下去
/// 只会把这次请求的记录无限期推迟（记录页看不到，比少记一点用量更糟）。
/// 1 秒足够覆盖上游正常吐字之间的间隔——真在生成时数据是连续的。
const CLIENT_GONE_DRAIN_GRACE: Duration = Duration::from_secs(1);

/// 上游响应体的数据流类型。
type TokioUpstream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>>;

/// 结算守卫：正常结束显式结算，客户端断开由 `Drop` 兜底。
struct SettlementGuard {
    settlement: Option<StreamSettlement>,
    accounting: StreamAccounting,
    /// 异常结束时补发终止帧用的（Responses 网关 ID、输入估算）。
    billing: Option<(Option<String>, u64)>,
    /// 下游断开时，上游的终止信号**还没**到（回答没写完）。
    ///
    /// 用来区分两种长得一样的流：上游写完、下游才走（正常完成，需求证）；
    /// 下游先走、我们替它把上游读完（client_gone，不能记成绿色成功）。
    unfinished_at_disconnect: bool,
}

impl SettlementGuard {
    /// 异常结束时交给下游计费的用量（上游 newapi 口径：缓存并入输入、不带缓存细分）。
    ///
    /// 上游没报输入时退回请求体估算（与 TPM 预留同一口径）；拿不到任何输入时
    /// 返回 None——没有可计费的量，补 0 反而是伪造。
    fn billed_usage(&self) -> Option<crate::protocol::canonical::Usage> {
        let (_, input_estimate) = self.billing.as_ref()?;
        let breakdown = crate::gateway::stream::billing_usage(
            self.accounting.usage_breakdown(),
            *input_estimate,
        );
        Some(crate::protocol::canonical::Usage {
            input: breakdown.input,
            output: breakdown.output,
            cache_read: None,
            cache_write: None,
            reasoning: breakdown.reasoning,
        })
        .filter(|usage| !usage.is_empty())
    }

    /// 错误帧之前补的一帧「只有 usage、没有终止信号」的用量帧。
    ///
    /// sub2api / newapi 读到错误事件就停止解析后续帧，但对任何带 usage 的帧都会
    /// 先累计用量。所以这帧不能带 finish_reason / message_stop / response.completed
    /// 之类的终止信号（否则下游提前收尾、看不到后面的错误），只带用量本身。
    fn usage_only_frame(&self) -> Option<axum::body::Bytes> {
        let settlement = self.settlement.as_ref()?;
        let (responses_id, _) = self.billing.as_ref()?;
        let usage = self.billed_usage()?;
        crate::protocol::usage_only_frame(settlement.protocol, usage, responses_id.clone())
    }

    /// 结算并返回异常结束时要补发给下游的计费终止帧（正常结束返回空）。
    ///
    /// 上游 newapi 流没正常收尾也会补一帧 usage 再结束，下游据此记账；akhub 此前
    /// 只发错误事件就断流，sub2api 把它当成"可重放失败"整条丢弃或按 95% 缓存读估算。
    /// 这里按上游口径（缓存并入输入、不带缓存细分）补出协议自己的终止帧，让下游
    /// 无论 sub2api / newapi 都按真实用量正常计费。拿不到输入用量时不补帧——没有
    /// 可计费的量，补一帧 0 用量反而是伪造。
    fn settle(&mut self, ending: Ending) -> Vec<axum::body::Bytes> {
        let Some(settlement) = self.settlement.take() else {
            return Vec::new();
        };
        self.accounting.finish();
        // 上游把错误写进流里时，生成器是"正常结束"的；语义上必须算失败。
        let ending = match ending {
            Ending::Completed if self.accounting.error().is_some() => {
                Ending::Failed("upstream_protocol_error")
            }
            // 客户端断开，但上游确认整轮回答已经写完：这次不是"掉线"。
            //
            // 判据是**断开那一刻**就已经收到了终止信号（`answer_completed`）。
            // 断开之后才由 drain 补收到的收尾不算——那种情况下驱动这次请求的客户端
            // 早就走了，记录页必须如实说 client_gone，否则「drain 回来的用量」会被
            // 误读成一次正常完成。
            Ending::Aborted
                if self.accounting.error().is_none()
                    && self.accounting.answer_completed()
                    && !self.unfinished_at_disconnect =>
            {
                Ending::Completed
            }
            other => other,
        };
        // 协议与网关 ID 先取出来：settlement 马上要被 move 进 settle_one。
        let protocol = settlement.protocol;
        let responses_id = self.billing.as_ref().and_then(|(id, _)| id.clone());
        let usage = self.billed_usage();
        let input_estimate = self.billing.as_ref().map(|(_, estimate)| *estimate);
        settle_one(settlement, ending, &self.accounting, input_estimate);
        // 异常结束才补终止帧：正常收尾的流上游已经发过 usage，再补一帧会重复计费。
        match (ending, usage) {
            (Ending::Completed, _) | (_, None) => Vec::new(),
            (Ending::Failed(_) | Ending::Aborted, Some(usage)) => {
                crate::protocol::terminal_usage_frames(protocol, usage, responses_id)
            }
        }
    }
}

impl Drop for SettlementGuard {
    fn drop(&mut self) {
        // 客户端断开或任务被取消：不是上游的错，但状态链必须处理。
        // 连接已经没了，补发的终止帧无处可送，丢弃即可（结算照常落库）。
        let _ = self.settle(Ending::Aborted);
    }
}

fn settle_one(
    settlement: StreamSettlement,
    ending: Ending,
    accounting: &StreamAccounting,
    input_estimate: Option<u64>,
) {
    // 先把"用户等到内容"的时刻取出来：后面 settlement 会被部分移动，借不到字段。
    let observed = observed_first_content(&settlement);
    let usage = accounting.usage_tokens();
    let outcome = match ending {
        Ending::Completed => health::Outcome::Success,
        Ending::Failed("upstream_timeout") => health::Outcome::Neutral,
        Ending::Failed(_) => health::Outcome::Fault,
        Ending::Aborted => health::Outcome::Neutral,
    };
    if let Some(admission) = settlement.admission {
        admission.settle(outcome, usage);
    }

    // 写完才断开的已在 `SettlementGuard::settle` 归为 Completed（§9.3 修订），
    // 走到这里的 Aborted 都是真的中途断流：没有可用证据，也不动可靠性。
    let kind = match ending {
        Ending::Aborted => score::SampleKind::Excluded,
        Ending::Completed | Ending::Failed(_) => score::SampleKind::Rated,
    };
    settlement.state.runtime.perf.observe(
        &settlement.target_id,
        settlement.dimension,
        &score::Sample {
            kind,
            success: kind != score::SampleKind::Rated || ending == Ending::Completed,
            // 样本用首字节而不是首字：评分要反映用户实际等了多久（§9.3）。
            // 走心跳的尝试例外——那里的首字节是第一个注释帧，用真实首字。
            first_token: observed,
            // 现在才是真正的"流结束时间"，不是首段提交时间。
            total: settlement.started.elapsed(),
            output_tokens: accounting.output_tokens(),
            // 这次请求的缓存命中率：喂给评分，让"缓存亲和"成为可被选择的证据。
            // 字段缺失时是 `None`（上游没上报），绝不拿 0 冒充"没命中"。
            cache_hit: crate::gateway::stream::cache_hit_rate(accounting.usage_breakdown()),
        },
        crate::storage::now_unix(),
    );

    let mut record = settlement.record;
    record.duration_ms = settlement.request_started.elapsed().as_millis() as i64;
    // 流式的用量与首字延迟只有在这里才拿得到（§6.6、§6.8）。
    // 记录里写**首字节**：一次排了 20 秒队、首个事件随即到达的请求，
    // 首字延迟是 1 毫秒，只有这一项能如实反映那次等待（§24.1）。
    //
    // 一个字节都没发出去时留空：不把"没等到"写成 0 毫秒（§6.6 的口径
    // 与用量一致——不知道就是不知道，绝不编造）。
    record.first_token_ms = observed.map(|value| value.as_millis() as i64);
    record.input_tokens = accounting.input_tokens().map(|value| value as i64);
    record.output_tokens = accounting.output_tokens().map(|value| value as i64);
    // Token 细分同样只有流结束才拿得到（§11.6）。
    let breakdown = accounting.usage_breakdown();
    record.cache_read_tokens = breakdown.cache_read.map(|value| value as i64);
    record.cache_write_tokens = breakdown.cache_write.map(|value| value as i64);
    record.reasoning_tokens = breakdown.reasoning.map(|value| value as i64);
    // 异常结束且上游没报用量时，记录页显示的是**下发给下游计费的那一份**估算：
    // 否则控制台永远是「未知 / 未知」，与下游实际扣费的用量对不上。正常结束或
    // 上游报了量时保持原样（绝不拿估算覆盖真实观测）。
    if matches!(ending, Ending::Failed(_))
        && record.input_tokens.is_none()
        && let Some(input_estimate) = input_estimate
    {
        let billed = crate::gateway::stream::billing_usage(breakdown, input_estimate);
        record.input_tokens = billed.input.map(|value| value as i64);
        record.output_tokens = billed.output.map(|value| value as i64);
    }
    // 结局写进 `error_code`：客户端断开记 `client_gone`，不再伪装成 200 成功
    // （§18.1、§24.1）。这是"这次为什么没有输入/输出"的第一手答案。
    if let Some(code) = ending.error_code() {
        record.error_code = Some(code.to_string());
    }
    // 把流中途记下的能力降级并进请求记录（§14.8）。去重后与发射阶段的
    // 降级合并，避免同一项出现两次。
    if let Ok(extra) = settlement.degraded.lock() {
        for capability in extra.iter() {
            let already = record
                .degraded
                .as_deref()
                .is_some_and(|existing| existing.split(',').any(|item| item == capability));
            if already {
                continue;
            }
            record.degraded = Some(match record.degraded.take() {
                Some(existing) => format!("{existing},{capability}"),
                None => capability.clone(),
            });
        }
    }
    settlement.state.recorder.record(record);

    let Some(completion) = settlement.responses else {
        return;
    };
    // 客户端断开要分两种：上游的最终对象已经完整送达（正文早就吐完，只是
    // 下游没等到收尾就关了连接），这次回答其实是完整的——删掉状态链会让
    // 用户下一次引用凭空得到 `response_state_expired`。只有真的没有最终对象
    // 时才删除：宁可由后续引用报过期，也不能保存残缺历史（§15.2）。
    let finished = match ending {
        Ending::Failed(_) => None,
        Ending::Completed | Ending::Aborted => accounting.finished_response().cloned(),
    };
    match finished {
        Some(finished) => {
            let output_items = finished.get("output").cloned();
            responses::record_state(
                &completion.state,
                &completion.chain,
                completion.pending,
                &completion.entry_body,
                completion.entry_protocol,
                output_items.as_ref(),
                Some(&finished),
                completion.retention_days,
                responses::Durability::Persist,
            );
        }
        // 流失败、或者断开时连最终对象都没等到：删除骨架状态，后续引用会得到
        // 明确的过期错误，而不是缺了助手轮次却继续发送（§15.2）。
        None => delete_state(completion),
    }
}

/// 删除一条不再可信的状态链记录；失败只记日志。
fn delete_state(completion: ResponsesCompletion) {
    let state = completion.state.clone();
    let gateway_id = completion.pending.gateway_id.clone();
    let group_id = completion.chain.group_id.clone();
    responses::forget_pending(&group_id, &gateway_id);
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        if let Err(error) = state
            .store
            .delete_response_state(&gateway_id, &group_id)
            .await
        {
            tracing::warn!(%error, "删除失效的 Responses 状态失败");
        }
    });
}
