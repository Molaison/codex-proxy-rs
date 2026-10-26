//! 验证写入阻塞后已到达的健康帧优先于过期的保活 deadline

use super::*;

#[derive(Clone, Copy, Debug)]
enum HealthFrame {
    Pong,
    Business,
    Liveness,
}

#[tokio::test]
async fn ready_pong_should_survive_expired_deadline_after_blocked_write() {
    healthy_frame_after_blocked_write(HealthFrame::Pong).await;
}

#[tokio::test]
async fn ready_business_frame_should_satisfy_ping_after_blocked_write() {
    healthy_frame_after_blocked_write(HealthFrame::Business).await;
}

#[tokio::test]
async fn ready_business_frame_should_refresh_liveness_after_blocked_write() {
    healthy_frame_after_blocked_write(HealthFrame::Liveness).await;
}

async fn healthy_frame_after_blocked_write(health: HealthFrame) {
    // 公平 select 的错误选择具有随机性，多次独立连接覆盖同时就绪的两个分支
    let mut failures = Vec::new();
    for attempt in 0..12 {
        if let Err(error) = blocked_write_with_ready_frame(health).await {
            failures.push(format!("attempt {attempt}: {error}"));
        }
    }
    assert!(failures.is_empty(), "{health:?}: {failures:#?}");
}

async fn blocked_write_with_ready_frame(health: HealthFrame) -> Result<(), String> {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(8192).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(1).unwrap();
    let addr = listener.local_addr().unwrap();
    let (idle_ready, idle_ready_rx) = tokio::sync::oneshot::channel();
    let (queued, queued_rx) = tokio::sync::oneshot::channel();
    let (drain, drain_rx) = tokio::sync::oneshot::channel();
    let (done, done_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // 不协商压缩，确保大请求会填满发送缓冲
        let mut websocket =
            tokio_tungstenite::accept_async_with_config(stream, Some(WebSocketConfig::default()))
                .await
                .unwrap();
        assert!(matches!(
            websocket.next().await.unwrap().unwrap(),
            Message::Text(_)
        ));
        websocket
            .send(Message::Text(
                completed_websocket_response("resp_warm", 1, 1).into(),
            ))
            .await
            .unwrap();
        let ping = if matches!(health, HealthFrame::Liveness) {
            None
        } else {
            let Message::Ping(payload) = websocket.next().await.unwrap().unwrap() else {
                panic!("idle connection should send a Ping");
            };
            Some(payload)
        };
        if ping.is_some() {
            // 丢弃读取 Ping 自动排队的 Pong，业务帧场景必须仅靠业务帧解除 deadline
            // 客户端尚在等待 idle_ready，没有后续帧会留在旧解析器中
            websocket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                websocket.into_inner(),
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                Some(WebSocketConfig::default()),
            )
            .await;
        }
        idle_ready.send(()).unwrap();

        // 先读帧头确认 pump 已进入发送；保留负载未读，使发送与读取串行的 pump 阻塞
        let stream = websocket.get_mut();
        assert_eq!(stream.read_u8().await.unwrap() & 0x0f, 1);
        let size = stream.read_u8().await.unwrap();
        assert_ne!(size & 0x80, 0, "client frame must be masked");
        let payload_len = match size & 0x7f {
            126 => u64::from(stream.read_u16().await.unwrap()),
            127 => stream.read_u64().await.unwrap(),
            len => u64::from(len),
        };
        let mut mask = [0; 4];
        stream.read_exact(&mut mask).await.unwrap();
        assert!(payload_len > 16 * 1024 * 1024);
        let message = match health {
            HealthFrame::Pong => Message::Pong(ping.unwrap()),
            HealthFrame::Business | HealthFrame::Liveness => Message::Text(
                json!({"type": "response.output_text.delta", "delta": "alive"})
                    .to_string()
                    .into(),
            ),
        };
        websocket.send(message).await.unwrap();
        queued.send(()).unwrap();
        drain_rx.await.unwrap();
        // 帧头已手动读取，仅排空这个请求的负载，不依赖被绕过的 WebSocket 解析器
        let drained = tokio::io::copy(
            &mut websocket.get_mut().take(payload_len),
            &mut tokio::io::sink(),
        )
        .await;
        // 误超时会导致客户端重置连接，由请求结果报告错误，而非让服务端任务先 panic
        if matches!(drained, Ok(len) if len == payload_len) {
            let _ = websocket
                .send(Message::Text(
                    completed_websocket_response("resp_healthy", 1, 1).into(),
                ))
                .await;
        }
        let _ = done_rx.await;
    });
    let liveness = matches!(health, HealthFrame::Liveness);
    let pool = Arc::new(CodexWebSocketPool::with_config(CodexWebSocketPoolConfig {
        ping_interval: (!liveness).then_some(Duration::from_millis(100)),
        ping_timeout: Duration::from_secs(2),
        liveness_timeout: liveness.then_some(Duration::from_secs(2)),
        maintenance_interval: None,
        ..CodexWebSocketPoolConfig::default()
    }));
    let backend = CodexBackendClient::new(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        format!("http://{addr}"),
        test_wire_profile(),
    )
    .with_websocket_pool(Arc::clone(&pool));
    let request = pooled_websocket_request("blocked-write-health");
    backend
        .create_response(&request, request_context("warm", Some("chatgpt-account")))
        .await
        .unwrap_or_else(|error| panic!("warm request failed: {error}"));
    timeout(Duration::from_secs(5), idle_ready_rx)
        .await
        .unwrap()
        .unwrap();
    let mut request = codex_request_with_prompt_cache_key(
        "gpt-test",
        "be brief",
        vec![json!({"role": "user", "content": "x".repeat(16 * 1024 * 1024)})],
        "blocked-write-health",
    );
    request.set_previous_response_id(Some("resp_previous".to_owned()));
    request.previous_response_scope = Some(PreviousResponseScope::Persisted);
    request.local_conversation_id = Some("blocked-write-health".to_owned());
    let result = {
        let pending = backend.create_response(
            &request,
            request_context("blocked", Some("chatgpt-account")),
        );
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("request should still be sending: {}", result.is_ok()),
            ready = queued_rx => ready.unwrap(),
            () = tokio::time::sleep(Duration::from_secs(5)) => panic!("write should start"),
        }
        // 帧已到达而 pump 仍在等待写入，将保活 deadline 推至过期再恢复读取
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::time::resume();
        drain.send(()).unwrap();
        timeout(Duration::from_secs(10), &mut pending)
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()))
            .and_then(|response| {
                if response.body.contains("resp_healthy") {
                    Ok(())
                } else {
                    Err(format!("missing healthy response: {}", response.body))
                }
            })
    };
    let _ = done.send(());
    pool.shutdown().await;
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    result
}
