//! 跨协议转换的按请求缓存（§14.3、§19.4）。
//!
//! 一次请求要对多个候选目标回答同一个问题："你的协议能表达这个请求吗，代价
//! 是什么？"逐个目标重新解析请求体会让资格过滤变成 O(目标数 × 请求体大小)。
//!
//! [`Translation`] 把这件事摊平：中间格式只解析一次，每个目标协议的请求体只
//! 发射一次，最多三份。同协议的目标根本不碰这里——透传路径连解析都不做。

use std::sync::{Arc, OnceLock};

use serde_json::Value;

use crate::domain::Protocol;
use crate::protocol::canonical::Request;
use crate::protocol::degrade::{Emitted, Fidelity, Unsupported};

/// 一次请求在三个协议上的转换结果缓存。
pub struct Translation<'a> {
    downstream: Protocol,
    body: &'a Value,
    canonical: OnceLock<Result<Request, Unsupported>>,
    emitted: [OnceLock<Result<Arc<Emitted>, Unsupported>>; 3],
}

impl<'a> Translation<'a> {
    pub fn new(downstream: Protocol, body: &'a Value) -> Self {
        Self {
            downstream,
            body,
            canonical: OnceLock::new(),
            emitted: [OnceLock::new(), OnceLock::new(), OnceLock::new()],
        }
    }

    pub fn downstream(&self) -> Protocol {
        self.downstream
    }

    /// 下游请求体的中间格式。解析失败说明请求本身有问题，与目标无关。
    fn canonical(&self) -> Result<&Request, Unsupported> {
        self.canonical
            .get_or_init(|| crate::protocol::parse_request(self.downstream, self.body))
            .as_ref()
            .map_err(Clone::clone)
    }

    /// 本次请求用到的能力名，供调度前的能力限制查询（§9.1、§16.7）。
    ///
    /// 解析失败时返回空：请求本身有问题，错误会在发射环节原样报出。
    pub fn requested_capabilities(&self) -> Vec<&'static str> {
        match self.canonical() {
            Ok(request) => request.requested_capabilities(),
            Err(_) => Vec::new(),
        }
    }

    /// 发往 `target` 协议的请求体，以及为此丢弃的白名单能力。
    ///
    /// 同协议直接返回原体：未知字段与供应商扩展因此天然保留（§14.1）。
    pub fn emit(&self, target: Protocol) -> Result<Arc<Emitted>, Unsupported> {
        if target == self.downstream {
            // 透传不需要缓存：`Arc` 只是为了与跨协议路径共用签名。
            return Ok(Arc::new(Emitted::lossless(self.body.clone())));
        }
        self.emitted[slot(target)]
            .get_or_init(|| {
                let canonical = self.canonical()?;
                crate::protocol::emit_request(target, canonical).map(Arc::new)
            })
            .clone()
    }

    /// 试算保真度，不产生请求体的所有权转移。
    pub fn fidelity(&self, target: Protocol) -> Result<Fidelity, Unsupported> {
        if target == self.downstream {
            return Ok(Fidelity::Lossless);
        }
        let emitted = self.emit(target)?;
        Ok(if emitted.degraded.is_empty() {
            Fidelity::Lossless
        } else {
            Fidelity::Degraded(emitted.degraded.clone())
        })
    }
}

fn slot(protocol: Protocol) -> usize {
    match protocol {
        Protocol::OpenAiChat => 0,
        Protocol::OpenAiResponses => 1,
        Protocol::AnthropicMessages => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_downstream_protocol_is_passed_through_byte_for_byte() {
        let body = json!({"model": "m", "messages": [], "厂商私有": 1});
        let translation = Translation::new(Protocol::OpenAiChat, &body);
        let emitted = translation.emit(Protocol::OpenAiChat).unwrap();
        assert_eq!(emitted.body, body, "同协议不解析、不重排、不丢字段");
        assert_eq!(
            translation.fidelity(Protocol::OpenAiChat).unwrap(),
            Fidelity::Lossless
        );
    }

    #[test]
    fn cross_protocol_bodies_are_computed_once_and_reused() {
        let body = json!({"model": "m", "max_tokens": 8, "messages": [
            {"role": "user", "content": "hi"}
        ]});
        let translation = Translation::new(Protocol::AnthropicMessages, &body);
        let first = translation.emit(Protocol::OpenAiChat).unwrap();
        let second = translation.emit(Protocol::OpenAiChat).unwrap();
        assert!(Arc::ptr_eq(&first, &second), "同一目标协议只发射一次");
    }

    #[test]
    fn an_inexpressible_request_reports_the_same_error_for_every_probe() {
        let body = json!({"model": "m", "messages": [], "n": 3});
        let translation = Translation::new(Protocol::OpenAiChat, &body);
        // 同协议照常透传，跨协议一律拒绝。
        assert!(translation.fidelity(Protocol::OpenAiChat).is_ok());
        for target in [Protocol::AnthropicMessages, Protocol::OpenAiResponses] {
            assert!(translation.fidelity(target).is_err());
        }
    }

    #[test]
    fn degradation_is_reported_per_target_protocol() {
        let body = json!({
            "model": "m", "max_tokens": 8,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "推理", "signature": "sig"},
                {"type": "text", "text": "答案"}
            ]}]
        });
        let translation = Translation::new(Protocol::AnthropicMessages, &body);
        assert_eq!(
            translation.fidelity(Protocol::OpenAiChat).unwrap(),
            Fidelity::Degraded(vec!["thinking".into()])
        );
        assert_eq!(
            translation.fidelity(Protocol::AnthropicMessages).unwrap(),
            Fidelity::Lossless,
            "原生目标不该因为跨协议的限制被降权"
        );
    }
}
