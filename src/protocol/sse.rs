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

/// 逐块喂入字节，切出完整的帧。TCP 不保证帧边界，半帧会留在缓冲里等下一块。
#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一块字节，返回其中已经完整的帧。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
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
        }
        frames
    }

    /// 流结束时把残留的半帧也当作一帧交出（最后一帧可能没有结尾空行）。
    pub fn finish(&mut self) -> Option<Frame> {
        let rest = std::mem::take(&mut self.buffer);
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
}
