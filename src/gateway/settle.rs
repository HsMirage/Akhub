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
    pub target_id: String,
    pub dimension: score::Dimension,
    /// 上游尝试开始的时间，用于真实总耗时。
    pub started: Instant,
    /// 请求开始时间，用于请求记录里的端到端耗时。
    pub request_started: Instant,
    /// 首个语义块时间（首字延迟）。
    pub first_token: Option<Duration>,
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
}

/// Responses 流完成后的状态链补写计划（§15.2）。
pub struct ResponsesCompletion {
    pub state: SharedState,
    pub chain: ChainPlan,
    pub pending: PendingState,
    pub entry_body: Value,
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
    let guard = SettlementGuard {
        settlement: Some(settlement),
        accounting: StreamAccounting::new(protocol),
    };

    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut filter = crate::gateway::translate::UnrequestedUsageFilter::new(strip_unrequested_usage);
        let mut upstream = body.into_data_stream();
        loop {
            match upstream.next().await {
                Some(Ok(chunk)) => {
                    guard.accounting.push(&chunk);
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
                    guard.settle(Ending::Failed("upstream_exhausted"));
                    yield Err::<axum::body::Bytes, _>(error);
                    return;
                }
                None => break,
            }
        }
        // 残留的半帧也要过一遍过滤，否则它会带着 usage 漏给下游。
        if let Some(tail) = filter.finish() {
            yield Ok::<_, axum::Error>(tail);
        }
        guard.settle(Ending::Completed);
    };

    Response::from_parts(parts, Body::from_stream(stream))
}

/// 结算守卫：正常结束显式结算，客户端断开由 `Drop` 兜底。
struct SettlementGuard {
    settlement: Option<StreamSettlement>,
    accounting: StreamAccounting,
}

impl SettlementGuard {
    fn settle(&mut self, ending: Ending) {
        let Some(settlement) = self.settlement.take() else {
            return;
        };
        self.accounting.finish();
        // 上游把错误写进流里时，生成器是"正常结束"的；语义上必须算失败。
        let ending = match ending {
            Ending::Completed if self.accounting.error().is_some() => {
                Ending::Failed("upstream_protocol_error")
            }
            other => other,
        };
        settle_one(settlement, ending, &self.accounting);
    }
}

impl Drop for SettlementGuard {
    fn drop(&mut self) {
        // 客户端断开或任务被取消：不是上游的错，但状态链必须处理。
        self.settle(Ending::Aborted);
    }
}

fn settle_one(settlement: StreamSettlement, ending: Ending, accounting: &StreamAccounting) {
    let usage = accounting.usage_tokens();
    let outcome = match ending {
        Ending::Completed => health::Outcome::Success,
        Ending::Failed(_) => health::Outcome::Fault,
        Ending::Aborted => health::Outcome::Neutral,
    };
    if let Some(admission) = settlement.admission {
        admission.settle(outcome, usage);
    }

    settlement.state.runtime.perf.observe(
        &settlement.target_id,
        settlement.dimension,
        &score::Sample {
            success: ending == Ending::Completed,
            // 客户端断开不是目标的质量信号：它既不算成功也不算失败，
            // 连样本都不进（§9.3、§12.3）。
            counts: ending != Ending::Aborted,
            // 样本用首字节而不是首字：评分要反映用户实际等了多久（§9.3）。
            first_token: settlement.first_byte,
            // 现在才是真正的"流结束时间"，不是首段提交时间。
            total: settlement.started.elapsed(),
            output_tokens: accounting.output_tokens(),
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
    record.first_token_ms = settlement.first_byte.map(|value| value.as_millis() as i64);
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
            let state = completion.state.clone();
            let Ok(handle) = tokio::runtime::Handle::try_current() else {
                tracing::warn!("流式结算时没有可用的异步运行时，状态链未补写");
                return;
            };
            handle.spawn(async move {
                responses::record_state(
                    &state,
                    &completion.chain,
                    completion.pending,
                    &completion.entry_body,
                    completion.entry_protocol,
                    output_items.as_ref(),
                    Some(&finished),
                    completion.retention_days,
                )
                .await;
            });
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
