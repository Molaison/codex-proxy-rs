//! 验证 Responses 请求解压、大小限制与原始字段保留

use std::io::{Read, Write, copy, repeat};

use axum::http::{HeaderMap, HeaderValue, header::CONTENT_ENCODING};
use gateway_api::openai::responses::{RequestDecodeError, decode_request_with_headers};
use serde_json::json;

use super::openai_wire_body;

const ENCODINGS: [&str; 3] = ["gzip", "deflate", "zstd"];
const REQUEST: &[u8] = br#"{"model":"gpt-test","input":"hello","future_field":{"value":1}}"#;
const DECOMPRESSED_LIMIT: u64 = 64 * 1024 * 1024;

pub(super) fn encode_body(encoding: &str, mut data: impl Read) -> Vec<u8> {
    match encoding {
        "gzip" => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            copy(&mut data, &mut encoder).expect("gzip encode");
            encoder.finish().expect("gzip finish")
        }
        "deflate" => {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            copy(&mut data, &mut encoder).expect("deflate encode");
            encoder.finish().expect("deflate finish")
        }
        "zstd" => zstd::stream::encode_all(data, 3).expect("zstd encode"),
        _ => panic!("unsupported fixture encoding"),
    }
}

fn headers(encoding: &str) -> HeaderMap {
    HeaderMap::from_iter([(
        CONTENT_ENCODING,
        HeaderValue::from_str(encoding).expect("encoding header"),
    )])
}

#[test]
fn http_decode_should_preserve_compressed_request_fields() {
    let expected = serde_json::from_slice::<serde_json::Value>(REQUEST).expect("fixture JSON");
    for encoding in ENCODINGS {
        let compressed = encode_body(encoding, REQUEST);
        let decoded =
            decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
                .expect(encoding);
        assert_eq!(
            openai_wire_body(&decoded),
            expected.as_object().expect("object")
        );
    }
}

#[test]
fn http_decode_should_accept_plain_identity_and_case_insensitive_encodings() {
    for request_headers in [HeaderMap::new(), headers(""), headers(" identity ")] {
        assert!(decode_request_with_headers(REQUEST, &request_headers, 64 * 1024 * 1024).is_ok());
    }
    let compressed = encode_body("gzip", REQUEST);
    assert!(decode_request_with_headers(&compressed, &headers(" GZip "), 64 * 1024 * 1024).is_ok());
}

#[test]
fn http_decode_should_reject_unknown_and_stacked_encodings() {
    let mut repeated = headers("identity");
    repeated.append(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    for request_headers in [headers("br"), headers("gzip, zstd"), repeated] {
        let error = decode_request_with_headers(REQUEST, &request_headers, 64 * 1024 * 1024)
            .expect_err("unsupported encoding");
        assert!(matches!(
            error,
            RequestDecodeError::UnsupportedContentEncoding { .. }
        ));
        assert_eq!(
            error.protocol_body().into_value()["error"]["code"],
            "unsupported_content_encoding"
        );
    }
}

#[test]
fn http_decode_should_reject_corrupted_and_truncated_compressed_bodies() {
    for encoding in ENCODINGS {
        let mut truncated = encode_body(encoding, REQUEST);
        truncated.truncate(truncated.len() - 4);
        for body in [b"not-a-compressed-stream".as_slice(), truncated.as_slice()] {
            let error = decode_request_with_headers(body, &headers(encoding), 64 * 1024 * 1024)
                .expect_err(encoding);
            assert_eq!(
                error.protocol_body().into_value(),
                json!({"error": {
                    "type": "invalid_request_error", "code": "invalid_json",
                    "message": "Request body must be valid JSON."
                }})
            );
        }
    }
}

#[test]
fn http_decode_should_read_all_gzip_members_and_zstd_frames() {
    for encoding in ["gzip", "zstd"] {
        let mut compressed = encode_body(encoding, &REQUEST[..12]);
        compressed.extend(encode_body(encoding, &REQUEST[12..]));
        let decoded =
            decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
                .expect(encoding);
        assert_eq!(openai_wire_body(&decoded)["input"], "hello");
    }
}

#[test]
fn http_decode_should_not_ignore_invalid_trailing_members() {
    for encoding in ["gzip", "zstd"] {
        let mut compressed = encode_body(encoding, REQUEST);
        compressed.extend(encode_body(encoding, b"invalid trailing JSON".as_slice()));
        assert_eq!(
            decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
                .expect_err(encoding),
            RequestDecodeError::MalformedJson
        );
    }
}

#[test]
fn http_decode_should_reject_zstd_frames_with_an_excessive_window() {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).expect("zstd encoder");
    encoder.window_log(27).expect("128 MiB window");
    encoder.write_all(REQUEST).expect("write JSON");
    // 提前输出帧头，避免编码器根据最终小正文缩减窗口
    encoder.flush().expect("flush frame header");
    let compressed = encoder.finish().expect("finish frame");
    assert_eq!(
        zstd::stream::decode_all(compressed.as_slice()).expect("valid zstd frame"),
        REQUEST
    );
    assert_eq!(
        decode_request_with_headers(&compressed, &headers("zstd"), 128 * 1024 * 1024)
            .expect_err("oversized window"),
        RequestDecodeError::MalformedJson
    );
}

#[test]
fn http_decode_should_accept_the_exact_decompressed_limit() {
    let padding = repeat(b' ').take(DECOMPRESSED_LIMIT - REQUEST.len() as u64);
    let compressed = encode_body("zstd", REQUEST.chain(padding));
    let decoded = decode_request_with_headers(&compressed, &headers("zstd"), 64 * 1024 * 1024)
        .expect("exact limit");
    assert_eq!(decoded.metadata().requested_model(), "gpt-test");
}

#[test]
fn http_decode_should_reject_bodies_above_the_decompressed_limit() {
    for encoding in ENCODINGS {
        let compressed = encode_body(encoding, repeat(0).take(DECOMPRESSED_LIMIT + 1));
        let error = decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
            .expect_err(encoding);
        assert_eq!(
            error,
            RequestDecodeError::DecompressedBodyTooLarge {
                limit_bytes: 64 * 1024 * 1024
            },
            "{encoding}"
        );
    }
}

#[test]
fn http_decode_should_stop_at_limit_before_reading_invalid_trailing_frames() {
    for encoding in ["zstd", "gzip"] {
        // 尾部故意损坏：若先完整解压再检查长度，就会错误地返回 invalid_json，
        // 这个断言验证越界时已经停止读取，而不只是最终返回了某个超限错误
        let mut compressed = encode_body(encoding, repeat(0).take(DECOMPRESSED_LIMIT + 1));
        compressed.extend_from_slice(b"invalid trailing frame");
        let error = decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
            .expect_err(encoding);
        assert_eq!(
            error,
            RequestDecodeError::DecompressedBodyTooLarge {
                limit_bytes: 64 * 1024 * 1024
            },
            "{encoding}"
        );
    }
}

#[test]
fn http_decode_should_apply_configured_limits_to_every_encoding_and_frame() {
    for encoding in ENCODINGS {
        let compressed = encode_body(encoding, REQUEST);
        assert!(
            decode_request_with_headers(&compressed, &headers(encoding), REQUEST.len()).is_ok()
        );
        let error = decode_request_with_headers(&compressed, &headers(encoding), REQUEST.len() - 1)
            .unwrap_err();
        assert_eq!(
            error,
            RequestDecodeError::DecompressedBodyTooLarge {
                limit_bytes: REQUEST.len() - 1
            }
        );
        assert!(
            error
                .protocol_body()
                .error
                .message
                .contains(&(REQUEST.len() - 1).to_string())
        );
    }
    for encoding in ["gzip", "zstd"] {
        let split = REQUEST.len() / 2;
        let mut compressed = encode_body(encoding, &REQUEST[..split]);
        compressed.extend(encode_body(encoding, &REQUEST[split..]));
        assert!(
            decode_request_with_headers(&compressed, &headers(encoding), REQUEST.len()).is_ok()
        );
        assert!(matches!(
            decode_request_with_headers(&compressed, &headers(encoding), REQUEST.len() - 1),
            Err(RequestDecodeError::DecompressedBodyTooLarge { .. })
        ));
    }
    for request_headers in [HeaderMap::new(), headers("identity")] {
        assert!(decode_request_with_headers(REQUEST, &request_headers, 1).is_ok());
    }
}

#[test]
fn web_roster_normalizes_all_modes_before_http_and_websocket_routing() {
    for family in ["5.6", "6"] {
        for (strength, backend, upstream) in [
            ("high", "sol", "high"),
            ("xhigh", "sol", "xhigh"),
            ("pro", "pro", "max"),
        ] {
            for (mode, suffix) in [("chat", ""), ("tools", "-tools")] {
                for (persistence, temporary) in [("persist", ""), ("temp", "-temporary")] {
                    let mut body = json!({
                        "model": format!("chatgpt-web/gpt-{family}"), "input": "hello",
                        "reasoning": {"effort": format!("{strength}-{mode}-{persistence}"), "summary": "auto"},
                        "previous_response_id": "resp_previous", "store": true,
                        "tools": [{"type": "function", "name": "read"}]
                    });
                    let mut alias = body.clone();
                    alias["model"] =
                        json!(format!("chatgpt-web/gpt-{family}-{mode}-{persistence}"));
                    alias["reasoning"]["effort"] = json!(upstream);
                    let alias_http = super::decode_request(alias.to_string().as_bytes()).unwrap();
                    alias["type"] = json!("response.create");
                    let alias_websocket =
                        super::decode_response_create(&alias.to_string()).unwrap();
                    let http = super::decode_request(body.to_string().as_bytes()).unwrap();
                    body["type"] = json!("response.create");
                    let websocket = super::decode_response_create(&body.to_string()).unwrap();
                    for decoded in [http, websocket, alias_http, alias_websocket] {
                        let expected_model = format!("chatgpt-web/gpt-{family}-{backend}{suffix}");
                        assert_eq!(decoded.metadata().requested_model(), expected_model);
                        let wire = openai_wire_body(&decoded);
                        assert_eq!(wire["model"], expected_model);
                        assert_eq!(
                            wire["reasoning"]["effort"],
                            format!("{upstream}{temporary}")
                        );
                        assert_eq!(wire["reasoning"]["summary"], "auto");
                        assert_eq!(wire["previous_response_id"], "resp_previous");
                        assert_eq!(wire["tools"], body["tools"]);
                        assert!(decoded.metadata().store());
                    }
                }
            }
        }
    }
}

#[test]
fn web_roster_defaults_and_invalid_efforts_are_explicit() {
    for reasoning in [None, Some(json!({"summary": "auto"}))] {
        let mut body = json!({"model": "chatgpt-web/gpt-6", "input": "hello"});
        if let Some(reasoning) = reasoning {
            body["reasoning"] = reasoning;
        }
        let decoded = super::decode_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(
            decoded.metadata().requested_model(),
            "chatgpt-web/gpt-6-sol-tools"
        );
        assert_eq!(openai_wire_body(&decoded)["reasoning"]["effort"], "xhigh");
    }
    for mode in ["chat", "tools"] {
        for persistence in ["persist", "temp"] {
            let model = format!("chatgpt-web/gpt-6-{mode}-{persistence}");
            let decoded = super::decode_request(
                json!({"model": model, "input": "hello"})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
            assert_eq!(
                decoded.metadata().requested_model(),
                format!(
                    "chatgpt-web/gpt-6-sol{}",
                    if mode == "tools" { "-tools" } else { "" }
                )
            );
            assert_eq!(
                openai_wire_body(&decoded)["reasoning"]["effort"],
                if persistence == "temp" {
                    "xhigh-temporary"
                } else {
                    "xhigh"
                }
            );
            for effort in [
                json!("medium"),
                json!("pro"),
                json!("xhigh-tools-persist"),
                json!(null),
                json!(42),
            ] {
                let mut body = json!({"model": model, "reasoning": {"effort": effort}});
                assert_eq!(
                    super::decode_request(body.to_string().as_bytes()).unwrap_err(),
                    RequestDecodeError::InvalidValue {
                        field: "reasoning.effort".to_owned()
                    }
                );
                body["type"] = json!("response.create");
                assert!(super::decode_response_create(&body.to_string()).is_err());
            }
            assert!(
                super::decode_request(
                    json!({"model": model, "reasoning": null})
                        .to_string()
                        .as_bytes()
                )
                .is_err()
            );
        }
    }
    for effort in [
        json!("high"),
        json!("low-chat-persist"),
        json!("max-tools-temp"),
        json!("pro-tools-temporary"),
        json!("xhigh-tools-persist-extra"),
        json!(null),
        json!(42),
    ] {
        let mut body = json!({"model": "chatgpt-web/gpt-6", "reasoning": {"effort": effort}});
        let error = super::decode_request(body.to_string().as_bytes()).unwrap_err();
        assert_eq!(
            error,
            RequestDecodeError::InvalidValue {
                field: "reasoning.effort".to_owned()
            }
        );
        body["type"] = json!("response.create");
        assert!(super::decode_response_create(&body.to_string()).is_err());
    }
}

#[test]
fn web_roster_preserves_old_models_and_their_efforts() {
    for model in [
        "chatgpt-web/gpt-6-sol",
        "chatgpt-web/gpt-5.6-pro-tools",
        "chatgpt-web/gpt-6-sol-instant",
        "gpt-6",
    ] {
        let body = json!({"model": model, "reasoning": {"effort": "future-effort"}, "input": "history", "previous_response_id": "resp_old"});
        let decoded = super::decode_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(openai_wire_body(&decoded), body.as_object().unwrap());
        assert_eq!(decoded.metadata().requested_model(), model);
    }
}

#[test]
fn web_roster_cannot_expand_the_keys_frozen_account_group_or_model_permissions() {
    use gateway_core::{
        account::{AccountModelAccess, AccountModelAccessMode, ProviderAccountId},
        routing::{
            AccountGroupId, ClientRoutingScope, FrozenAccountScope, ProviderKind, PublicModelId,
            RoutingContext, RoutingGroupSnapshot, RuntimeAccount, RuntimeAccountDirectory,
        },
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::Arc,
    };
    let provider = ProviderKind::new("openai").unwrap();
    let chat_group = AccountGroupId::new("grp_00000000000000000000000000000001").unwrap();
    let tools_group = AccountGroupId::new("grp_00000000000000000000000000000002").unwrap();
    let directory = Arc::new(RuntimeAccountDirectory::new(BTreeMap::from([
        (
            ProviderAccountId::new("acct_chat").unwrap(),
            RuntimeAccount::new(provider.clone(), BTreeSet::from([chat_group.clone()]))
                .with_model_access(
                    AccountModelAccess::new(
                        AccountModelAccessMode::Allowlist,
                        vec!["chatgpt-web/gpt-6-sol".to_owned()],
                    )
                    .unwrap(),
                ),
        ),
        (
            ProviderAccountId::new("acct_tools").unwrap(),
            RuntimeAccount::new(provider.clone(), BTreeSet::from([tools_group])),
        ),
    ])));
    let scope = Arc::new(FrozenAccountScope::new(
        Arc::clone(&directory),
        ClientRoutingScope::restricted(
            vec![RoutingGroupSnapshot::new(
                chat_group.clone(),
                "Chat only".to_owned(),
            )],
            BTreeSet::from([chat_group]),
            BTreeSet::from([provider.clone()]),
        )
        .unwrap(),
    ));
    let models = [
        "chatgpt-web/gpt-6-sol",
        "chatgpt-web/gpt-6-sol-tools",
        "chatgpt-web/gpt-6-pro",
        "chatgpt-web/gpt-6-pro-tools",
    ]
    .map(|model| {
        gateway_core::routing::ProviderModel::new(
            provider.clone(),
            gateway_core::routing::UpstreamModelId::new(model).unwrap(),
            gateway_core::routing::ModelCapabilities::new(
                BTreeSet::from([gateway_core::operation::OperationKind::Generate]),
                None,
            )
            .with_upstream_feature_validation(),
        )
    })
    .to_vec();
    let snapshot = gateway_core::routing::RuntimeSnapshot::new(
        gateway_core::routing::ConfigRevision::new(1).unwrap(),
        gateway_core::settings::SettingsValues::new(2, 1, "smart", Default::default(), None, None),
        vec![provider],
        models,
        vec![],
    )
    .unwrap()
    .with_account_directory(directory);
    for (effort, allowed) in [
        ("high-chat-persist", true),
        ("xhigh-chat-temp", true),
        ("pro-chat-persist", false),
        ("high-tools-persist", false),
        ("pro-tools-temp", false),
    ] {
        let parts = effort.split('-').collect::<Vec<_>>();
        for (model, effort) in [
            ("chatgpt-web/gpt-6".to_owned(), effort.to_owned()),
            (
                format!("chatgpt-web/gpt-6-{}-{}", parts[1], parts[2]),
                if parts[0] == "pro" { "max" } else { parts[0] }.to_owned(),
            ),
        ] {
            let decoded = super::decode_request(
                json!({"model": model, "reasoning": {"effort": effort}, "input": "hello"})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
            let result = snapshot.plan(
                &PublicModelId::new(decoded.metadata().requested_model()).unwrap(),
                decoded.operation(),
                Arc::clone(&scope),
                &RoutingContext::default(),
            );
            assert_eq!(result.is_ok(), allowed, "{model}/{effort}: {result:?}");
        }
    }
}
