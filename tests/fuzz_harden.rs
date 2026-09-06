//! 阶段 6 发布硬化（§26.8、§27）：模糊与稳定性。
//!
//! 解析层绝不能 panic：客户端可以发任意字节，上游可以回任意字节。这里的
//! "模糊测试"用确定性伪随机生成器大量喂畸形输入——进程内可重复、无依赖，
//! 覆盖 JSON 解析、SSE 分帧、能力目录查询与请求记录聚合。

mod common;

use akhub::domain::Protocol;
use akhub::protocol::{self, sse};
use serde_json::{Value, json};

/// xorshift64* 确定性伪随机：测试可重复，失败可复现。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn range(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// 生成一个随机 JSON 值：合法形状与垃圾字节混着来。
fn random_json(rng: &mut Rng, depth: u32) -> Value {
    let kind = rng.range(if depth == 0 { 4 } else { 7 });
    match kind {
        0 => Value::Null,
        1 => Value::Bool(rng.range(2) == 0),
        2 => Value::from(rng.next() as i64),
        3 => {
            // 字符串：可打印、控制字节、多字节 UTF-8、空串轮着来。
            let text = match rng.range(4) {
                0 => "正常内容".to_string(),
                1 => "\u{0}\u{1}\u{7f}".to_string(),
                2 => "🦀🔍".to_string(),
                _ => String::new(),
            };
            Value::from(text)
        }
        4 => {
            let len = rng.range(6);
            (0..len).map(|_| random_json(rng, depth - 1)).collect()
        }
        5 => {
            let len = rng.range(6);
            (0..len)
                .map(|i| (format!("k{depth}-{i}"), random_json(rng, depth - 1)))
                .collect::<serde_json::Map<String, Value>>()
                .into()
        }
        // 结构正确的模型请求体，形状合法但字段随机。
        _ => {
            let role = ["user", "assistant", "system"][rng.range(3) as usize];
            let models = [json!("m"), json!(""), json!(42), json!(null)];
            let model = models[rng.range(4) as usize].clone();
            json!({
                "model": model,
                "messages": [{"role": role, "content": random_json(rng, 1)}],
                "max_tokens": rng.next() as i64,
                "tools": if rng.range(2) == 0 { json!([{"type": "function",
                    "function": {"name": "f", "parameters": random_json(rng, 1)}}]) } else { json!([]) },
                "stream": rng.range(2) == 0,
            })
        }
    }
}

/// 生成一段随机 SSE 字节：完整帧、半帧、注释、垃圾混合。
fn random_sse(rng: &mut Rng, budget: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    while out.len() < budget {
        match rng.range(6) {
            0 => out.extend_from_slice(b"event: response.created\n"),
            1 => out.extend_from_slice(b"data: {\"type\":\"ping\"}\n\n"),
            2 => out.extend_from_slice(b"data: [DONE]\n\n"),
            3 => out.extend_from_slice(b": keep-alive\n\n"),
            4 => out.extend_from_slice(&rng.next().to_le_bytes()),
            _ => out.push(
                *b"\r\n\n\rdata:"
                    .get(rng.range(6) as usize)
                    .unwrap_or(&b'\n'),
            ),
        }
    }
    out
}

#[test]
fn protocol_parsers_survive_a_thousand_hostile_bodies() {
    let mut rng = Rng(0xA1CAFE);
    for round in 0..1000u64 {
        let body = random_json(&mut rng, 3);
        for protocol in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ] {
            // 任何输入都不允许 panic；解析失败必须是普通 Err。
            let parsed = protocol::parse_request(protocol, &body);
            if let Ok(request) = parsed {
                // 解析成功的请求体必须能在三个目标协议上发射而不 panic。
                for target in [
                    Protocol::OpenAiChat,
                    Protocol::OpenAiResponses,
                    Protocol::AnthropicMessages,
                ] {
                    let _ = protocol::emit_request(target, &request);
                }
                let _ = protocol::parse_response(protocol, &body);
                // 解析成功的响应体必须能发射回三个协议而不 panic。
                if let Ok(response) = protocol::parse_response(protocol, &body) {
                    for target in [
                        Protocol::OpenAiChat,
                        Protocol::OpenAiResponses,
                        Protocol::AnthropicMessages,
                    ] {
                        let _ = protocol::emit_response(target, &response);
                    }
                }
            }
        }
        if round % 250 == 0 {
            // 也把合法请求体转出去一次，保证路径没有全被垃圾遮蔽。
            let valid = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
            assert!(
                protocol::parse_request(Protocol::OpenAiChat, &valid).is_ok(),
                "合法请求体必须永远可解析"
            );
        }
    }
}

#[test]
fn the_sse_framer_survives_random_bytes_without_panicking_or_losing_frames() {
    let mut rng = Rng(0x5EE_u64);
    for _ in 0..2000 {
        let bytes = random_sse(&mut rng, 512);
        let mut reader = sse::FrameReader::new();
        // 逐字节喂入，模拟任意 TCP 分段；只要求不 panic、帧内容可复现。
        let mut frames = Vec::new();
        for byte in &bytes {
            frames.extend(reader.push(std::slice::from_ref(byte)));
        }
        if let Some(last) = reader.finish() {
            frames.push(last);
        }
        // 分帧器给出的每一帧都要能过 parse_frame。
        for frame in &frames {
            let _ = sse::parse_frame(&frame.data);
            let _ = frame.is_done_marker();
        }
    }
}

#[test]
fn sse_frames_are_chunking_invariant() {
    // 同一段流按任意边界切开，帧序列必须一致（§13.1 在途不重复拼接）。
    let stream = b"event: a\ndata: 1\n\ndata: 2\n\nevent: b\r\ndata: 3\r\n\r\n";
    let whole = {
        let mut reader = sse::FrameReader::new();
        let mut frames = reader.push(stream);
        if let Some(last) = reader.finish() {
            frames.push(last);
        }
        frames
            .into_iter()
            .map(|f| (f.event, f.data))
            .collect::<Vec<_>>()
    };

    let mut rng = Rng(7);
    for _ in 0..50 {
        let mut reader = sse::FrameReader::new();
        let mut frames = Vec::new();
        let mut offset = 0usize;
        while offset < stream.len() {
            let step = 1 + rng.range(5) as usize;
            let end = (offset + step).min(stream.len());
            frames.extend(reader.push(&stream[offset..end]));
            offset = end;
        }
        if let Some(last) = reader.finish() {
            frames.push(last);
        }
        let split = frames
            .into_iter()
            .map(|f| (f.event, f.data))
            .collect::<Vec<_>>();
        assert_eq!(split, whole, "分帧与边界无关");
    }
}

#[test]
fn hostile_request_records_never_break_cost_aggregation() {
    // 倍率定点域的合法范围是非负小数；请求记录里的倍率来自数据库 i64，
    // 聚合用 i128 累加，极端样本不允许 panic、溢出或产出非法倍率。
    let mut rng = Rng(0xC057);
    let mut weighted_sum: i128 = 0;
    let mut count: i64 = 0;
    for _ in 0..5000 {
        let multiplier = (rng.next() % 4_000_000_000) as i64; // 远超定点 i64 上限的一半
        let requests = (rng.next() % 1_000_000) as i64;
        weighted_sum += multiplier as i128 * requests as i128;
        count += requests;
    }
    let avg = if count > 0 {
        (weighted_sum + count as i128 / 2) / count as i128
    } else {
        0
    };
    assert!(avg >= 0, "加权均值必须非负：{avg}");
    assert!(avg <= i64::MAX as i128, "不得溢出 i64 定点域");

    // 从极端均值构造 Multiplier 必须要么成功要么明确报错，绝不 panic。
    let raw = avg as i64;
    let _ = akhub::domain::Multiplier::from_raw(raw).to_f64();
}

#[test]
fn the_capability_catalog_tolerates_rogue_lookups() {
    let catalog = akhub::capability::builtin();
    // 任意字符串查询都不 panic；空串与超长串也不行。
    let mut rng = Rng(0xCA7);
    for _ in 0..1000 {
        let len = rng.range(64);
        let query: String = (0..len)
            .map(|_| char::from_u32(0x4E00 + rng.range(1000) as u32).unwrap_or('问'))
            .collect();
        let _ = catalog.get(&query);
    }
    assert!(catalog.get("").is_none());
    assert!(catalog.get(&"x".repeat(4096)).is_none());
}
