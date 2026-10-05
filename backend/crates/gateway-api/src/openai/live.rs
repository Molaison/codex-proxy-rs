//! 原生实时语音信令：同一次 Core 执行在创建和 sideband 期间持有同一账号。
use super::{
    auth::{authenticate_client, client_access_error_response},
    error::{engine_error_response, gateway_error_response},
    service::OpenAiService,
};
use crate::ApiState;
use axum::{
    body::{Body, Bytes, to_bytes},
    extract::{
        FromRequest, Multipart, Path, Query, Request, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt, channel::mpsc};
use gateway_core::{
    account::ProviderAccountId,
    engine::{
        CommitRequirement, ModelRequestId,
        execution::{AuthenticatedClient, StartedExecution},
    },
    operation::{Operation, RawJsonPayload, RealtimeInput, RealtimeRequest, RealtimeTransport},
};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc as queue, oneshot};

type LiveEvent = (String, Value);
#[derive(Default)]
pub(crate) struct LiveSessions {
    calls: Mutex<HashMap<String, Arc<LiveCall>>>,
}
struct LiveCall {
    client: String,
    account: ProviderAccountId,
    input: Mutex<mpsc::Sender<RealtimeInput>>,
    output: Mutex<Option<queue::Receiver<LiveEvent>>>,
    attached: AtomicBool,
    joined: AtomicBool,
}
struct Handshake {
    status: u16,
    body: String,
    account: ProviderAccountId,
}
fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error":{"type":"invalid_request_error","message":message}})),
    )
        .into_response()
}
fn context(headers: &HeaderMap) -> Map<String, Value> {
    let mut context = Map::new();
    for name in [
        "x-oai-attestation",
        "x-oai-attestation-token",
        "x-session-id",
        "session-id",
        "thread-id",
        "originator",
    ] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            context.insert(name.to_owned(), Value::String(value.to_owned()));
        }
    }
    context
}
async fn body(request: Request, state: &ApiState) -> Result<Value, Response> {
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if content_type.starts_with("application/json") {
        let bytes = to_bytes(request.into_body(), usize::MAX)
            .await
            .map_err(|_| error(StatusCode::BAD_REQUEST, "Cannot read Live request"))?;
        return serde_json::from_slice(&bytes)
            .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid Live JSON"));
    }
    let mut multipart = Multipart::from_request(request, state).await.map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "Live requires multipart sdp and session",
        )
    })?;
    let mut payload = Map::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid Live multipart"))?
    {
        let name = field.name().unwrap_or("").to_owned();
        if name != "sdp" && name != "session" {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Unknown Live multipart field",
            ));
        }
        let text = field
            .text()
            .await
            .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid Live field"))?;
        let value = if name == "sdp" {
            Value::String(text)
        } else {
            serde_json::from_str(&text)
                .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid Live session JSON"))?
        };
        if payload.insert(name, value).is_some() {
            return Err(error(StatusCode::BAD_REQUEST, "Duplicate Live field"));
        }
    }
    Ok(Value::Object(payload))
}

async fn start(
    service: &OpenAiService,
    client: AuthenticatedClient,
    payload: Value,
    headers: &HeaderMap,
    transport: RealtimeTransport,
) -> Result<(String, Arc<LiveCall>, Handshake, ModelRequestId), Response> {
    let (input, receiver) = mpsc::channel(64);
    let payload = RawJsonPayload::new(
        "openai",
        Bytes::from(
            serde_json::to_vec(&payload)
                .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid Live payload"))?,
        ),
    )
    .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid Live payload"))?
    .with_context(context(headers));
    let operation = Operation::Realtime(RealtimeRequest::new(payload, transport, receiver));
    let started = service
        .start_live(
            client.clone(),
            operation,
            headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            transport,
        )
        .await
        .map_err(|e| gateway_error_response(&e))?;
    let request_id = started.request_id.clone();
    let handle = format!("rtc_{}", uuid::Uuid::now_v7().simple());
    let (output_tx, output_rx) = queue::channel(64);
    let (handshake_tx, handshake_rx) = oneshot::channel();
    let registry = Arc::clone(&service.live);
    let cleanup_id = handle.clone();
    let guard = service
        .try_register_connection()
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is draining"))?;
    tokio::spawn(async move {
        let _guard = guard;
        drive(started, handshake_tx, output_tx).await;
        registry.calls.lock().await.remove(&cleanup_id);
    });
    let handshake = handshake_rx.await.map_err(|_| {
        error(
            StatusCode::BAD_GATEWAY,
            "Live execution ended before handshake",
        )
    })??;
    let call = Arc::new(LiveCall {
        client: client.policy().key_id().as_str().to_owned(),
        account: handshake.account.clone(),
        input: Mutex::new(input),
        output: Mutex::new(Some(output_rx)),
        attached: AtomicBool::new(false),
        joined: AtomicBool::new(transport == RealtimeTransport::WebSocket),
    });
    service
        .live
        .calls
        .lock()
        .await
        .insert(handle.clone(), Arc::clone(&call));
    Ok((handle, call, handshake, request_id))
}
async fn drive(
    started: StartedExecution,
    handshake: oneshot::Sender<Result<Handshake, Response>>,
    output: queue::Sender<LiveEvent>,
) {
    let mut session = started.session;
    let mut handshake = Some(handshake);
    let mut ready = false;
    loop {
        let next = if ready {
            session.next_event().await
        } else {
            match tokio::time::timeout(Duration::from_secs(60), session.next_event()).await {
                Ok(next) => next,
                Err(_) => {
                    session.cancel();
                    break;
                }
            }
        };
        let batch = match next {
            Ok(Some(batch)) => batch,
            Ok(None) => break,
            Err(e) => {
                if let Some(tx) = handshake.take() {
                    let _ = tx.send(Err(engine_error_response(&e)));
                } else {
                    let _ = output
                        .send((
                            "close".to_owned(),
                            json!({"code":1011,"reason":"Upstream Live connection failed"}),
                        ))
                        .await;
                }
                break;
            }
        };
        let commit = batch.commit_requirement() == CommitRequirement::CommitBeforeDelivery;
        let mut frames = Vec::new();
        let mut status = None;
        for event in batch.into_provider_events() {
            if let Some(wire) = event.wire_event().filter(|w| w.protocol() == "openai-live") {
                let kind = wire.event_type().unwrap_or("").to_owned();
                if kind == "handshake" {
                    status = wire
                        .data()
                        .get("status")
                        .and_then(Value::as_u64)
                        .map(|s| s as u16);
                }
                frames.push((kind, wire.data().clone()));
            }
        }
        if commit {
            if let Err(e) = session.commit_downstream(status).await {
                if let Some(tx) = handshake.take() {
                    let _ = tx.send(Err(engine_error_response(&e)));
                }
                session.cancel();
                break;
            }
        }
        for (kind, data) in frames {
            if kind == "handshake" {
                let parsed = data
                    .get("account_id")
                    .and_then(Value::as_str)
                    .and_then(|s| ProviderAccountId::new(s).ok());
                let result = parsed
                    .map(|account| Handshake {
                        account,
                        status: data["status"].as_u64().unwrap_or(502) as u16,
                        body: data["body"].as_str().unwrap_or("").to_owned(),
                    })
                    .ok_or_else(|| error(StatusCode::BAD_GATEWAY, "Invalid Live handshake"));
                if let Some(tx) = handshake.take() {
                    if tx.send(result).is_err() {
                        session.cancel();
                    }
                }
            } else {
                if kind == "ready" {
                    ready = true;
                }
                if output.send((kind, data)).await.is_err() {
                    session.cancel();
                    break;
                }
            }
        }
    }
    session.cancel();
    session.detach_finalize().await;
}

pub(crate) async fn create(State(state): State<ApiState>, request: Request) -> Response {
    let service = state.openai();
    let headers = request.headers().clone();
    let client = match authenticate_client(service, &headers).await {
        Ok(c) => c,
        Err(e) => return client_access_error_response(e),
    };
    let payload = match body(request, &state).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    if !payload.get("sdp").is_some_and(Value::is_string)
        || !payload.get("session").is_some_and(Value::is_object)
    {
        return error(StatusCode::BAD_REQUEST, "Live requires sdp and session");
    }
    let (id, _, handshake, request_id) = match start(
        service,
        client,
        payload,
        &headers,
        RealtimeTransport::WebRtc,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut response = Response::new(Body::from(handshake.body));
    *response.status_mut() =
        StatusCode::from_u16(handshake.status).unwrap_or(StatusCode::BAD_GATEWAY);
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/sdp"));
    if let Ok(location) = HeaderValue::from_str(&format!("/v1/live/{id}")) {
        response.headers_mut().insert("location", location);
    }
    super::with_model_request_id(response, &request_id)
}

pub(crate) async fn websocket(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    websocket: WebSocketUpgrade,
) -> Response {
    let service = state.openai();
    let client = match authenticate_client(service, &headers).await {
        Ok(c) => c,
        Err(e) => return client_access_error_response(e),
    };
    let payload = json!({"model":query.get("model")});
    let (_, call, _, request_id) = match start(
        service,
        client,
        payload,
        &headers,
        RealtimeTransport::WebSocket,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    super::with_model_request_id(attach(call, websocket).await, &request_id)
}
pub(crate) async fn sideband(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    let service = state.openai();
    let client = match authenticate_client(service, &headers).await {
        Ok(c) => c,
        Err(e) => return client_access_error_response(e),
    };
    let call = service.live.calls.lock().await.get(&id).cloned();
    let Some(call) = call.filter(|c| {
        c.client == client.policy().key_id().as_str()
            && client.policy().account_scope().allows(&c.account)
    }) else {
        return error(StatusCode::NOT_FOUND, "Live call not found for this client");
    };
    attach(call, websocket).await
}
async fn attach(call: Arc<LiveCall>, websocket: WebSocketUpgrade) -> Response {
    if call.attached.swap(true, Ordering::SeqCst) {
        return error(StatusCode::CONFLICT, "Live call already connected");
    }
    let Some(mut output) = call.output.lock().await.take() else {
        call.attached.store(false, Ordering::SeqCst);
        return error(StatusCode::GONE, "Live call has ended");
    };
    let first = !call.joined.swap(true, Ordering::SeqCst);
    if first
        && call
            .input
            .lock()
            .await
            .send(RealtimeInput::Attach)
            .await
            .is_err()
    {
        return error(StatusCode::GONE, "Live call has ended");
    }
    if first || !call.joined.load(Ordering::SeqCst) {
        match tokio::time::timeout(Duration::from_secs(60), output.recv()).await {
            Ok(Some((kind, _))) if kind == "ready" => {}
            _ => {
                call.attached.store(false, Ordering::SeqCst);
                return error(StatusCode::BAD_GATEWAY, "Live sideband handshake failed");
            }
        }
    }
    websocket.on_upgrade(move |socket| pump(socket, call, output))
}
async fn pump(socket: WebSocket, call: Arc<LiveCall>, mut output: queue::Receiver<LiveEvent>) {
    let (mut sink, mut source) = socket.split();
    let mut ended = false;
    loop {
        tokio::select! {
            incoming=source.next()=>{
                let command=match incoming {
                    Some(Ok(Message::Text(s)))=>RealtimeInput::Text(s.to_string()),
                    Some(Ok(Message::Binary(b)))=>RealtimeInput::Binary(b),
                    Some(Ok(Message::Ping(b)))=>RealtimeInput::Ping(b),
                    Some(Ok(Message::Pong(b)))=>RealtimeInput::Pong(b),
                    Some(Ok(Message::Close(frame)))=>{let (code,reason)=frame.map_or((1000,String::new()),|f|(f.code,f.reason.to_string()));ended=true;RealtimeInput::Close{code,reason}},
                    _=>break,
                };
                if call.input.lock().await.send(command).await.is_err()||ended{break;}
            }
            event=output.recv()=>{
                let Some((kind,data))=event else{ended=true;break};
                let message=match kind.as_str(){
                    "ready"=>continue,
                    "text"=>Message::Text(data.as_str().unwrap_or("").to_owned().into()),
                    "binary"|"ping"|"pong"=>{
                        let Ok(bytes)=STANDARD.decode(data.as_str().unwrap_or(""))else{ended=true;break};
                        match kind.as_str(){"binary"=>Message::Binary(bytes.into()),"ping"=>Message::Ping(bytes.into()),_=>Message::Pong(bytes.into())}
                    },
                    "close"=>{ended=true;Message::Close(Some(CloseFrame{code:data["code"].as_u64().unwrap_or(1011)as u16,reason:data["reason"].as_str().unwrap_or("").to_owned().into()}))},
                    _=>continue,
                };
                if sink.send(message).await.is_err()||ended{break;}
            }
        }
    }
    *call.output.lock().await = Some(output);
    call.attached.store(false, Ordering::SeqCst);
    if !ended {
        // 短暂断线保留原通话及账号租约，不创建新通话。
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if !call.attached.load(Ordering::SeqCst) {
                let _ = call
                    .input
                    .lock()
                    .await
                    .send(RealtimeInput::Close {
                        code: 1000,
                        reason: "Sideband reconnect window expired".to_owned(),
                    })
                    .await;
            }
        });
    }
}
