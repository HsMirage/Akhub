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
//!   只是首字时间，用它喂 EWMA 会系统性高估吞吐。
//! - 请求记录：流结束才落库，错误流的 `error_code` 不再伪装成成功。
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
    pub first_token: Duration,
    /// 提交那一刻生成的记录，流结束后才真正落库。
    pub record: RequestRecord,
    /// 健康与限额准入；流结束时释放。
    pub admission: Option<health::Admission>,
    /// Responses 入口才有的状态链补写计划。
    pub responses: Option<ResponsesCompletion>,
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
    Aborted,
}

/// 把结算绑定到响应体的完整生命周期。
pub fn settle_stream(response: Response, settlement: StreamSettlement) -> Response {
    let (parts, body) = response.into_parts();
    let protocol = settlement.protocol;
    let guard = SettlementGuard {
        settlement: Some(settlement),
        accounting: StreamAccounting::new(protocol),
    };

    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut upstream = body.into_data_stream();
        loop {
            match upstream.next().await {
                Some(Ok(chunk)) => {
                    guard.accounting.push(&chunk);
                    yield Ok::<_, axum::Error>(chunk);
                }
                Some(Err(error)) => {
                    guard.settle(Ending::Failed("upstream_exhausted"));
                    yield Err::<axum::body::Bytes, _>(error);
                    return;
                }
                None => break,
            }
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
            first_token: Some(settlement.first_token),
            // 现在才是真正的"流结束时间"，不是首段提交时间。
            total: settlement.started.elapsed(),
            output_tokens: accounting.output_tokens(),
        },
    );

    let mut record = settlement.record;
    record.duration_ms = settlement.request_started.elapsed().as_millis() as i64;
    if let Ending::Failed(code) = ending {
        record.error_code = Some(code.to_string());
    }
    settlement.state.recorder.record(record);

    let Some(completion) = settlement.responses else {
        return;
    };
    match ending {
        Ending::Completed => {
            // 用最终响应对象里的输出项补写完整历史；没有最终对象就不补写，
            // 宁可由后续引用报过期，也不能保存残缺历史。
            let Some(finished) = accounting.finished_response().cloned() else {
                delete_state(completion);
                return;
            };
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
        // 流失败或客户端中断：删除骨架状态，后续引用会得到明确的过期错误，
        // 而不是缺了助手轮次却继续发送（§15.2）。
        Ending::Failed(_) | Ending::Aborted => delete_state(completion),
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
