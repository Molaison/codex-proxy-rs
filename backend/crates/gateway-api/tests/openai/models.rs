//! 模型列表、详情与客户端原生目录接口的投影及访问校验测试

use std::sync::{Arc, Mutex};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::AUTHORIZATION},
};
use bytes::Bytes;
use futures::future::BoxFuture;
use gateway_core::account::FastMode;
use gateway_core::engine::execution::{
    AuthenticatedClient, ClientAuthenticationError, ExecutionService, StartExecution,
    StartProviderExecution, StartedExecution,
};
use gateway_core::error::{GatewayError, GatewayErrorKind};
use gateway_core::routing::{
    ModelPresentation, ModelServiceTier, ProviderCatalogUnavailable, PublicModelDescriptor,
    PublicModelId, PublicModelProfile,
};
use tower::ServiceExt;

use super::{api_router, authenticated_client, authenticated_client_with_min_versions};

pub(super) struct ModelsExecution {
    client: AuthenticatedClient,
    presentation: Option<ModelPresentation>,
    native: Option<Result<Vec<PublicModelDescriptor>, ProviderCatalogUnavailable>>,
    requested_versions: Mutex<Vec<String>>,
    middleware: Option<Arc<crate::openai::middleware::RequestMiddleware>>,
}

impl ModelsExecution {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            client: authenticated_client("sk_models_test"),
            presentation: None,
            native: None,
            requested_versions: Mutex::default(),
            middleware: None,
        })
    }

    fn with_profiles() -> Arc<Self> {
        Self::with_presentation(
            ModelPresentation::new(
                Some("Grok 4.5".to_owned()),
                Some("xAI Grok 4.5 frontier model.".to_owned()),
            )
            .with_reasoning(
                Some("medium".to_owned()),
                ["low", "medium", "high", "xhigh"]
                    .map(str::to_owned)
                    .to_vec(),
            )
            .with_context_window_tokens(Some(500_000))
            .with_max_context_window_tokens(Some(900_000))
            .with_image_input(true)
            .with_agent_tools(true, true)
            .with_service_tiers(vec![
                ModelServiceTier::new(
                    "priority",
                    "Fast",
                    "Route the request through Codex fast mode.",
                )
                .with_speed_tier("fast"),
            ]),
        )
    }

    fn with_presentation(presentation: ModelPresentation) -> Arc<Self> {
        Arc::new(Self {
            client: authenticated_client("sk_models_test"),
            presentation: Some(presentation),
            native: None,
            requested_versions: Mutex::default(),
            middleware: None,
        })
    }

    fn with_cli_min() -> Arc<Self> {
        Arc::new(Self {
            client: authenticated_client_with_min_versions("sk_models_test", None, Some("0.40.0")),
            presentation: None,
            native: None,
            requested_versions: Mutex::default(),
            middleware: None,
        })
    }

    fn with_desktop_min() -> Arc<Self> {
        Arc::new(Self {
            client: authenticated_client_with_min_versions(
                "sk_models_test",
                Some("26.825.51511"),
                None,
            ),
            presentation: None,
            native: None,
            requested_versions: Mutex::default(),
            middleware: None,
        })
    }

    fn profiles(&self) -> Vec<PublicModelProfile> {
        self.presentation
            .as_ref()
            .map(|presentation| {
                PublicModelProfile::new(
                    PublicModelId::new("grok-4.5").expect("model"),
                    presentation.clone(),
                )
            })
            .into_iter()
            .collect()
    }
}

impl ExecutionService for ModelsExecution {
    fn middleware_plan(
        &self,
        _: &gateway_core::engine::execution::PreparedRootExecution,
    ) -> Option<gateway_core::engine::middleware::FrozenMiddlewarePlan> {
        self.middleware
            .as_ref()
            .map(|middleware| middleware.frozen())
    }

    fn client_model_catalog<'a>(
        &'a self,
        _client: &'a AuthenticatedClient,
        protocol: &'a str,
        version: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PublicModelDescriptor>, ProviderCatalogUnavailable>> {
        Box::pin(async move {
            assert_eq!(protocol, "codex");
            self.requested_versions
                .lock()
                .expect("versions")
                .push(version.to_owned());
            self.native.clone().unwrap_or_else(|| {
                Ok(self
                    .profiles()
                    .into_iter()
                    .map(PublicModelDescriptor::Adapted)
                    .collect())
            })
        })
    }

    fn authenticate(
        &self,
        plaintext: &str,
    ) -> Result<AuthenticatedClient, ClientAuthenticationError> {
        if plaintext == "sk_models_test" {
            Ok(self.client.clone())
        } else {
            Err(ClientAuthenticationError::InvalidKey)
        }
    }

    fn public_models(&self, client: &AuthenticatedClient) -> Vec<PublicModelId> {
        if let Some(Ok(native)) = &self.native {
            return native
                .iter()
                .map(|entry| match entry {
                    PublicModelDescriptor::Native { model, .. } => model.clone(),
                    PublicModelDescriptor::Adapted(profile) => profile.model().clone(),
                })
                .collect();
        }
        ["model-a", "model-b"]
            .into_iter()
            .map(|model| PublicModelId::new(client.snapshot().mapped_model(model)).expect("model"))
            .collect()
    }

    fn contains_public_model(&self, _: &AuthenticatedClient, model: &PublicModelId) -> bool {
        self.public_models(&self.client).contains(model)
    }

    fn start(&self, _: StartExecution) -> BoxFuture<'_, Result<StartedExecution, GatewayError>> {
        Box::pin(async {
            Err(GatewayError::new(
                GatewayErrorKind::Internal,
                "models test must not start a response",
            ))
        })
    }

    fn start_provider_endpoint(
        &self,
        _: StartProviderExecution,
    ) -> BoxFuture<'_, Result<StartedExecution, GatewayError>> {
        Box::pin(async {
            Err(GatewayError::new(
                GatewayErrorKind::Internal,
                "models test must not start a provider endpoint",
            ))
        })
    }
}

fn authorized_request(path: &str) -> Request<Body> {
    Request::get(path)
        .header(AUTHORIZATION, "Bearer sk_models_test")
        .body(Body::empty())
        .expect("build models request")
}

#[tokio::test]
async fn request_settings_reach_query_terminal_without_mutating_authenticated_baseline() {
    let mut execution = ModelsExecution::new();
    Arc::get_mut(&mut execution).unwrap().middleware =
        Some(Arc::new(crate::openai::middleware::RequestMiddleware {
            settings: Some(|settings| {
                let mut runtime = serde_json::to_value(&settings.runtime).unwrap();
                runtime["model_mappings"] = serde_json::json!({"model-a":"plugin-model"});
                runtime["concurrency_wait_timeout_seconds"] = serde_json::json!(30);
                settings.runtime = serde_json::from_value(runtime).unwrap();
                settings.fast_mode = FastMode::Disabled;
            }),
            ..Default::default()
        }));
    let app = api_router(execution.clone()).await;
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(authorized_request("/v1/models"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["data"][0]["id"], "plugin-model");
        assert_eq!(body["data"][1]["id"], "model-b");
        assert_eq!(
            execution.client.snapshot().mapped_model("model-a"),
            "model-a"
        );
        assert_ne!(
            execution.client.policy().account_scope().fast_mode(),
            FastMode::Disabled
        );
    }
}

#[tokio::test]
async fn model_queries_enter_the_same_chain_after_auth_and_hold_generation_until_body_end() {
    use std::sync::atomic::Ordering;

    let middleware = Arc::new(crate::openai::middleware::RequestMiddleware::default());
    let mut execution = ModelsExecution::new();
    Arc::get_mut(&mut execution).unwrap().middleware = Some(middleware.clone());
    let app = api_router(execution).await;
    let unauthenticated = Request::get("/v1/models").body(Body::empty()).unwrap();
    assert_eq!(
        app.clone().oneshot(unauthenticated).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(middleware.endpoints.lock().unwrap().is_empty());

    for (path, status) in [
        ("/v1/models", StatusCode::OK),
        ("/v1/models/model-a", StatusCode::OK),
        ("/v1/models/hidden-model", StatusCode::NOT_FOUND),
    ] {
        let response = app.clone().oneshot(authorized_request(path)).await.unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["x-request-middleware"], "applied");
        assert_eq!(middleware.live_leases.load(Ordering::SeqCst), 1);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!body.is_empty());
        assert_eq!(middleware.live_leases.load(Ordering::SeqCst), 0);
    }
    assert_eq!(
        *middleware.endpoints.lock().unwrap(),
        [
            "/v1/models",
            "/v1/models/model-a",
            "/v1/models/hidden-model"
        ]
    );
}

#[tokio::test]
async fn model_query_middleware_short_circuit_sets_json_content_type_without_calling_terminal() {
    let middleware = Arc::new(
        crate::openai::middleware::RequestMiddleware::default().with_json_short_circuit(
            Bytes::from_static(br#"{"object":"list","data":[],"source":"middleware"}"#),
        ),
    );
    let mut execution = ModelsExecution::new();
    Arc::get_mut(&mut execution).unwrap().middleware = Some(middleware);

    let response = api_router(execution.clone())
        .await
        .oneshot(authorized_request(
            "/v1/models?client_version=0.154.0-alpha.3",
        ))
        .await
        .expect("short-circuit response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("short-circuit body");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).expect("short-circuit JSON"),
        serde_json::json!({"object":"list","data":[],"source":"middleware"})
    );
    assert!(
        execution
            .requested_versions
            .lock()
            .expect("versions")
            .is_empty(),
        "terminal catalog lookup must not run after middleware short circuit"
    );
}

#[tokio::test]
async fn native_catalog_preserves_complete_objects_and_only_rewrites_alias_slug() {
    let original = serde_json::json!({
        "slug": "gpt-native", "display_name": "Official name", "priority": 47,
        "base_instructions": "Official instructions\nDo not replace.",
        "model_messages": {"token_budget": "upstream template", "future_section": [null, {"a": true}]},
        "service_tiers": [{"id": "priority", "name": "Fast", "description": "Official speed tier"}],
        "tool_mode": "code_mode_only", "use_responses_lite": true,
        "future_field": {"nested": [1, null, "value"]}, "explicit_null": null
    });
    let entry = |model: &str| PublicModelDescriptor::Native {
        model: PublicModelId::new(model).expect("model"),
        payload: gateway_core::operation::RawJsonPayload::new(
            "codex",
            serde_json::to_vec(&original).expect("JSON").into(),
        )
        .expect("payload"),
    };
    let execution = Arc::new(ModelsExecution {
        client: authenticated_client("sk_models_test"),
        presentation: None,
        native: Some(Ok(vec![entry("gpt-native"), entry("my-alias")])),
        requested_versions: Mutex::default(),
        middleware: None,
    });
    let response = api_router(execution.clone())
        .await
        .oneshot(authorized_request(
            "/v1/models?client_version=0.154.0-alpha.3",
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert!(response.headers().get("etag").is_none());
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(value["models"][0], original);
    let mut alias = original;
    alias["slug"] = serde_json::json!("my-alias");
    assert_eq!(value["models"][1], alias);
    assert_eq!(
        *execution.requested_versions.lock().expect("versions"),
        ["0.154.0-alpha.3"]
    );
}

#[tokio::test]
async fn native_catalog_failure_does_not_publish_a_synthetic_or_empty_success() {
    let execution = Arc::new(ModelsExecution {
        client: authenticated_client("sk_models_test"),
        presentation: None,
        native: Some(Err(ProviderCatalogUnavailable)),
        requested_versions: Mutex::default(),
        middleware: None,
    });
    let response = api_router(execution)
        .await
        .oneshot(authorized_request("/v1/models?client_version=0.154.0"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(value["error"]["code"], "model_catalog_unavailable");
    assert!(value.get("models").is_none());
}

#[tokio::test]
async fn catalog_rejects_unbounded_or_control_character_versions_before_fetching() {
    for version in [
        "x".repeat(65),
        "0.154.0%0A".to_owned(),
        "0.154.0%20".to_owned(),
    ] {
        let execution = ModelsExecution::new();
        let response = api_router(execution.clone())
            .await
            .oneshot(authorized_request(&format!(
                "/v1/models?client_version={version}"
            )))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            execution
                .requested_versions
                .lock()
                .expect("versions")
                .is_empty()
        );
    }
}

#[tokio::test]
async fn models_should_encode_the_service_visible_catalog() {
    let response = api_router(ModelsExecution::new())
        .await
        .oneshot(authorized_request("/v1/models"))
        .await
        .expect("list models response");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read models body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("models JSON");

    assert_eq!(
        value,
        serde_json::json!({
            "object": "list",
            "data": [
                {"id":"model-a","object":"model","created":1700000000_i64,"owned_by":"gateway"},
                {"id":"model-b","object":"model","created":1700000000_i64,"owned_by":"gateway"}
            ]
        })
    );
}

#[tokio::test]
async fn models_should_reject_recognized_cli_below_configured_min() {
    let request = Request::get("/v1/models")
        .header(AUTHORIZATION, "Bearer sk_models_test")
        .header("user-agent", "codex_cli_rs/0.39.0 (Linux; x86_64)")
        .body(Body::empty())
        .expect("build models request");
    let response = api_router(ModelsExecution::with_cli_min())
        .await
        .oneshot(request)
        .await
        .expect("list models response");

    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read version rejection body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("version rejection JSON");
    assert_eq!(value["error"]["code"], "client_version_too_old");
    assert_eq!(value["error"]["client"], "codex_cli");
    assert_eq!(value["error"]["current_version"], "0.39.0");
    assert_eq!(value["error"]["min_version"], "0.40.0");
}

#[tokio::test]
async fn models_should_reject_recognized_cli_with_invalid_version() {
    let request = Request::get("/v1/models")
        .header(AUTHORIZATION, "Bearer sk_models_test")
        .header("user-agent", "codex-cli/not-a-version")
        .body(Body::empty())
        .expect("build models request");
    let response = api_router(ModelsExecution::with_cli_min())
        .await
        .oneshot(request)
        .await
        .expect("list models response");

    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read version rejection body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("version rejection JSON");
    assert_eq!(value["error"]["code"], "client_version_unavailable");
    assert!(value["error"]["current_version"].is_null());
    assert_eq!(value["error"]["min_version"], "0.40.0");
}

#[tokio::test]
async fn models_should_reject_desktop_below_its_configured_min() {
    let request = Request::get("/v1/models")
        .header(AUTHORIZATION, "Bearer sk_models_test")
        .header("originator", "Codex Desktop")
        .header("version", "26.825.50000")
        .header("user-agent", "codex_cli_rs/99.0.0 (Codex Desktop)")
        .body(Body::empty())
        .expect("build models request");
    let response = api_router(ModelsExecution::with_desktop_min())
        .await
        .oneshot(request)
        .await
        .expect("list models response");

    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read version rejection body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("version rejection JSON");
    assert_eq!(value["error"]["code"], "client_version_too_old");
    assert_eq!(value["error"]["client"], "codex_desktop");
    assert_eq!(value["error"]["current_version"], "26.825.50000");
    assert_eq!(value["error"]["min_version"], "26.825.51511");
}

#[tokio::test]
async fn models_should_allow_cli_at_its_configured_min() {
    let request = Request::get("/v1/models")
        .header(AUTHORIZATION, "Bearer sk_models_test")
        .header("user-agent", "codex_cli_rs/0.40.0 (Linux; x86_64)")
        .body(Body::empty())
        .expect("build models request");
    let response = api_router(ModelsExecution::with_cli_min())
        .await
        .oneshot(request)
        .await
        .expect("list models response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn models_should_leave_unknown_clients_unrestricted() {
    let request = Request::get("/v1/models")
        .header(AUTHORIZATION, "Bearer sk_models_test")
        .header("user-agent", "curl/8.14.1")
        .body(Body::empty())
        .expect("build models request");
    let response = api_router(ModelsExecution::with_cli_min())
        .await
        .oneshot(request)
        .await
        .expect("list models response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn models_should_encode_provider_profiles_for_current_codex_clients() {
    let response = api_router(ModelsExecution::with_profiles())
        .await
        .oneshot(authorized_request("/v1/models?client_version=0.145.0"))
        .await
        .expect("list Codex models response");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read Codex models body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("models JSON");
    let model = &value["models"][0];

    assert_eq!(
        value
            .as_object()
            .map(|catalog| catalog.keys().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["models"])
    );
    assert_eq!(model["slug"], "grok-4.5");
    assert_eq!(model["default_reasoning_level"], "medium");
    assert_eq!(model["context_window"], 500_000);
    assert_eq!(model["max_context_window"], 900_000);
    assert_eq!(model["apply_patch_tool_type"], "freeform");
    assert_eq!(model["additional_speed_tiers"], serde_json::json!(["fast"]));
    assert_eq!(
        model["service_tiers"],
        serde_json::json!([{
            "id": "priority",
            "name": "Fast",
            "description": "Route the request through Codex fast mode."
        }])
    );
    assert_eq!(
        model["supported_reasoning_levels"],
        serde_json::json!([
            {
                "effort": "low",
                "description": "Fast responses with lighter reasoning"
            },
            {
                "effort": "medium",
                "description": "Balances speed and reasoning depth for everyday tasks"
            },
            {
                "effort": "high",
                "description": "Greater reasoning depth for complex problems"
            },
            {
                "effort": "xhigh",
                "description": "Extra high reasoning depth for complex problems"
            }
        ])
    );
}

#[tokio::test]
async fn models_should_preserve_unknown_context_windows() {
    for (context_window, max_context_window) in
        [(None, None), (Some(272_000), None), (None, Some(872_000))]
    {
        let presentation = ModelPresentation::default()
            .with_context_window_tokens(context_window)
            .with_max_context_window_tokens(max_context_window);
        let response = api_router(ModelsExecution::with_presentation(presentation))
            .await
            .oneshot(authorized_request("/v1/models?client_version=0.145.0"))
            .await
            .expect("list Codex models response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read Codex models body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("models JSON");
        let model = &value["models"][0];

        assert_eq!(model["context_window"], serde_json::json!(context_window));
        assert_eq!(
            model["max_context_window"],
            serde_json::json!(max_context_window)
        );
    }
}

#[tokio::test]
async fn models_should_keep_the_codex_contract_when_profiles_are_empty() {
    let response = api_router(ModelsExecution::new())
        .await
        .oneshot(authorized_request("/v1/models?client_version=0.145.0"))
        .await
        .expect("list empty Codex models response");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read empty Codex models body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("Codex models JSON");

    assert_eq!(value, serde_json::json!({ "models": [] }));
}

#[tokio::test]
async fn models_with_provider_profiles_should_keep_the_openai_list_contract() {
    let response = api_router(ModelsExecution::with_profiles())
        .await
        .oneshot(authorized_request("/v1/models"))
        .await
        .expect("list compatible models response");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read compatible models body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("models JSON");

    assert_eq!(
        value,
        serde_json::json!({
            "object": "list",
            "data": [
                {"id":"model-a","object":"model","created":1700000000_i64,"owned_by":"gateway"},
                {"id":"model-b","object":"model","created":1700000000_i64,"owned_by":"gateway"}
            ]
        })
    );
}

#[tokio::test]
async fn model_detail_should_keep_the_official_path_id_contract() {
    let response = api_router(ModelsExecution::new())
        .await
        .oneshot(authorized_request("/v1/models/model-a"))
        .await
        .expect("model detail response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn model_detail_should_hide_unknown_models() {
    let response = api_router(ModelsExecution::new())
        .await
        .oneshot(authorized_request("/v1/models/model-private"))
        .await
        .expect("unknown model response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn models_should_authenticate_before_applying_the_version_gate() {
    let response = api_router(ModelsExecution::with_cli_min())
        .await
        .oneshot(
            Request::get("/v1/models")
                .header("user-agent", "codex_cli_rs/0.1.0 (Linux; x86_64)")
                .body(Body::empty())
                .expect("build unauthenticated request"),
        )
        .await
        .expect("unauthenticated models response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

fn web_roster_execution(rows: Vec<serde_json::Value>) -> Arc<ModelsExecution> {
    Arc::new(ModelsExecution {
        client: authenticated_client("sk_models_test"),
        presentation: None,
        native: Some(Ok(rows
            .into_iter()
            .map(|row| PublicModelDescriptor::Native {
                model: PublicModelId::new(row["slug"].as_str().unwrap()).unwrap(),
                payload: gateway_core::operation::RawJsonPayload::new(
                    "codex",
                    serde_json::to_vec(&row).unwrap().into(),
                )
                .unwrap(),
            })
            .collect())),
        requested_versions: Mutex::default(),
        middleware: None,
    })
}

fn web_roster_row(slug: &str, efforts: &[&str], window: u64, compact: u64) -> serde_json::Value {
    serde_json::json!({
        "slug": slug, "visibility": "list", "priority": 7,
        "supported_reasoning_levels": efforts.iter().map(|effort| serde_json::json!({"effort": effort})).collect::<Vec<_>>(),
        "context_window": window, "max_context_window": window, "auto_compact_token_limit": compact,
        "base_instructions": slug, "apply_patch_tool_type": if slug.ends_with("-tools") { Some("freeform") } else { None },
        "future_field": {"preserved": true}
    })
}

async fn web_roster_catalog(execution: Arc<ModelsExecution>) -> serde_json::Value {
    let response = api_router(execution)
        .await
        .oneshot(authorized_request("/v1/models?client_version=0.162.1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn web_roster_catalog_has_ordered_modes_minimum_budgets_and_full_template() {
    let non_web = web_roster_row("native-model", &["high"], 400_000, 350_000);
    let mut other_non_web = web_roster_row("vendor/chatgpt-web/model", &["low"], 80_000, 60_000);
    other_non_web["visibility"] = serde_json::json!("hide");
    let mut rows = vec![non_web.clone()];
    for family in ["5.6", "6"] {
        for (backend, efforts, window, compact) in [
            ("sol", vec!["medium", "high", "xhigh"], 240_000, 220_000),
            ("pro", vec!["max"], 336_579, 285_000),
        ] {
            rows.push(web_roster_row(
                &format!("chatgpt-web/gpt-{family}-{backend}"),
                &efforts,
                111_193,
                95_000,
            ));
            rows.push(web_roster_row(
                &format!("chatgpt-web/gpt-{family}-{backend}-tools"),
                &efforts,
                window,
                compact,
            ));
        }
    }
    rows.push(web_roster_row(
        "chatgpt-web/gpt-6-sol-instant",
        &["none"],
        10_000,
        8_000,
    ));
    rows.push(web_roster_row(
        "chatgpt-web/gpt-6",
        &["high"],
        10_000,
        8_000,
    ));
    rows.push(other_non_web.clone());
    let value = web_roster_catalog(web_roster_execution(rows)).await;
    let models = value["models"].as_array().unwrap();
    assert_eq!(models[0], non_web);
    assert_eq!(models[1], other_non_web);
    assert_eq!(models.len(), 10);
    let mut index = 0;
    for family in ["5.6", "6"] {
        for mode in ["chat", "tools"] {
            for persistence in ["persist", "temp"] {
                let model = &models[index + 2];
                assert_eq!(
                    model["slug"],
                    format!("chatgpt-web/gpt-{family}-{mode}-{persistence}")
                );
                assert_eq!(model["priority"], index);
                let suffix = if mode == "tools" { "-tools" } else { "" };
                assert_eq!(
                    model["base_instructions"],
                    format!("chatgpt-web/gpt-{family}-sol{suffix}")
                );
                assert_eq!(
                    model["apply_patch_tool_type"],
                    serde_json::json!(if mode == "tools" {
                        Some("freeform")
                    } else {
                        None
                    })
                );
                assert_eq!(
                    model["future_field"],
                    serde_json::json!({"preserved": true})
                );
                let window = if mode == "tools" { 240_000 } else { 111_193 };
                assert_eq!(model["context_window"], window);
                assert_eq!(model["max_context_window"], window);
                assert_eq!(
                    model["auto_compact_token_limit"],
                    if mode == "tools" { 220_000 } else { 95_000 }
                );
                assert_eq!(model["default_reasoning_level"], "xhigh");
                assert_eq!(
                    model["supported_reasoning_levels"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| row["effort"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["high", "xhigh", "max"]
                );
                index += 1;
            }
        }
    }
}

#[tokio::test]
async fn web_roster_catalog_does_not_add_unauthorized_modes_or_unsupported_strengths() {
    let value = web_roster_catalog(web_roster_execution(vec![
        web_roster_row(
            "chatgpt-web/gpt-6-sol",
            &["medium", "high"],
            111_193,
            95_000,
        ),
        web_roster_row("chatgpt-web/gpt-6-pro", &["low"], 10_000, 8_000),
        web_roster_row("chatgpt-web/gpt-5.6-sol-tools", &["low"], 240_000, 220_000),
    ]))
    .await;
    let models = value["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    for (model, persistence) in models.iter().zip(["persist", "temp"]) {
        assert_eq!(
            model["slug"],
            format!("chatgpt-web/gpt-6-chat-{persistence}")
        );
        assert!(model["apply_patch_tool_type"].is_null());
        assert_eq!(model["default_reasoning_level"], "high");
        assert_eq!(model["context_window"], 111_193);
        assert_eq!(
            model["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["effort"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["high"]
        );
    }
}

#[tokio::test]
async fn web_roster_plain_list_and_detail_hide_old_ids_and_require_source_access() {
    let execution = web_roster_execution(vec![
        web_roster_row("chatgpt-web/gpt-6-sol", &["high"], 111_193, 95_000),
        web_roster_row(
            "chatgpt-web/gpt-5.6-tools-persist",
            &["high"],
            240_000,
            220_000,
        ),
        web_roster_row("chatgpt-web/gpt-6", &["high"], 111_193, 95_000),
    ]);
    let router = api_router(execution).await;
    let response = router
        .clone()
        .oneshot(authorized_request("/v1/models"))
        .await
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        value["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "chatgpt-web/gpt-6-chat-persist",
            "chatgpt-web/gpt-6-chat-temp"
        ]
    );
    for (id, status) in [
        ("chatgpt-web%2Fgpt-6-chat-persist", StatusCode::OK),
        ("chatgpt-web%2Fgpt-6-chat-temp", StatusCode::OK),
        ("chatgpt-web%2Fgpt-6-tools-persist", StatusCode::NOT_FOUND),
        ("chatgpt-web%2Fgpt-6-tools-temp", StatusCode::NOT_FOUND),
        ("chatgpt-web%2Fgpt-5.6-tools-persist", StatusCode::NOT_FOUND),
        ("chatgpt-web%2Fgpt-6", StatusCode::NOT_FOUND),
        ("chatgpt-web%2Fgpt-6-sol", StatusCode::NOT_FOUND),
        ("chatgpt-web%2Fgpt-5.6", StatusCode::NOT_FOUND),
    ] {
        let response = router
            .clone()
            .oneshot(authorized_request(&format!("/v1/models/{id}")))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{id}");
    }
}

#[tokio::test]
async fn web_roster_pro_tools_only_preserves_its_authorized_full_template() {
    let value = web_roster_catalog(web_roster_execution(vec![
        web_roster_row("chatgpt-web/gpt-6-sol", &["high"], 111_193, 95_000),
        web_roster_row("chatgpt-web/gpt-6-pro-tools", &["max"], 336_579, 285_000),
    ]))
    .await;
    let models = value["models"].as_array().unwrap();
    assert_eq!(models.len(), 4);
    for persistence in ["persist", "temp"] {
        let model = models
            .iter()
            .find(|row| row["slug"] == format!("chatgpt-web/gpt-6-tools-{persistence}"))
            .unwrap();
        assert_eq!(model["base_instructions"], "chatgpt-web/gpt-6-pro-tools");
        assert_eq!(model["apply_patch_tool_type"], "freeform");
        assert_eq!(model["context_window"], 336_579);
        assert_eq!(model["auto_compact_token_limit"], 285_000);
        assert_eq!(model["default_reasoning_level"], "max");
        assert_eq!(
            model["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["effort"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["max"]
        );
    }
}

#[tokio::test]
async fn web_roster_invalid_effort_returns_http_400_before_execution() {
    for (model, effort) in [
        ("chatgpt-web/gpt-6", "high"),
        ("chatgpt-web/gpt-6-tools-persist", "pro"),
        ("chatgpt-web/gpt-5.6-chat-temp", "medium"),
    ] {
        let response = api_router(ModelsExecution::new()).await.oneshot(
            Request::post("/v1/responses")
                .header(AUTHORIZATION, "Bearer sk_models_test")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"model": model, "reasoning": {"effort": effort}, "input": "hello"}).to_string())).unwrap()
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(value["error"]["param"], "reasoning.effort");
    }
}

#[tokio::test]
async fn web_roster_plain_catalog_preserves_non_web_order_and_mode_details() {
    let execution = web_roster_execution(vec![
        web_roster_row("native-model", &["high"], 400_000, 350_000),
        web_roster_row("chatgpt-web/gpt-5.6-pro-tools", &["max"], 336_579, 285_000),
        web_roster_row("vendor/chatgpt-web/model", &["low"], 80_000, 60_000),
        web_roster_row("chatgpt-web/gpt-6-sol", &["high", "xhigh"], 111_193, 95_000),
        web_roster_row("chatgpt-web/gpt-6-sol-instant", &["none"], 10_000, 8_000),
    ]);
    let router = api_router(execution).await;
    let response = router
        .clone()
        .oneshot(authorized_request("/v1/models"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let expected_ids = [
        "native-model",
        "vendor/chatgpt-web/model",
        "chatgpt-web/gpt-5.6-tools-persist",
        "chatgpt-web/gpt-5.6-tools-temp",
        "chatgpt-web/gpt-6-chat-persist",
        "chatgpt-web/gpt-6-chat-temp",
    ];
    assert_eq!(
        value,
        serde_json::json!({"object": "list", "data": expected_ids.map(|id| serde_json::json!({"id": id, "object": "model", "created": 1_700_000_000_i64, "owned_by": "gateway"}))})
    );
    for id in expected_ids {
        let response = router
            .clone()
            .oneshot(authorized_request(&format!(
                "/v1/models/{}",
                id.replace('/', "%2F")
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{id}");
    }
    for id in [
        "chatgpt-web/gpt-5.6",
        "chatgpt-web/gpt-6",
        "chatgpt-web/gpt-5.6-pro-tools",
        "chatgpt-web/gpt-6-sol",
        "chatgpt-web/gpt-6-sol-instant",
    ] {
        let response = router
            .clone()
            .oneshot(authorized_request(&format!(
                "/v1/models/{}",
                id.replace('/', "%2F")
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{id}");
    }
}
