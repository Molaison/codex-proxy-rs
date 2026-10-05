use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use gateway_protocol::openai::sse::SseEventDecoder;
use provider_openai::transport::output_order::serialize_output_items;
use provider_openai::transport::{CodexBackendSseStream, CodexClientError};
use serde_json::{Value, json};

mod sequence;

fn frame(value: Value) -> Bytes {
    Bytes::from(format!(
        "event: {}\r\ndata: {value}\r\n\r\n",
        value["type"].as_str().unwrap()
    ))
}

fn item(phase: &str, index: u64, id: &str, kind: &str) -> Bytes {
    frame(json!({"type":format!("response.output_item.{phase}"),
        "output_index":index,"item":{"id":id,"type":kind}}))
}

fn delta(index: u64, id: &str, kind: &str, text: &str) -> Bytes {
    frame(json!({"type":kind,"output_index":index,"item_id":id,"delta":text}))
}

fn joined(frames: &[Bytes]) -> Vec<u8> {
    frames
        .iter()
        .flat_map(|frame| frame.iter().copied())
        .collect()
}

async fn reorder(bytes: &[u8], chunk_size: usize) -> Vec<u8> {
    let chunks = bytes
        .chunks(chunk_size)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect::<Vec<_>>();
    let stream: CodexBackendSseStream = Box::pin(futures::stream::iter(chunks));
    joined(
        &serialize_output_items(stream)
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
    )
}

// 模拟官方 0.154 turn.rs 的单 active_item 消费约束，而非适配器的排队算法。
fn single_active_consumer(bytes: &[u8]) -> Result<String, &'static str> {
    let mut active: Option<String> = None;
    let mut text = String::new();
    for event in SseEventDecoder::default().push(bytes).unwrap() {
        let value: Value = serde_json::from_str(&event.data).unwrap();
        match value["type"].as_str().unwrap() {
            "response.output_item.added" => {
                active = value["item"]["id"].as_str().map(str::to_owned)
            }
            "response.output_item.done" => active = None,
            "response.output_text.delta" | "response.function_call_arguments.delta" => {
                if active.as_deref() != value["item_id"].as_str() {
                    return Err("delta without matching active item");
                }
                text.push_str(value["delta"].as_str().unwrap());
            }
            _ => {}
        }
    }
    Ok(text)
}

#[tokio::test]
async fn interleaved_reasoning_done_does_not_clear_the_message_item() {
    // 保留现场事件顺序，ID/内容均为合成值；不依赖上游真实时间或随机 ID。
    let frames = vec![
        item("added", 0, "reason", "reasoning"),
        frame(json!({"type":"response.output_item.added","sequence_number":49,
            "output_index":1,"item":{"id":"message","type":"message"}})),
        frame(json!({"type":"response.content_part.added","sequence_number":50,
            "output_index":1,"item_id":"message","content_index":0,
            "part":{"type":"output_text","text":""}})),
        frame(json!({"type":"response.reasoning.done","sequence_number":51,
            "output_index":0,"item_id":"reason","text":"reasoning"})),
        frame(json!({"type":"response.output_item.done","sequence_number":52,
            "output_index":0,"item":{"id":"reason","type":"reasoning"}})),
        Bytes::from_static(b"id: upstream-id\nevent: response.output_text.delta\ndata: { \"type\": \"response.output_text.delta\", \"sequence_number\": 53, \"output_index\": 1, \"item_id\": \"message\", \"delta\": \"OK\", \"future\":90071992547409931234567890 }\n\n"),
        item("done", 1, "message", "message"),
        frame(json!({"type":"response.completed","response":{"id":"response","status":"completed"}})),
    ];
    let original = joined(&frames);
    assert!(single_active_consumer(&original).is_err());
    let expected = joined(&[
        frames[0].clone(),
        Bytes::from(
            String::from_utf8(frames[3].to_vec())
                .unwrap()
                .replace("\"sequence_number\":51", "\"sequence_number\":49"),
        ),
        Bytes::from(
            String::from_utf8(frames[4].to_vec())
                .unwrap()
                .replace("\"sequence_number\":52", "\"sequence_number\":50"),
        ),
        Bytes::from(
            String::from_utf8(frames[1].to_vec())
                .unwrap()
                .replace("\"sequence_number\":49", "\"sequence_number\":51"),
        ),
        Bytes::from(
            String::from_utf8(frames[2].to_vec())
                .unwrap()
                .replace("\"sequence_number\":50", "\"sequence_number\":52"),
        ),
        frames[5].clone(),
        frames[6].clone(),
        frames[7].clone(),
    ]);
    for chunk_size in [1, 7, original.len()] {
        let actual = reorder(&original, chunk_size).await;
        assert_eq!(
            actual, expected,
            "only reordered top-level sequence numbers may change"
        );
        assert_eq!(single_active_consumer(&actual), Ok("OK".to_owned()));
    }
}

#[tokio::test]
async fn normal_sequential_streams_keep_exact_bytes() {
    let original = joined(&[
        frame(json!({"type":"response.created","response":{"id":"response"}})),
        item("added", 0, "reason", "reasoning"),
        delta(0, "reason", "response.reasoning.delta", "分析"),
        item("done", 0, "reason", "reasoning"),
        Bytes::from_static(b": heartbeat\n\n"),
        item("added", 1, "message", "message"),
        delta(1, "message", "response.output_text.delta", "答案"),
        item("done", 1, "message", "message"),
        frame(json!({"type":"response.completed"})),
        Bytes::from_static(b"data: [DONE]\n\n"),
    ]);
    for chunk_size in [1, 11, original.len()] {
        assert_eq!(reorder(&original, chunk_size).await, original);
    }
}

#[tokio::test]
async fn interleaved_tool_arguments_keep_their_own_item_and_original_bytes() {
    let frames = vec![
        item("added", 0, "reason", "reasoning"),
        item("added", 1, "tool-a", "function_call"),
        item("added", 2, "tool-b", "function_call"),
        delta(
            1,
            "tool-a",
            "response.function_call_arguments.delta",
            "{\"a\":1}",
        ),
        item("done", 1, "tool-a", "function_call"),
        delta(
            2,
            "tool-b",
            "response.function_call_arguments.delta",
            "{\"b\":2}",
        ),
        item("done", 0, "reason", "reasoning"),
        item("done", 2, "tool-b", "function_call"),
    ];
    let actual = reorder(&joined(&frames), 13).await;
    let expected = [0, 6, 1, 3, 4, 2, 5, 7].map(|index| frames[index].clone());
    assert_eq!(actual, joined(&expected));
    assert_eq!(
        single_active_consumer(&actual),
        Ok("{\"a\":1}{\"b\":2}".to_owned())
    );
}

#[tokio::test]
async fn terminal_errors_and_eof_flush_pending_frames_without_fabricating_done() {
    let prefix = [
        item("added", 0, "reason", "reasoning"),
        item("added", 1, "message", "message"),
        delta(1, "message", "response.output_text.delta", "partial"),
    ];
    for terminal in [
        "response.failed",
        "response.incomplete",
        "response.cancelled",
        "error",
        "response.completed",
    ] {
        let mut input = joined(&prefix);
        input.extend(frame(
            json!({"type":terminal,"error":{"code":"original_error"},"future":[1,2]}),
        ));
        assert_eq!(reorder(&input, 5).await, input);
    }
    let mut input = joined(&prefix);
    assert_eq!(reorder(&input, 5).await, input);
    input.extend_from_slice(b"data: {\"unfinished\":");
    assert_eq!(reorder(&input, 5).await, input);
}

#[tokio::test]
async fn buffer_limits_release_all_original_frames_without_waiting_for_done() {
    for byte_limit in [false, true] {
        let mut frames = vec![
            item("added", 0, "reason", "reasoning"),
            item("added", 1, "message", "message"),
        ];
        if byte_limit {
            frames.push(delta(
                1,
                "message",
                "response.output_text.delta",
                &"x".repeat(1024 * 1024 + 1),
            ));
        } else {
            frames.extend((0..300).map(|_| delta(1, "message", "response.output_text.delta", "x")));
        }
        let original = joined(&frames);
        let input: CodexBackendSseStream = Box::pin(
            futures::stream::iter(vec![Ok(Bytes::copy_from_slice(&original))])
                .chain(futures::stream::pending()),
        );
        let mut stream = serialize_output_items(input);
        let mut actual = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while actual.len() < original.len() {
                actual.extend(stream.next().await.unwrap().unwrap());
            }
        })
        .await
        .expect("bounds must release pending frames before source completion");
        assert_eq!(actual, original);
    }
}

#[tokio::test]
async fn read_failure_keeps_pending_payload_and_the_original_error() {
    let prefix = joined(&[
        item("added", 0, "reason", "reasoning"),
        item("added", 1, "message", "message"),
        delta(1, "message", "response.output_text.delta", "partial"),
    ]);
    let input: CodexBackendSseStream = Box::pin(futures::stream::iter(vec![
        Ok(Bytes::copy_from_slice(&prefix)),
        Err(CodexClientError::StreamIdleTimeout {
            timeout: Duration::from_secs(42),
        }),
    ]));
    let mut stream = serialize_output_items(input);
    let mut actual = Vec::new();
    loop {
        match stream
            .next()
            .await
            .expect("original error must be returned")
        {
            Ok(bytes) => actual.extend(bytes),
            Err(error) => {
                assert!(
                    matches!(error, CodexClientError::StreamIdleTimeout { timeout } if timeout == Duration::from_secs(42))
                );
                break;
            }
        }
    }
    assert_eq!(actual, prefix);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn cancelling_buffered_stream_drops_the_upstream_without_a_background_task() {
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&dropped));
    let first = item("added", 0, "reason", "reasoning");
    let prefix = joined(&[first.clone(), item("added", 1, "message", "message")]);
    let input: CodexBackendSseStream = Box::pin(async_stream::stream! {
        let _guard = guard;
        yield Ok(Bytes::from(prefix));
        futures::future::pending::<()>().await;
    });
    let mut stream = serialize_output_items(input);
    assert_eq!(stream.next().await.unwrap().unwrap(), first);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), stream.next())
            .await
            .is_err()
    );
    drop(stream);
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn ambiguous_item_identity_falls_back_without_discarding_wire() {
    for ambiguity in [
        frame(
            json!({"type":"response.output_text.delta","item_id":"message","delta":"missing index"}),
        ),
        item("added", 1, "message", "message"),
        item("added", 0, "conflicting-id", "message"),
        item("added", 2, "message", "message"),
        Bytes::from_static(b"data: malformed-json\n\n"),
    ] {
        let original = joined(&[
            item("added", 0, "reason", "reasoning"),
            item("added", 1, "message", "message"),
            ambiguity,
            item("done", 0, "reason", "reasoning"),
            delta(1, "message", "response.output_text.delta", "kept"),
        ]);
        assert_eq!(reorder(&original, 7).await, original);
    }
}

#[tokio::test]
async fn oauth_http_transport_preserves_even_interleaved_upstream_frames() {
    use super::{CodexBackendClient, codex_request, request_context, test_wire_profile};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let original = joined(&[
        item("added", 0, "reason", "reasoning"),
        item("added", 1, "message", "message"),
        item("done", 0, "reason", "reasoning"),
        delta(1, "message", "response.output_text.delta", "unchanged"),
        item("done", 1, "message", "message"),
        frame(json!({"type":"response.completed"})),
    ]);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(original.clone(), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = CodexBackendClient::new(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        server.uri(),
        test_wire_profile(),
    );
    let mut request = codex_request("gpt-5.5", "", Vec::new());
    request.force_http_sse = true;
    let response = client
        .create_response_stream_with_pool_account(
            &request,
            request_context("oauth-ordering", Some("account")),
            None,
        )
        .await
        .expect("OAuth SSE");
    let actual = joined(&response.body.try_collect::<Vec<_>>().await.unwrap());
    assert_eq!(
        actual, original,
        "OAuth never enables the API-key ordering adapter"
    );
}
