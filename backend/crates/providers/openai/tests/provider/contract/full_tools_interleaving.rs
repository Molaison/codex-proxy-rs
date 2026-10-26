use super::*;

const FULL: &str = "chatgpt-web/gpt-5.6-sol-tools";
const NATIVE: &str = "gpt-5.6-luna";
const TOOL_CALL: &str = "call_full_original_owner";

#[tokio::test]
async fn native_full_native_full_keeps_tool_result_with_original_owner() {
    assert_interleaving(&[NATIVE, FULL, NATIVE, FULL]).await;
}

#[tokio::test]
async fn full_native_full_keeps_tool_result_with_original_owner() {
    assert_interleaving(&[FULL, NATIVE, FULL]).await;
}

async fn assert_interleaving(order: &[&str]) {
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_session_affinity").await;
    create_account(&store, "acct_local_affinity").await;
    let affinity = Arc::new(MemorySessionAffinity::default());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = captured_request_body(request);
            let continued = body["input"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["type"] == "function_call_output")
            });
            let output = if body["model"] == FULL && !continued {
                json!([{"type":"function_call","id":"fc_full_owner","call_id":TOOL_CALL,
                    "name":"read_file","arguments":"{}","status":"completed"}])
            } else {
                json!([])
            };
            let event = json!({"type":"response.completed","response":{
                "id":"resp_scope_capture","model":body["model"],"status":"completed",
                "output":output,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}});
            ResponseTemplate::new(200).set_body_raw(
                format!("event: response.completed\ndata: {event}\n\n"),
                "text/event-stream",
            )
        })
        .expect(u64::try_from(order.len()).unwrap())
        .mount(&server)
        .await;
    let provider = provider_with_affinity_and_base_url(&store, Arc::clone(&affinity), server.uri());
    let operation = |model: &str, input: Value| {
        Operation::Generate(GenerateRequest::from_protocol_payload(
            ProtocolPayload::json_object(
                "openai",
                Map::from_iter([
                    ("model".to_owned(), json!(model)),
                    ("input".to_owned(), input),
                    ("session_id".to_owned(), json!("shared-client-thread")),
                ]),
            )
            .unwrap()
            .with_context(Map::from_iter([("use_websocket".to_owned(), json!(false))])),
        ))
    };
    // 固定两条已存在绑定，再经过真实成功流验证普通请求的CAS不迁移Full owner。
    let key = ClientApiKeyId::new("key_openai_contract").unwrap();
    for (model, owner) in [
        (FULL, "acct_session_affinity"),
        (NATIVE, "acct_local_affinity"),
    ] {
        let lookup = provider
            .request_observation(&operation(model, json!("start")), &key)
            .continuation
            .affinity_hash
            .unwrap();
        affinity.seed_binding(
            &ProviderKind::new("openai").unwrap(),
            &lookup,
            ProviderAccountId::new(owner).unwrap(),
        );
    }
    let mut emitted_call = None;
    for (index, model) in order.iter().enumerate() {
        let input = if *model == FULL && emitted_call.is_some() {
            json!([
                {"role":"user","content":"read the file"},
                {"type":"function_call","id":"fc_full_owner","call_id":emitted_call,
                    "name":"read_file","arguments":"{}"},
                {"type":"function_call_output","call_id":emitted_call,"output":"read result"}
            ])
        } else {
            json!([{"role":"user","content":"start"}])
        };
        let mut stream = Arc::clone(&provider)
            .execute(
                planned_request_for_model("openai", operation(model, input), model),
                context(
                    &format!("req_interleaving_{index}"),
                    CancellationToken::new(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            stream.metadata().provider_account_id().as_str(),
            if *model == FULL {
                "acct_session_affinity"
            } else {
                "acct_local_affinity"
            }
        );
        let mut completed = false;
        while let Some(event) = stream.next().await {
            let event = event.unwrap();
            if let Some(wire) = event.wire_event()
                && wire.data()["type"] == "response.completed"
            {
                completed = true;
                if *model == FULL && emitted_call.is_none() {
                    emitted_call = Some(
                        wire.data()["response"]["output"][0]["call_id"]
                            .as_str()
                            .expect("original upstream tool call")
                            .to_owned(),
                    );
                }
            }
        }
        assert!(
            completed,
            "must consume successful completion to run affinity CAS"
        );
    }
    assert_eq!(emitted_call.as_deref(), Some(TOOL_CALL));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), order.len());
    let first_full = order.iter().position(|model| *model == FULL).unwrap();
    let last = requests.last().unwrap();
    assert_eq!(
        requests[first_full].headers["chatgpt-account-id"],
        last.headers["chatgpt-account-id"]
    );
    assert_eq!(
        last.headers["chatgpt-account-id"],
        "chatgpt-acct_session_affinity"
    );
    let body = captured_request_body(last);
    let tool_result = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(tool_result["call_id"], TOOL_CALL);
    assert_eq!(tool_result["output"], "read result");
    assert_eq!(affinity.binding_count(), 2);
    server.verify().await;
}
