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
    };

    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut filter = crate::gateway::translate::UnrequestedUsageFilter::new(strip_unrequested_usage);
        let mut upstream = body.into_data_stream();
        // 内层转发（private_stream / translate）会把上游断流改写成错误帧再正常收尾，
        //  accounting 不一定看得到错误。这里自己探一遍错误帧，收尾时据此补计费终止帧。
        let mut error_probe = crate::protocol::sse::FrameReader::new();
        let mut saw_error_frame = false;
        loop {
            let next = if let Some(timeout) = idle_timeout {
                match tokio::time::timeout(timeout, upstream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        tracing::warn!(%request_id, idle_secs = timeout.as_secs(), "流式响应连续无数据，结束等待");
                        let frames = guard.settle(Ending::Failed("upstream_timeout"));
                        // 按上游口径补出带用量的终止帧，让下游正常计费；拿不到用量
                        // 时退回原来的超时错误事件，至少把失败如实告诉客户端。
                        if frames.is_empty() {
                            yield Ok::<_, axum::Error>(crate::protocol::stream_error(
                                protocol,
                                crate::gateway::error::ErrorCode::UpstreamTimeout,
                                "上游流式响应长时间未返回数据，请重试",
                                Some(&request_id),
                            ));
                        } else {
                            for frame in frames {
                                yield Ok::<_, axum::Error>(frame);
                            }
                        }
                        return;
                    }
                }
            } else {
                upstream.next().await
            };
            match next {
                Some(Ok(chunk)) => {
                    guard.accounting.push(&chunk);
                    for frame in error_probe.push(&chunk) {
                        if crate::protocol::frame_error(&frame).is_some() {
                            saw_error_frame = true;
                        }
                    }
                    if strip_unrequested_usage {
                        let bytes = filter.push(&chunk);
                        if !bytes.is_empty() {
                            yield Ok::<_, axum::Error>(bytes);
                        }
                    } else {
                        yield Ok::<_, axum::Error>(chunk);
                    }
                }
                Some(Err(error)) => {
                    let frames = guard.settle(Ending::Failed("upstream_exhausted"));
                    // 上游断流（EOF）：补出带用量的终止帧让下游计费，而不是把传输
                    // 错误直接抛给下游——那会让 sub2api 当成可重放失败整条丢弃。
                    if frames.is_empty() {
                        yield Err::<axum::body::Bytes, _>(error);
                    } else {
                        for frame in frames {
                            yield Ok::<_, axum::Error>(frame);
                        }
                    }
                    return;
                }
                None => break,
            }
        }
        // 残留的半帧也要过一遍过滤，否则它会带着 usage 漏给下游。
        if let Some(tail) = filter.finish() {
            yield Ok::<_, axum::Error>(tail);
        }
        if let Some(frame) = error_probe.finish()
            && crate::protocol::frame_error(&frame).is_some()
        {
            saw_error_frame = true;
        }
        // 流里出现过错误帧（上游断流被内层改写成错误事件）：按失败结算并补计费
        // 终止帧；否则才是正常收尾。
        let ending = if saw_error_frame {
            Ending::Failed("upstream_protocol_error")
        } else {
            Ending::Completed
        };
        for frame in guard.settle(ending) {
            yield Ok::<_, axum::Error>(frame);
        }
    };

    Response::from_parts(parts, Body::from_stream(stream))
}

/// 结算守卫：正常结束显式结算，客户端断开由 `Drop` 兜底。
struct SettlementGuard {
    settlement: Option<StreamSettlement>,
    accounting: StreamAccounting,
    /// 异常结束时补发终止帧用的（Responses 网关 ID、输入估算）。
    billing: Option<(Option<String>, u64)>,
}

impl SettlementGuard {
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
            // 终止信号已经转发、下游读到收尾就关连接（sub2api / New API 的常态）：
            // 回答是完整送达的，不是掉线。现场 DeepSeek 流 93% 被记成 client_gone。
            Ending::Aborted
                if self.accounting.error().is_none() && self.accounting.answer_completed() =>
            {
                Ending::Completed
            }
            other => other,
        };
        // 协议先取出来：settlement 马上要被 move 进 settle_one。
        let protocol = settlement.protocol;
        let frames = match ending {
            // 正常收尾的流上游已经发过 usage，再补一帧会重复计费。
            Ending::Completed => Vec::new(),
            Ending::Failed(_) | Ending::Aborted => self
                .billing
                .as_ref()
                .map(|(responses_id, input_estimate)| {
                    let breakdown = crate::gateway::stream::billing_usage(
                        self.accounting.usage_breakdown(),
                        *input_estimate,
                    );
                    crate::protocol::terminal_usage_frames(
                        protocol,
                        crate::protocol::canonical::Usage {
                            input: breakdown.input,
                            output: breakdown.output,
                            cache_read: None,
                            cache_write: None,
                            reasoning: breakdown.reasoning,
                        },
                        responses_id.clone(),
                    )
                })
                .unwrap_or_default(),
        };
        settle_one(settlement, ending, &self.accounting);
        frames
    }
}

impl Drop for SettlementGuard {
    fn drop(&mut self) {
        // 客户端断开或任务被取消：不是上游的错，但状态链必须处理。
        // 连接已经没了，补发的终止帧无处可送，丢弃即可（结算照常落库）。
        let _ = self.settle(Ending::Aborted);
    }
}

fn settle_one(settlement: StreamSettlement, ending: Ending, accounting: &StreamAccounting) {
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
    let usage = accounting.usage_breakdown();
    record.cache_read_tokens = usage.cache_read.map(|value| value as i64);
    record.cache_write_tokens = usage.cache_write.map(|value| value as i64);
    record.reasoning_tokens = usage.reasoning.map(|value| value as i64);
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
