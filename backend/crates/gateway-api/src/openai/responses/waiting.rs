//! 长排队只输出 SSE 注释；请求仍由原执行 future 持有，断开时一起取消。

use std::time::Duration;

use axum::{
    body::{Body, BodyDataStream, Bytes},
    http::{HeaderValue, header},
    response::Response,
};
use futures::{StreamExt, future::BoxFuture};
use gateway_core::lifecycle::CancellationToken;
use gateway_protocol::openai::sse::{DONE_SSE_FRAME, response_failed_sse_event_with_id};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

pub(super) async fn with_queue_keepalive(
    mut pending: BoxFuture<'static, Response>,
    cancellation: CancellationToken,
) -> Response {
    tokio::select! {
        biased;
        response = &mut pending => return response,
        () = tokio::time::sleep(HEARTBEAT_INTERVAL) => {}
    }
    let waiting = WaitingResponse {
        pending: Some(pending),
        body: None,
        cancellation,
        first_heartbeat: true,
    };
    let stream = futures::stream::unfold(waiting, |mut waiting| async move {
        if waiting.first_heartbeat {
            waiting.first_heartbeat = false;
            return Some((Ok(Bytes::from_static(b": keep-alive\n\n")), waiting));
        }
        {
            if let Some(pending) = waiting.pending.as_mut() {
                let response = tokio::select! {
                    biased;
                    response = pending => response,
                    () = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                        return Some((Ok(Bytes::from_static(b": keep-alive\n\n")), waiting));
                    }
                };
                waiting.pending = None;
                let streaming = response.status().is_success()
                    && response
                        .headers()
                        .get(header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.starts_with("text/event-stream"));
                if streaming {
                    waiting.body = Some(response.into_body().into_data_stream());
                } else {
                    // 心跳已发出 HTTP 200，后续拒绝必须转为明确的流内失败，不能伪装成功或重放。
                    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                        .await
                        .unwrap_or_default();
                    let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
                    let error = value.as_ref().and_then(|value| value.get("error"));
                    let field = |key| {
                        error
                            .and_then(|value| value.get(key))
                            .and_then(serde_json::Value::as_str)
                    };
                    let mut failure = response_failed_sse_event_with_id(
                        None,
                        field("type").unwrap_or("server_error"),
                        field("code").unwrap_or("upstream_unavailable"),
                        field("message").unwrap_or("Request failed while waiting for capacity."),
                    );
                    failure.push_str(DONE_SSE_FRAME);
                    waiting.body = Some(Body::from(failure).into_data_stream());
                }
            }
            let chunk = waiting.body.as_mut()?.next().await?;
            Some((chunk, waiting))
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

struct WaitingResponse {
    pending: Option<BoxFuture<'static, Response>>,
    body: Option<BodyDataStream>,
    cancellation: CancellationToken,
    first_heartbeat: bool,
}

impl Drop for WaitingResponse {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
