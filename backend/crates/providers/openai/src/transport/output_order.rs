//! API Key Responses 上游交错输出项的有界交付兼容。
//!
//! Codex 0.154 的 turn consumer 只维护一个 active item。后项 added 早于前项 done
//! 会让前项 done 清空后项，丢失增量展示。这里只延后实际交错的后项原帧；
//! 实际重排时仅调整顶层 sequence_number 的数字字节；不重写其他字段或补造 done。

mod sequence;

use std::collections::VecDeque;

use bytes::Bytes;
use futures::StreamExt;
use gateway_protocol::openai::sse::{SseEventDecoder, SseFrame};
use serde_json::Value;

use super::client::CodexBackendSseStream;
use sequence::{SequenceSlots, SequencedFrame};

const MAX_DEFERRED_BYTES: usize = 1024 * 1024;
const MAX_DEFERRED_FRAMES: usize = 256;

/// 串行交付实际交错的输出项；无交错时保持每帧原始字节与顺序。
/// 仅由标准 API Key 的 HTTP/SSE transport 启用，OAuth/WS 不使用此适配。
pub fn serialize_output_items(mut stream: CodexBackendSseStream) -> CodexBackendSseStream {
    Box::pin(async_stream::stream! {
        let mut decoder = SseEventDecoder::default();
        let mut order = OutputItemOrder::default();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    for frame in decoder.push_frames(&bytes) {
                        for raw in order.push(frame) {
                            yield Ok(raw);
                        }
                    }
                }
                Err(error) => {
                    // 保留已收到的原文和原错误；不能把半途失败补成成功或重试信号。
                    for frame in decoder.finish_frames() {
                        for raw in order.push(frame) {
                            yield Ok(raw);
                        }
                    }
                    for raw in order.finish() {
                        yield Ok(raw);
                    }
                    yield Err(error);
                    return;
                }
            }
        }
        for frame in decoder.finish_frames() {
            for raw in order.push(frame) {
                yield Ok(raw);
            }
        }
        for raw in order.finish() {
            yield Ok(raw);
        }
    })
}

#[derive(PartialEq, Eq)]
struct ItemKey {
    index: u64,
    id: String,
}

#[derive(PartialEq, Eq)]
enum ItemPhase {
    Added,
    Content,
    Done,
}

struct ItemFrame {
    key: ItemKey,
    phase: ItemPhase,
    raw: SequencedFrame,
}

enum FrameKind {
    Item(ItemKey, ItemPhase),
    Terminal,
    Other,
    Opaque,
}

#[derive(Default)]
struct OutputItemOrder {
    active: Option<ItemKey>,
    deferred: VecDeque<ItemFrame>,
    deferred_bytes: usize,
    disabled: bool,
    sequences: SequenceSlots,
}

impl OutputItemOrder {
    fn push(&mut self, frame: SseFrame) -> Vec<Bytes> {
        if self.disabled {
            return vec![Bytes::from(frame.into_parts().0)];
        }
        let mut kind = classify(&frame);
        let (raw, valid_sequence) = SequencedFrame::new(frame);
        if !valid_sequence {
            kind = FrameKind::Opaque;
        }
        self.sequences.observe(&raw);
        let (key, phase) = match kind {
            FrameKind::Item(key, phase) => (key, phase),
            FrameKind::Other => return vec![self.sequences.emit(raw)],
            FrameKind::Terminal | FrameKind::Opaque => {
                let mut output = self.finish();
                output.push(self.sequences.emit(raw));
                return output;
            }
        };
        let is_active = self.active.as_ref() == Some(&key);
        let identity_conflict = self
            .active
            .iter()
            .chain(self.deferred.iter().map(|frame| &frame.key))
            .any(|known| known != &key && (known.index == key.index || known.id == key.id));
        if identity_conflict {
            let mut output = self.finish();
            output.push(self.sequences.emit(raw));
            return output;
        }
        if self.active.is_none() && phase == ItemPhase::Added {
            self.active = Some(key);
            return vec![self.sequences.emit(raw)];
        }
        if is_active && phase != ItemPhase::Added {
            let mut output = vec![self.sequences.emit(raw)];
            if phase == ItemPhase::Done {
                self.active = None;
                self.drain_ready(&mut output);
            }
            return output;
        }
        let already_deferred = self.deferred.iter().any(|frame| frame.key == key);
        let can_defer = self.active.is_some()
            && !is_active
            && if phase == ItemPhase::Added {
                !already_deferred
            } else {
                already_deferred
            };
        if can_defer
            && self.deferred.len() < MAX_DEFERRED_FRAMES
            && raw.len() <= MAX_DEFERRED_BYTES.saturating_sub(self.deferred_bytes)
        {
            self.deferred_bytes += raw.len();
            self.deferred.push_back(ItemFrame { key, phase, raw });
            return Vec::new();
        }
        // 身份不完整、重复 added 或超限时放弃适配，完整释放原帧；不臆造生命周期。
        let mut output = self.finish();
        output.push(self.sequences.emit(raw));
        output
    }

    fn drain_ready(&mut self, output: &mut Vec<Bytes>) {
        while !self.deferred.is_empty() {
            if self.active.is_none() {
                let frame = self.deferred.pop_front().expect("deferred frame");
                self.deferred_bytes -= frame.raw.len();
                if frame.phase != ItemPhase::Added {
                    output.push(self.sequences.emit(frame.raw));
                    output.extend(self.finish());
                    return;
                }
                self.active = Some(frame.key);
                output.push(self.sequences.emit(frame.raw));
            }
            let Some(index) = self
                .deferred
                .iter()
                .position(|frame| self.active.as_ref() == Some(&frame.key))
            else {
                return;
            };
            let frame = self
                .deferred
                .remove(index)
                .expect("matching deferred frame");
            self.deferred_bytes -= frame.raw.len();
            output.push(self.sequences.emit(frame.raw));
            if frame.phase == ItemPhase::Done {
                self.active = None;
            }
        }
    }

    fn finish(&mut self) -> Vec<Bytes> {
        self.disabled = true;
        self.active = None;
        self.deferred_bytes = 0;
        self.deferred
            .drain(..)
            .map(|frame| self.sequences.emit(frame.raw))
            .collect()
    }
}

fn classify(frame: &SseFrame) -> FrameKind {
    let [event] = frame.events() else {
        // 心跳注释不参与条目生命周期，也不应使正在等待的交错项提前释放。
        return if frame.raw().starts_with(b":") {
            FrameKind::Other
        } else {
            FrameKind::Opaque
        };
    };
    if event.data.trim() == "[DONE]" {
        return FrameKind::Terminal;
    }
    let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
        return FrameKind::Opaque;
    };
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return FrameKind::Opaque;
    };
    if matches!(
        kind,
        "error"
            | "response.failed"
            | "response.completed"
            | "response.incomplete"
            | "response.cancelled"
    ) {
        return FrameKind::Terminal;
    }
    let phase = match kind {
        "response.output_item.added" => ItemPhase::Added,
        "response.output_item.done" => ItemPhase::Done,
        _ if value.get("item_id").is_some() => ItemPhase::Content,
        _ => return FrameKind::Other,
    };
    let id = match phase {
        ItemPhase::Added | ItemPhase::Done => value.get("item").and_then(|item| item.get("id")),
        ItemPhase::Content => value.get("item_id"),
    };
    match (
        value.get("output_index").and_then(Value::as_u64),
        id.and_then(Value::as_str),
    ) {
        (Some(index), Some(id)) if !id.is_empty() => FrameKind::Item(
            ItemKey {
                index,
                id: id.to_owned(),
            },
            phase,
        ),
        _ => FrameKind::Opaque,
    }
}
