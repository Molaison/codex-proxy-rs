use super::*;

fn numbered(mut value: Value, sequence: u64) -> Bytes {
    value["sequence_number"] = json!(sequence);
    frame(value)
}

fn added(index: u64, id: &str, sequence: u64) -> Bytes {
    numbered(
        json!({"type":"response.output_item.added","output_index":index,
        "item":{"id":id,"type":"message"}}),
        sequence,
    )
}

fn done(index: u64, id: &str, sequence: u64) -> Bytes {
    numbered(
        json!({"type":"response.output_item.done","output_index":index,
        "item":{"id":id,"type":"message"}}),
        sequence,
    )
}

fn values(bytes: &[u8]) -> Vec<Value> {
    SseEventDecoder::default()
        .push(bytes)
        .unwrap()
        .iter()
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect()
}

fn sequences(bytes: &[u8]) -> Vec<u64> {
    values(bytes)
        .iter()
        .map(|value| value["sequence_number"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn reordered_frames_reuse_source_sequence_slots_monotonically_without_overflow() {
    for slots in [
        [0, 1, 2, 3],
        [47, 49, 58, 99],
        [u64::MAX - 3, u64::MAX - 2, u64::MAX - 1, u64::MAX],
    ] {
        let original = joined(&[
            added(0, "a", slots[0]),
            added(1, "b", slots[1]),
            done(0, "a", slots[2]),
            done(1, "b", slots[3]),
        ]);
        let actual = reorder(&original, 5).await;
        assert_eq!(sequences(&actual), slots);
        let parsed = values(&actual);
        assert_eq!(
            parsed
                .iter()
                .map(|value| value["item"]["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "a", "b", "b"]
        );
    }
}

#[tokio::test]
async fn numbered_sequential_streams_keep_exact_original_bytes_and_gaps() {
    let original = joined(&[
        added(0, "a", 40),
        done(0, "a", 45),
        added(1, "b", 49),
        done(1, "b", 51),
    ]);
    for chunk_size in [1, 19, original.len()] {
        assert_eq!(reorder(&original, chunk_size).await, original);
    }
}

#[tokio::test]
async fn sequence_patch_preserves_multiline_sse_escaped_keys_and_nested_numbers() {
    let later = concat!(
        ": keep this comment\r\n",
        "id: original-91\r\nretry: 1234\r\nevent: response.output_item.added\r\n",
        "data: { \"type\":\"response.output_item.added\", \"output_index\":1,\r\n",
        "data: \"nested\":{\"sequence_number\":91,\"big\":90071992547409931234567890},\r\n",
        "data: \"text\":\"原文 \\\"sequence_number\\\":91\",\r\n",
        "data: \"\\u0073equence_number\" : 91, \"item\":{\"id\":\"b\",\"type\":\"message\"} }\r\n\r\n"
    );
    for separator in ["\r\n", "\n", "\r"] {
        let later = later.replace("\r\n", separator);
        let original = joined(&[
            added(0, "a", 90),
            Bytes::from(later.clone()),
            done(0, "a", 92),
            done(1, "b", 93),
        ]);
        let expected = joined(&[
            added(0, "a", 90),
            done(0, "a", 91),
            Bytes::from(later.replace(
                "\"\\u0073equence_number\" : 91",
                "\"\\u0073equence_number\" : 92",
            )),
            done(1, "b", 93),
        ]);
        let actual = reorder(&original, 1).await;
        if separator == "\r" {
            // 共享 decoder 仅识别 LF/CRLF；不可解析的 CR-only wire 必须原样保留。
            assert_eq!(actual, original);
            continue;
        }
        assert_eq!(actual, expected);
        assert_eq!(sequences(&actual), [90, 91, 92, 93]);
    }
}

#[tokio::test]
async fn fallback_after_actual_reordering_keeps_monotonic_numbers_and_all_payloads() {
    for terminal in [None, Some("response.failed"), Some("response.completed")] {
        let mut frames = vec![
            added(0, "a", 0),
            added(1, "b", 1),
            numbered(
                json!({"type":"response.reasoning.delta","item_id":"a","output_index":0,"delta":"kept"}),
                2,
            ),
        ];
        if let Some(terminal) = terminal {
            frames.push(numbered(
                json!({"type":terminal,"error":{"code":"original"}}),
                3,
            ));
        }
        let actual = reorder(&joined(&frames), 7).await;
        assert_eq!(
            sequences(&actual),
            (0..frames.len() as u64).collect::<Vec<_>>()
        );
        let parsed = values(&actual);
        assert_eq!(parsed[1]["delta"], "kept");
        assert_eq!(parsed[2]["item"]["id"], "b");
        assert!(
            !parsed
                .iter()
                .any(|value| value["type"] == "response.output_item.done")
        );
        if terminal.is_some() {
            assert_eq!(parsed[3]["error"]["code"], "original");
        }
    }
}

#[tokio::test]
async fn overflow_after_actual_reordering_keeps_sequence_slots_and_full_wire_content() {
    let mut frames = vec![
        added(0, "a", 0),
        added(1, "b", 1),
        numbered(
            json!({"type":"response.reasoning.delta","output_index":0,"item_id":"a","delta":"reason"}),
            2,
        ),
    ];
    frames.extend((3..270).map(|sequence| numbered(json!({
        "type":"response.output_text.delta","output_index":1,"item_id":"b","delta":format!("text-{sequence}")
    }), sequence)));
    frames.push(done(0, "a", 270));
    let actual = reorder(&joined(&frames), 31).await;
    assert_eq!(sequences(&actual), (0..=270).collect::<Vec<_>>());
    let parsed = values(&actual);
    let text = parsed
        .iter()
        .filter_map(|value| value.get("delta").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(text.len(), 268);
    assert_eq!(text[0], "reason");
    assert_eq!(text[267], "text-269");
}

#[tokio::test]
async fn ambiguous_or_invalid_sequence_fields_disable_reordering_without_rewriting_them() {
    for invalid in [
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"a\"},\"sequence_number\":2,\"sequence_number\":3}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"a\"},\"sequence_number\":\"2\"}\n\n",
    ] {
        let original = joined(&[
            added(0, "a", 0),
            added(1, "b", 1),
            Bytes::copy_from_slice(invalid.as_bytes()),
            done(1, "b", 4),
        ]);
        assert_eq!(reorder(&original, 7).await, original);
    }
}
