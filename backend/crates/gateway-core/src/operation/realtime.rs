//! 实时会话跨 HTTP/WS 连接复用同一次执行及其账号租约。
use super::RawJsonPayload;
use bytes::Bytes;
use futures::{channel::mpsc, lock::Mutex};
use std::{fmt, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeTransport {
    WebRtc,
    WebSocket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeInput {
    /// 仅加入本次执行创建的通话，不接受客户端指定的上游通话 ID。
    Attach,
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close {
        code: u16,
        reason: String,
    },
}

#[derive(Clone)]
pub struct RealtimeRequest {
    pub(super) payload: RawJsonPayload,
    transport: RealtimeTransport,
    input: Arc<Mutex<mpsc::Receiver<RealtimeInput>>>,
}
impl RealtimeRequest {
    pub fn new(
        payload: RawJsonPayload,
        transport: RealtimeTransport,
        input: mpsc::Receiver<RealtimeInput>,
    ) -> Self {
        Self {
            payload,
            transport,
            input: Arc::new(Mutex::new(input)),
        }
    }
    pub const fn payload(&self) -> &RawJsonPayload {
        &self.payload
    }
    pub const fn transport(&self) -> RealtimeTransport {
        self.transport
    }
    pub fn input(&self) -> Arc<Mutex<mpsc::Receiver<RealtimeInput>>> {
        Arc::clone(&self.input)
    }
}
impl PartialEq for RealtimeRequest {
    fn eq(&self, other: &Self) -> bool {
        self.payload == other.payload
            && self.transport == other.transport
            && Arc::ptr_eq(&self.input, &other.input)
    }
}
impl fmt::Debug for RealtimeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealtimeRequest")
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}
