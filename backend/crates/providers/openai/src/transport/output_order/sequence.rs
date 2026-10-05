//! 保留原始 SSE/JSON 字节，仅按交付顺序重分配已有的序号位置。

use std::{collections::VecDeque, ops::Range};

use bytes::Bytes;
use gateway_protocol::openai::sse::SseFrame;
use serde_json::Value;

pub(super) struct SequencedFrame {
    raw: Bytes,
    sequence: Option<(u64, Range<usize>)>,
}

impl SequencedFrame {
    pub(super) fn new(frame: SseFrame) -> (Self, bool) {
        let sequence = sequence_field(&frame);
        let valid = sequence.is_ok();
        (
            Self {
                raw: Bytes::from(frame.into_parts().0),
                sequence: sequence.unwrap_or(None),
            },
            valid,
        )
    }

    pub(super) fn len(&self) -> usize {
        self.raw.len()
    }
}

#[derive(Default)]
pub(super) struct SequenceSlots(VecDeque<u64>);

impl SequenceSlots {
    pub(super) fn observe(&mut self, frame: &SequencedFrame) {
        if let Some((number, _)) = &frame.sequence {
            self.0.push_back(*number);
        }
    }

    pub(super) fn emit(&mut self, frame: SequencedFrame) -> Bytes {
        let Some((original, range)) = frame.sequence else {
            return frame.raw;
        };
        // 每个有序号的输入帧只登记和交付一次，队列大小受暂存帧上限约束。
        let assigned = self.0.pop_front().expect("observed sequence slot");
        if original == assigned {
            return frame.raw;
        }
        let replacement = assigned.to_string();
        let mut raw = Vec::with_capacity(frame.raw.len() - range.len() + replacement.len());
        raw.extend_from_slice(&frame.raw[..range.start]);
        raw.extend_from_slice(replacement.as_bytes());
        raw.extend_from_slice(&frame.raw[range.end..]);
        Bytes::from(raw)
    }
}

fn sequence_field(frame: &SseFrame) -> Result<Option<(u64, Range<usize>)>, ()> {
    let [event] = frame.events() else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
        return Ok(None);
    };
    let Some(number) = value.get("sequence_number") else {
        return Ok(None);
    };
    let number = number.as_u64().ok_or(())?;
    let range = number_token(event.data.as_bytes()).ok_or(())?;
    let range = raw_data_range(frame.raw(), range).ok_or(())?;
    Ok(Some((number, range)))
}

/// JSON 已由 serde 验证；只定位顶层唯一字段，跳过嵌套对象和字符串内容。
fn number_token(data: &[u8]) -> Option<Range<usize>> {
    let mut depth = 0usize;
    let mut index = 0;
    let mut found = None;
    while index < data.len() {
        match data[index] {
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                index += 1;
            }
            b'"' => {
                let start = index;
                index += 1;
                loop {
                    match *data.get(index)? {
                        b'\\' => index += 2,
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                if depth != 1
                    || serde_json::from_slice::<String>(&data[start..index])
                        .ok()?
                        .as_str()
                        != "sequence_number"
                {
                    continue;
                }
                let mut value = skip_whitespace(data, index);
                if data.get(value) != Some(&b':') {
                    continue;
                }
                value = skip_whitespace(data, value + 1);
                let start = value;
                while data.get(value).is_some_and(u8::is_ascii_digit) {
                    value += 1;
                }
                if start == value || found.is_some() {
                    return None;
                }
                found = Some(start..value);
            }
            _ => index += 1,
        }
    }
    found
}

fn skip_whitespace(data: &[u8], mut index: usize) -> usize {
    while data.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

/// 将解析后的 data 偏移映射回物理 SSE 行；不改动 id/retry/注释、空白或换行。
fn raw_data_range(raw: &[u8], number: Range<usize>) -> Option<Range<usize>> {
    let mut start = 0;
    let mut data_offset = 0;
    while start < raw.len() {
        let end = raw[start..]
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .map_or(raw.len(), |length| start + length);
        let line_start = if start == 0 && raw.starts_with(b"\xef\xbb\xbf") {
            3
        } else {
            start
        };
        let line = &raw[line_start..end];
        let content = if line.starts_with(b"data:") {
            let mut offset = line_start + 5;
            if raw.get(offset) == Some(&b' ') {
                offset += 1;
            }
            Some(offset)
        } else if line == b"data" {
            Some(end)
        } else {
            None
        };
        if let Some(content) = content {
            let length = end - content;
            if number.start >= data_offset && number.end <= data_offset + length {
                return Some(
                    content + number.start - data_offset..content + number.end - data_offset,
                );
            }
            data_offset += length + 1;
        }
        start = end
            + if raw.get(end..end + 2) == Some(b"\r\n") {
                2
            } else {
                1
            };
    }
    None
}
