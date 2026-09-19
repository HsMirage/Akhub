//! SSE 帧的切分与格式化，供流式嗅探与跨协议转换共用。
//!
//! 只处理传输层：把字节流切成以空行分隔的帧，取出 `event:` 与 `data:` 行。
//! 帧的语义由各协议的解析器决定。

use axum::body::Bytes;

/// 一帧 SSE：可选的事件名与拼接后的数据行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub event: Option<String>,
    pub data: String,
}

impl Frame {
    /// 是否是纯注释 / 心跳帧（没有任何 data 行）。
    pub fn is_comment(&self) -> bool {
        self.event.is_none() && self.data.is_empty()
    }

    /// OpenAI 风格的终止标记。
    pub fn is_done_marker(&self) -> bool {
        self.data == "[DONE]"
    }
}

/// 单个 SSE 帧允许的最大字节数（§19.4）。
///
/// 正常的帧是几百字节到几十 KB；一个请求的完整响应对象（Responses 的
/// `response.completed`）可能上百 KB，2 MB 给了很宽的余量。
///
/// 这个上限是**防无界增长**，不是防大帧：缓冲里攒不出完整帧时，字节会一直
/// 留在内存里，而"上游不停发字节、永远不发空行"是一条真实存在的故障与攻击
/// 路径（§19.4 要求所有缓冲都有上限）。
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// 逐块喂入字节，切出完整的帧。TCP 不保证帧边界，半帧会留在缓冲里等下一块。
#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
    /// 已经扫描过的位置：没有它，每次喂入都要把整个尾巴重扫一遍，慢速滴水
    /// 式的输入会退化成 O(n²)（§19.4）。
    scanned: usize,
    /// 缓冲越过上限时记下的原因。一旦置位就不再收字节。
    overflow: Option<String>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一块字节，返回其中已经完整的帧。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        if self.overflow.is_some() {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        let mut consumed = 0;
        while let Some(end) = find_frame_end(&self.buffer, consumed) {
            let raw = &self.buffer[consumed..end];
            let frame = parse_frame(&String::from_utf8_lossy(raw));
            consumed = end;
            frames.push(frame);
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
            self.scanned = 0;
        }
        // 扫过的部分下次不必重扫；留 3 字节是为了不漏掉横跨两次输入的分隔符
        // （空行分隔的最后一个字节可能刚到）。
        self.scanned = self.buffer.len().saturating_sub(3);
        if self.buffer.len() > MAX_FRAME_BYTES {
            // 不静默丢弃：把原因记下来交给调用方，由它按 §18.2 在流内报错。
            self.overflow = Some(format!(
                "上游单帧超过 {} 字节仍未结束（缓冲已丢弃），疑似上游异常或非 SSE 响应",
                MAX_FRAME_BYTES
            ));
            self.buffer = Vec::new();
            self.scanned = 0;
        }
        frames
    }

    /// 缓冲越过上限的原因。调用方看到它就必须终止这条流并报错，
    /// **不能**当作正常结束（§14.8：没有静默丢失）。
    pub fn overflow(&self) -> Option<&str> {
        self.overflow.as_deref()
    }

    /// 流结束时把残留的半帧也当作一帧交出（最后一帧可能没有结尾空行）。
    pub fn finish(&mut self) -> Option<Frame> {
        if self.overflow.is_some() {
            return None;
        }
        let rest = std::mem::take(&mut self.buffer);
        self.scanned = 0;
        if rest.iter().all(|b| b.is_ascii_whitespace()) {
            return None;
        }
        Some(parse_frame(&String::from_utf8_lossy(&rest)))
    }
}

/// 找到从 `from` 开始的第一个完整帧的结束位置（含分隔空行）。
pub fn find_frame_end(buffer: &[u8], from: usize) -> Option<usize> {
    let mut index = from;
    while index + 1 < buffer.len() {
        if buffer[index] == b'\n' && buffer[index + 1] == b'\n' {
            return Some(index + 2);
        }
        if index + 3 < buffer.len() && &buffer[index..index + 4] == b"\r\n\r\n" {
            return Some(index + 4);
        }
        index += 1;
    }
    None
}

/// 解析一帧的 `event:` 与 `data:` 行；注释行（冒号开头）是心跳，直接忽略。
pub fn parse_frame(frame: &str) -> Frame {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    Frame {
        event: event.filter(|e| !e.is_empty()),
        data: data.join("\n"),
    }
}

/// 输出一帧：有事件名时写 `event:` 行，随后写 `data:` 行与分隔空行。
pub fn format_frame(event: Option<&str>, data: &str) -> Bytes {
    let mut out = String::with_capacity(data.len() + 32);
    if let Some(event) = event {
        out.push_str("event: ");
        out.push_str(event);
        out.push('\n');
    }
    out.push_str("data: ");
    out.push_str(data);
    out.push_str("\n\n");
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_on_blank_lines_and_survive_chunk_boundaries() {
        let mut reader = FrameReader::new();
        assert!(reader.push(b"event: ping\ndata: {\"a\":1}").is_empty());
        let frames = reader.push(b"\n\n: comment\n\ndata: x\ndata: y\n\n");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].event.as_deref(), Some("ping"));
        assert_eq!(frames[0].data, "{\"a\":1}");
        assert!(frames[1].is_comment());
        assert_eq!(frames[2].data, "x\ny", "多行 data 用换行拼接");
    }

    #[test]
    fn crlf_and_trailing_partial_frames_are_handled() {
        let mut reader = FrameReader::new();
        let frames = reader.push(b"data: a\r\n\r\ndata: [DONE]");
        assert_eq!(frames.len(), 1);
        let last = reader.finish().unwrap();
        assert!(last.is_done_marker());
        assert!(reader.finish().is_none());
    }

    #[test]
    fn formatted_frames_parse_back() {
        let bytes = format_frame(Some("message_start"), "{\"x\":1}");
        let frame = parse_frame(std::str::from_utf8(&bytes).unwrap());
        assert_eq!(frame.event.as_deref(), Some("message_start"));
        assert_eq!(frame.data, "{\"x\":1}");
    }

    /// 单帧超过上限时不能无界增长，且必须留下可上报的原因（§19.4、§14.8）。
    #[test]
    fn an_endless_frame_is_capped_and_reported() {
        let mut reader = FrameReader::new();
        // 上游一直发字节但从不发空行——真实存在的故障与攻击路径。
        let filler = vec![b'x'; 64 * 1024];
        let mut total = 0usize;
        let mut frames = Vec::new();
        while reader.overflow().is_none() && total < MAX_FRAME_BYTES * 3 {
            frames.extend(reader.push(&filler));
            total += filler.len();
        }
        assert!(frames.is_empty(), "没有完整帧就不该产出帧");
        let reason = reader.overflow().expect("必须记下溢出原因");
        assert!(reason.contains("单帧超过"), "{reason}");
        // 触顶之后缓冲必须被释放，而不是留着那份内存。
        assert!(
            reader.buffer.len() <= MAX_FRAME_BYTES,
            "缓冲必须被清掉：{}",
            reader.buffer.len()
        );
        // 溢出之后不再收字节，也不再产出帧。
        assert!(reader.push(b"data: {}\n\n").is_empty());
        assert!(reader.finish().is_none(), "溢出后不能把残渣当成最后一帧");
    }

    /// 正常的大帧（例如带上完整响应对象的收尾事件）不该被误伤。
    #[test]
    fn a_large_but_legal_frame_still_parses() {
        let mut reader = FrameReader::new();
        let payload = "y".repeat(512 * 1024);
        let raw = format!("event: response.completed\ndata: {{\"v\":\"{payload}\"}}\n\n");
        let frames = reader.push(raw.as_bytes());
        assert_eq!(frames.len(), 1, "512 KB 的帧必须正常解析");
        assert_eq!(frames[0].event.as_deref(), Some("response.completed"));
        assert!(reader.overflow().is_none());
    }

    /// 跨块到达的帧仍然能拼起来——加了扫描游标之后最容易坏的就是这一条。
    #[test]
    fn a_frame_split_across_many_chunks_still_arrives() {
        let mut reader = FrameReader::new();
        let mut frames = Vec::new();
        for byte in b"data: hello\n\n" {
            frames.extend(reader.push(&[*byte]));
        }
        assert_eq!(frames.len(), 1, "逐字节喂入也要拼出一个完整帧");
        assert_eq!(frames[0].data, "hello");
    }

    /// 分隔符恰好横跨两次输入时不能漏掉（扫描游标留了 3 字节就是为这个）。
    #[test]
    fn a_delimiter_split_across_chunks_is_not_missed() {
        for split in 1..6 {
            let raw = b"data: a\n\nb";
            let mut reader = FrameReader::new();
            let mut frames = reader.push(&raw[..split]);
            frames.extend(reader.push(&raw[split..]));
            assert_eq!(frames.len(), 1, "在第 {split} 字节切开时漏帧了");
            assert_eq!(frames[0].data, "a");
        }
    }
}
