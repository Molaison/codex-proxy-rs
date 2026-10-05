//! 同一次原生执行持有创建、sideband、凭据与容量，直到实时通话结束。
use super::*;
use crate::transport::{CodexRequestContext, realtime::failure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::SinkExt;
use gateway_core::operation::{RealtimeInput, RealtimeRequest, RealtimeTransport};
use tokio_tungstenite::tungstenite::{
    Message,
    protocol::{CloseFrame, frame::coding::CloseCode},
};

fn wire(kind: &str, data: Value) -> Result<ProtocolWireEvent, ProviderError> {
    ProtocolWireEvent::json("openai-live", Some(kind.to_owned()), data)
        .map_err(|e| failure(e.to_string()))
}
impl CodexProvider {
    pub(super) async fn execute_realtime(
        &self,
        request: &RealtimeRequest,
        context: AttemptContext,
    ) -> Result<ProviderStream, ProviderError> {
        let selection_started = Instant::now();
        let affinity = derive_codex_endpoint_session_affinity(
            request.payload(),
            context.client_api_key_ref(),
            "id",
        );
        let lease = Arc::new(
            self.selector
                .select_for_realtime(&SelectCodexProviderEndpointCredential {
                    request_url: &self.responses_url,
                    attempt: &context,
                    session_affinity: affinity.as_ref(),
                })
                .await
                .map_err(map_selection_error)?,
        );
        let client = self
            .client_for_request(&context)?
            .for_account(lease.account())
            .map_err(|e| failure(e.to_string()))?
            .with_authentication(lease.authentication());
        let metadata = ProviderCallMetadata::for_provider_endpoint(
            ProviderKind::new(PROVIDER_NAME).map_err(|e| failure(e.to_string()))?,
            lease.account_id().clone(),
            UpstreamTransport::new("websocket").map_err(|e| failure(e.to_string()))?,
        )
        .with_selection_observation(ProviderSelectionObservation::new(
            selection_started.elapsed().as_millis() as u64,
            lease.capacity_snapshot(),
        ));
        let request = request.clone();
        let guard = Arc::clone(&lease);
        let events: EventStream = Box::pin(async_stream::try_stream! {
            let auth = lease.authentication().authorization_header().map_err(|e| failure(e.to_string()))?;
            let cookies = build_cookie_header(lease.cookies())?;
            let mut request_context = CodexRequestContext::auxiliary(auth.expose_secret(), lease.account().upstream_account_id(), context.request_id().as_str(), Some(lease.installation_id()));
            request_context.cookie_header = cookies.as_ref().map(ExposeSecret::expose_secret);
            let headers = client.live_headers(request_context, request.payload().context())?;
            let response_meta = ResponseMeta::for_provider_endpoint(context.request_id().as_str());
            let input = request.input();
            let mut input = input.lock().await;
            let call_id;
            if request.transport() == RealtimeTransport::WebRtc {
                let created = tokio::select! {
                    _ = context.cancellation().cancelled() => Err(failure("Live creation cancelled")),
                    result = client.live_create(request.payload().body().clone(), headers.clone()) => result,
                }?;
                call_id = Some(created.1);
                let sdp = String::from_utf8(created.2.to_vec()).map_err(|e| failure(e.to_string()))?;
                yield ProviderEvent::canonical_with_wire(vec![GatewayEvent::Started(response_meta.clone())], wire("handshake", serde_json::json!({
                    "status":created.0,"body":sdp,"account_id":lease.account_id().as_str(),"call_id":call_id,
                }))?);
                let attach = tokio::select! {
                    _ = context.cancellation().cancelled() => None,
                    next = input.next() => next,
                };
                if !matches!(attach, Some(RealtimeInput::Attach)) {
                    yield ProviderEvent::canonical(GatewayEvent::Completed(response_meta.with_finish_reason(FinishReason::Stop)));
                    return;
                }
            } else { call_id = None; }
            let payload: Value = serde_json::from_slice(request.payload().body()).map_err(|e| failure(e.to_string()))?;
            let mut ws = tokio::select! {
                _ = context.cancellation().cancelled() => Err(failure("Live connection cancelled")),
                result = client.live_connect(call_id.as_deref(), payload.get("model").and_then(Value::as_str), headers) => result,
            }?;
            if request.transport() == RealtimeTransport::WebSocket {
                yield ProviderEvent::canonical_with_wire(vec![GatewayEvent::Started(response_meta.clone())], wire("handshake", serde_json::json!({
                    "status":101,"body":"","account_id":lease.account_id().as_str(),
                }))?);
            }
            yield ProviderEvent::wire(wire("ready", Value::Null)?);
            enum Input {
                Cancel,
                Command(Option<RealtimeInput>),
                Wire(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
            }
            loop {
                let incoming = tokio::select! {
                    _ = context.cancellation().cancelled() => Input::Cancel,
                    command = input.next() => Input::Command(command),
                    message = ws.next() => Input::Wire(message),
                };
                match incoming {
                    Input::Cancel | Input::Command(None) => { let _ = ws.close(None).await; break; }
                    Input::Command(command) => {
                        let message = match command {
                            Some(RealtimeInput::Text(text)) => Message::Text(text.into()),
                            Some(RealtimeInput::Binary(bytes)) => Message::Binary(bytes),
                            Some(RealtimeInput::Ping(bytes)) => Message::Ping(bytes),
                            Some(RealtimeInput::Pong(bytes)) => Message::Pong(bytes),
                            Some(RealtimeInput::Close {code,reason}) => {
                                let _ = ws.close(Some(CloseFrame { code: CloseCode::from(code), reason: reason.into() })).await; break;
                            }
                            Some(RealtimeInput::Attach) => continue,
                            None => break,
                        };
                        ws.send(message).await.map_err(|e| failure(e.to_string()))?;
                    }
                    Input::Wire(message) => {
                        let Some(message) = message else { break; };
                        let (kind,data) = match message.map_err(|e| failure(e.to_string()))? {
                            Message::Text(text) => ("text", Value::String(text.to_string())),
                            Message::Binary(bytes) => ("binary", Value::String(STANDARD.encode(bytes))),
                            Message::Ping(bytes) => ("ping", Value::String(STANDARD.encode(bytes))),
                            Message::Pong(bytes) => ("pong", Value::String(STANDARD.encode(bytes))),
                            Message::Close(frame) => {
                                let (code,reason) = frame.map_or((1000,String::new()), |f| (u16::from(f.code),f.reason.to_string()));
                                yield ProviderEvent::wire(wire("close", serde_json::json!({"code":code,"reason":reason}))?); break;
                            }
                            Message::Frame(_) => continue,
                        };
                        yield ProviderEvent::wire(wire(kind,data)?);
                    }
                }
            }
            // 上游未报告实时语音用量时保持未知，不补造 token 或费用。
            yield ProviderEvent::canonical(GatewayEvent::Completed(response_meta.with_finish_reason(FinishReason::Stop)));
        });
        Ok(ProviderStream::new(metadata, events, guard))
    }
}
