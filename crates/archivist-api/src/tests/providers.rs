//! AI provider resolution, model discovery, runtime hints and prompt tester tests.

use crate::test_support::*;
use crate::*;

#[test]
fn ollama_cloud_detection_matches_hosted_endpoint() {
    assert!(is_ollama_cloud("https://ollama.com"));
    assert!(is_ollama_cloud("https://OLLAMA.com/"));
    assert!(!is_ollama_cloud("http://ollama:11434"));
    assert!(!is_ollama_cloud("http://localhost:11434"));
}

#[test]
fn openai_model_filter_keeps_chat_drops_non_chat() {
    for keep in [
        "gpt-4o",
        "gpt-4o-mini",
        "gpt-5.5",
        "chatgpt-4o-latest",
        "o3",
        "o4-mini",
    ] {
        assert!(openai_id_is_chat_capable(keep), "should keep {keep}");
    }
    for drop in [
        "text-embedding-3-large",
        "whisper-1",
        "tts-1",
        "dall-e-3",
        "gpt-image-1",
        "gpt-4o-audio-preview",
        "omni-moderation-latest",
    ] {
        assert!(!openai_id_is_chat_capable(drop), "should drop {drop}");
    }
}

#[tokio::test]
async fn sglang_minimax_m3_is_confirmed_through_openai_compatible_models_endpoint() {
    use axum::Json as AxumJson;

    const MODEL: &str = "ressl/MiniMax-M3-uncensored-NVFP4";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new().route(
        "/v1/models",
        get(|| async { AxumJson(json!({ "data": [{ "id": MODEL }] })) }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    let provider = ApiProvider {
        name: "sglang-minimax-m3".to_owned(),
        kind: AiProviderKind::OpenaiCompatible,
        base_url: format!("http://{address}/v1"),
        model: MODEL.to_owned(),
        secret_id: None,
        tuning: RuntimeSettings::default().effective_tuning(),
    };

    let models = discover_provider_models(&provider, None)
        .await
        .expect("SGLang model discovery succeeds");

    assert_eq!(
        models
            .iter()
            .map(|model| model.name.as_str())
            .collect::<Vec<_>>(),
        vec![MODEL]
    );
    server.abort();
}

fn api_provider_profile_settings(first_url: &str, second_url: &str) -> RuntimeSettings {
    let mut settings = RuntimeSettings::default();
    settings.ai.default_provider = "first".to_owned();
    settings.ai.default_text_model = "gpt-5-first".to_owned();
    settings.ai.providers = vec![
        AiProviderSettings {
            name: "first".to_owned(),
            kind: AiProviderKind::OpenaiCompatible,
            base_url: first_url.to_owned(),
            default_text_model: Some("gpt-5-first".to_owned()),
            default_vision_model: None,
            cost_per_1m_input_tokens_usd: None,
            cost_per_1m_output_tokens_usd: None,
            secret_id: None,
            enabled: true,
            tuning: ProviderTuning {
                text_num_ctx: Some(11_111),
                reasoning_effort: Some(archivist_core::ReasoningEffort::Low),
                max_output_tokens: Some(111),
                structured_output: Some(archivist_core::StructuredOutputMode::Off),
                request_timeout_seconds: Some(11),
                ..ProviderTuning::default()
            },
        },
        AiProviderSettings {
            name: "second".to_owned(),
            kind: AiProviderKind::OpenaiCompatible,
            base_url: second_url.to_owned(),
            default_text_model: Some("gpt-5-second".to_owned()),
            default_vision_model: None,
            cost_per_1m_input_tokens_usd: None,
            cost_per_1m_output_tokens_usd: None,
            secret_id: None,
            enabled: true,
            tuning: ProviderTuning {
                text_num_ctx: Some(22_222),
                reasoning_effort: Some(archivist_core::ReasoningEffort::High),
                max_output_tokens: Some(222),
                structured_output: Some(archivist_core::StructuredOutputMode::JsonObject),
                request_timeout_seconds: Some(22),
                ..ProviderTuning::default()
            },
        },
    ];
    settings
}

fn api_test_chat_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_owned(),
        system_prompt: "system".to_owned(),
        user_prompt: "user".to_owned(),
        temperature: 0.1,
        num_ctx: None,
        response_schema: Some(json!({ "type": "object" })),
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    }
}

fn metadata_prompt_test_settings() -> RuntimeSettings {
    let mut settings = RuntimeSettings::default();
    settings.workflow.enabled_stages = vec![Stage::Metadata];
    settings.tagging.tag_output_language = "de".to_owned();
    settings.fields.max_fields = 1;
    settings.fields.mappings = vec![archivist_core::CustomFieldMapping {
        field_name: "HiddenField".to_owned(),
        enabled: false,
        aliases: Vec::new(),
        instructions: None,
    }];
    settings.ai.providers[0].tuning.allowed_list_max = Some(2);
    settings.ai.providers[0].tuning.max_tags = Some(3);
    settings
}

fn metadata_prompt_test_catalog() -> MetadataPromptTestCatalog {
    MetadataPromptTestCatalog {
        correspondents: vec![
            "Acme AG".to_owned(),
            "Beta GmbH".to_owned(),
            "Gamma AG".to_owned(),
        ],
        document_types: vec![
            "Invoice".to_owned(),
            "Letter".to_owned(),
            "Receipt".to_owned(),
        ],
        tags: vec!["Finance".to_owned(), "Tax".to_owned(), "Urgent".to_owned()],
        fields: vec![
            ("InvoiceNumber".to_owned(), Some("string".to_owned())),
            ("HiddenField".to_owned(), Some("integer".to_owned())),
        ],
    }
}

#[test]
fn metadata_prompt_test_request_matches_worker_prompt_schema_and_runtime_catalog() {
    let settings = metadata_prompt_test_settings();
    let tuning = settings.effective_tuning();
    let request = build_metadata_prompt_test_chat_request(
        &settings,
        &tuning,
        "Rechnung für Beratung und Entwicklung von Acme AG. Der Betrag ist mit Datum fällig. Invoice Tax Urgent Rechnungsnummer 41",
        metadata_prompt_test_catalog(),
    )
    .expect("metadata prompt request");

    assert!(
        request
            .user_prompt
            .contains("Detected document language: de")
    );
    assert!(
        request
            .user_prompt
            .contains("Desired language for newly generated business tags: de")
    );
    assert!(request.user_prompt.contains("Acme AG"));
    assert!(!request.user_prompt.contains("Beta GmbH"));
    assert!(!request.user_prompt.contains("Gamma AG"));
    assert!(request.user_prompt.contains("Invoice"));
    assert!(!request.user_prompt.contains("Receipt"));
    assert!(request.user_prompt.contains("Tax"));
    assert!(request.user_prompt.contains("Urgent"));
    assert!(!request.user_prompt.contains("Finance"));
    assert!(request.user_prompt.contains("\"InvoiceNumber\" (text)"));
    assert!(!request.user_prompt.contains("HiddenField"));
    assert!(request.user_prompt.contains("at most 3 tags"));
    assert!(request.user_prompt.contains("at most 1 entries"));

    let schema = request.response_schema.expect("metadata response schema");
    assert_eq!(
        schema["properties"]["correspondent"]["properties"]["name"]["enum"],
        json!(["Acme AG"])
    );
    assert_eq!(
        schema["properties"]["tags"]["properties"]["tags"]["items"]["enum"],
        json!(["Tax", "Urgent"])
    );
    assert_eq!(
        schema["properties"]["fields"]["properties"]["fields"]["items"]["properties"]["name"]["enum"],
        json!(["InvoiceNumber"])
    );
    assert_eq!(
        schema["properties"]["tags"]["properties"]["tags"]["maxItems"],
        3
    );
    assert_eq!(
        schema["properties"]["fields"]["properties"]["fields"]["maxItems"],
        1
    );
}

#[test]
fn metadata_prompt_test_editor_content_replaces_only_system_prompt() {
    let settings = metadata_prompt_test_settings();
    let mut request = build_metadata_prompt_test_chat_request(
        &settings,
        &settings.effective_tuning(),
        "Acme AG Invoice",
        metadata_prompt_test_catalog(),
    )
    .unwrap();
    let original_user = request.user_prompt.clone();
    let original_schema = request.response_schema.clone();

    apply_prompt_test_system_prompt(&mut request, "  operator system prompt  ");

    assert_eq!(request.system_prompt, "operator system prompt");
    assert_eq!(request.user_prompt, original_user);
    assert_eq!(request.response_schema, original_schema);
}

#[test]
fn metadata_prompt_test_parser_returns_typed_valid_and_partial_results() {
    let valid = parse_prompt_test_output(
        Stage::Metadata,
        r#"{"title":{"title":"Invoice 41","confidence":0.98},"document_date":{"date":"2026-07-17","confidence":0.9,"warnings":["date inferred"]}}"#,
    );
    assert!(valid.validation_errors.is_empty());
    assert_eq!(valid.parsed["suggestion"]["title"]["title"], "Invoice 41");
    assert_eq!(valid.parsed["diagnostics"]["status"], "valid");
    assert_eq!(valid.warnings, vec!["date inferred"]);

    let partial = parse_prompt_test_output(
        Stage::Metadata,
        r#"{"title":{"title":"Retained","confidence":0.8},"tags":"wrong","extra":"redacted"}"#,
    );
    assert_eq!(partial.parsed["suggestion"]["title"]["title"], "Retained");
    assert!(partial.parsed["suggestion"].get("tags").is_none());
    assert_eq!(
        partial.parsed["diagnostics"]["status"],
        "contract_violation"
    );
    assert_eq!(
        partial.validation_errors,
        vec![
            "metadata field(s) have wrong types or unknown nested properties: tags",
            "metadata response contains 1 unknown field(s)",
        ]
    );
}

#[test]
fn metadata_prompt_test_parser_rejects_malformed_non_object_and_omitted_outputs() {
    let malformed = parse_prompt_test_output(Stage::Metadata, "not json");
    assert_eq!(
        malformed.validation_errors,
        vec!["metadata response envelope is not valid JSON"]
    );
    assert_eq!(malformed.parsed["diagnostics"]["envelope_error"], "no_json");

    let non_object = parse_prompt_test_output(Stage::Metadata, "[1, 2]");
    assert_eq!(
        non_object.validation_errors,
        vec!["metadata response must be a JSON object"]
    );
    assert_eq!(
        non_object.parsed["diagnostics"]["envelope_error"],
        "non_object"
    );

    let omitted = parse_prompt_test_output(Stage::Metadata, "{}");
    assert!(omitted.validation_errors.is_empty());
    assert_eq!(
        omitted.warnings,
        vec!["metadata response omitted every requested field"]
    );
    assert_eq!(omitted.parsed["diagnostics"]["status"], "omitted");
}

#[test]
fn api_provider_tuning_follows_selected_provider_without_cross_profile_leakage() {
    let settings = api_provider_profile_settings(
        "https://first.example.test/v1",
        "https://second.example.test/v1",
    );
    let first = provider_by_name(&settings, "first").unwrap();
    let mut second = provider_by_name(&settings, "second").unwrap();
    second.model = "gpt-5-second-override".to_owned();

    let mut first_request = api_test_chat_request(&first.model);
    apply_api_provider_tuning(&first, &mut first_request);
    let mut second_request = api_test_chat_request(&second.model);
    apply_api_provider_tuning(&second, &mut second_request);

    assert_eq!(first_request.model, "gpt-5-first");
    assert_eq!(first_request.num_ctx, Some(11_111));
    assert_eq!(
        first_request.reasoning_effort,
        Some(archivist_core::ReasoningEffort::Low)
    );
    assert_eq!(first_request.max_output_tokens, Some(111));
    assert_eq!(
        first_request.structured_output,
        Some(archivist_core::StructuredOutputMode::Off)
    );
    assert_eq!(first.tuning.request_timeout_seconds, 11);

    assert_eq!(second_request.model, "gpt-5-second-override");
    assert_eq!(second_request.num_ctx, Some(22_222));
    assert_eq!(
        second_request.reasoning_effort,
        Some(archivist_core::ReasoningEffort::High)
    );
    assert_eq!(second_request.max_output_tokens, Some(222));
    assert_eq!(
        second_request.structured_output,
        Some(archivist_core::StructuredOutputMode::JsonObject)
    );
    assert_eq!(second.tuning.request_timeout_seconds, 22);
}

#[test]
fn prompt_tester_defaults_to_metadata_stage_provider_but_keeps_explicit_overrides() {
    let mut settings = api_provider_profile_settings(
        "https://first.example.test/v1",
        "https://second.example.test/v1",
    );
    settings.ai.stage_models = vec![archivist_core::StageModelOverride {
        stage: Stage::Metadata,
        provider: "second".to_owned(),
        model: "ressl/MiniMax-M3-uncensored-NVFP4".to_owned(),
    }];
    let mut request = TestPromptRequest {
        stage: Stage::Metadata,
        content: "metadata system".to_owned(),
        sample_text: Some("sample".to_owned()),
        paperless_document_id: None,
        provider_name: None,
        model: None,
    };

    let stage_provider = prompt_test_provider(&settings, &request).unwrap();
    assert_eq!(stage_provider.name, "second");
    assert_eq!(stage_provider.model, "ressl/MiniMax-M3-uncensored-NVFP4");
    assert_eq!(stage_provider.tuning.text_num_ctx, Some(22_222));
    assert_eq!(stage_provider.tuning.max_output_tokens, Some(222));

    request.provider_name = Some("first".to_owned());
    request.model = Some("explicit-model".to_owned());
    let explicit_provider = prompt_test_provider(&settings, &request).unwrap();
    assert_eq!(explicit_provider.name, "first");
    assert_eq!(explicit_provider.model, "explicit-model");
    assert_eq!(explicit_provider.tuning.text_num_ctx, Some(11_111));
    assert_eq!(explicit_provider.tuning.max_output_tokens, Some(111));

    settings.ai.providers[1].kind = AiProviderKind::Mineru;
    settings
        .ai
        .stage_models
        .push(archivist_core::StageModelOverride {
            stage: Stage::Ocr,
            provider: "second".to_owned(),
            model: "mineru".to_owned(),
        });
    request.stage = Stage::Ocr;
    request.provider_name = None;
    request.model = None;
    let ocr_text_provider = prompt_test_provider(&settings, &request).unwrap();
    assert_eq!(ocr_text_provider.name, "first");
    assert_eq!(ocr_text_provider.kind, AiProviderKind::OpenaiCompatible);
    assert_eq!(ocr_text_provider.model, "gpt-5-first");
}

#[test]
fn document_chat_request_uses_default_text_provider_tuning() {
    let settings = api_provider_profile_settings(
        "https://first.example.test/v1",
        "https://second.example.test/v1",
    );
    let provider = provider_for_default_text(&settings).unwrap();
    let request =
        build_document_chat_request(&provider, "chat system".to_owned(), "chat user".to_owned());

    assert_eq!(request.model, "gpt-5-first");
    assert_eq!(request.temperature, 0.1);
    assert_eq!(request.num_ctx, Some(11_111));
    assert_eq!(
        request.reasoning_effort,
        Some(archivist_core::ReasoningEffort::Low)
    );
    assert_eq!(request.max_output_tokens, Some(111));
    assert_eq!(
        request.structured_output,
        Some(archivist_core::StructuredOutputMode::Off)
    );
}

#[test]
fn runtime_hints_non_ollama_returns_stub_with_provider_specific_hint() {
    for (kind, expected_fragment) in [
        (AiProviderKind::Openai, "openai-specific"),
        (AiProviderKind::Anthropic, "anthropic-specific"),
        (
            AiProviderKind::OpenaiCompatible,
            "openai_compatible-specific",
        ),
    ] {
        let provider = make_api_provider(kind.clone());
        let response = non_ollama_runtime_hints(&provider);
        assert_eq!(response.provider, provider.name);
        assert!(response.reachable);
        assert!(response.version.is_none());
        assert!(response.loaded_models.is_empty());
        assert!(response.num_parallel.is_none());
        assert!(response.max_loaded_models.is_none());
        assert!(response.keep_alive.is_none());
        let hint = response
            .hint
            .as_deref()
            .expect("non-ollama hint must be populated");
        assert!(
            hint.contains(expected_fragment),
            "hint for {kind:?} must mention '{expected_fragment}', got {hint:?}"
        );
    }
}

async fn spawn_mock_ollama(
    version_response: Option<Value>,
    ps_response: Option<Value>,
) -> (String, tokio::task::JoinHandle<()>) {
    use axum::Json as AxumJson;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let version_handler = {
        let version_response = version_response.clone();
        move || async move {
            match version_response {
                Some(body) => AxumJson(body).into_response(),
                None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
    };
    let ps_handler = {
        let ps_response = ps_response.clone();
        move || async move {
            match ps_response {
                Some(body) => AxumJson(body).into_response(),
                None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
    };
    let router = Router::new()
        .route("/api/version", get(version_handler))
        .route("/api/ps", get(ps_handler));
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn runtime_hints_ollama_happy_path_collects_version_and_loaded_models() {
    let (base_url, handle) = spawn_mock_ollama(
        Some(serde_json::json!({ "version": "0.5.7" })),
        Some(serde_json::json!({
            "models": [
                {
                    "name": "qwen3-paperless:8b",
                    "size_vram": 6_396_411_904u64,
                    "expires_at": "2026-05-17T12:00:00Z"
                }
            ]
        })),
    )
    .await;
    let client = OllamaClient::new_with_timeout(
        "ollama",
        &base_url,
        None,
        std::time::Duration::from_secs(2),
    )
    .expect("client builds");
    let response = fetch_ollama_runtime_hints_with_client("ollama", &client).await;
    assert!(response.reachable, "Ollama mock should be reachable");
    assert_eq!(response.provider, "ollama");
    assert_eq!(response.version.as_deref(), Some("0.5.7"));
    assert_eq!(response.loaded_models.len(), 1);
    let model = &response.loaded_models[0];
    assert_eq!(model.name, "qwen3-paperless:8b");
    assert_eq!(model.size_vram_bytes, Some(6_396_411_904));
    assert!(response.num_parallel.is_none());
    assert!(response.max_loaded_models.is_none());
    assert!(response.keep_alive.is_none());
    let hint = response.hint.as_deref().expect("ollama hint string");
    assert!(
        hint.contains("NUM_PARALLEL"),
        "happy-path hint must explain the env-only knobs, got {hint:?}"
    );
    handle.abort();
}

#[tokio::test]
async fn runtime_hints_ollama_unreachable_falls_back_with_error_hint() {
    // Point the client at a port nothing listens on — the version
    // probe must fail fast and surface `reachable: false`.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = listener.local_addr().unwrap();
    drop(listener); // close the socket so the next connect refuses

    let client = OllamaClient::new_with_timeout(
        "ollama",
        &format!("http://{dead_addr}"),
        None,
        std::time::Duration::from_millis(500),
    )
    .expect("client builds");
    let response = fetch_ollama_runtime_hints_with_client("ollama", &client).await;
    assert!(!response.reachable);
    assert!(response.version.is_none());
    assert!(response.loaded_models.is_empty());
    let hint = response.hint.as_deref().expect("hint populated");
    assert!(
        hint.contains("Ollama unreachable"),
        "unreachable hint should explain the failure, got {hint:?}"
    );
}

#[tokio::test]
async fn ollama_chat_stamps_configured_provider_name_not_kind() {
    // Regression: the OllamaClient used to hardcode provider = "ollama", so
    // two ollama-kind providers (local "ollama" vs "ollama-cloud") collapsed
    // into one label in usage metrics. It must now stamp the configured name.
    use axum::Json as AxumJson;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new().route(
        "/api/chat",
        post(|| async { AxumJson(serde_json::json!({ "message": { "content": "ok" } })) }),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    let client =
        OllamaClient::new("ollama-cloud", &format!("http://{addr}"), None).expect("client builds");
    let response = client
        .chat(ChatRequest {
            model: "glm-5.1".to_owned(),
            system_prompt: "s".to_owned(),
            user_prompt: "u".to_owned(),
            temperature: 0.0,
            num_ctx: None,
            response_schema: None,
            reasoning_effort: None,
            max_output_tokens: None,
            structured_output: None,
        })
        .await
        .expect("chat succeeds");

    assert_eq!(
        response.provider, "ollama-cloud",
        "metric must carry the configured provider name, not the hardcoded kind"
    );
    assert_eq!(response.model, "glm-5.1");
    handle.abort();
}

#[derive(Clone, Default)]
struct ProviderProbeCapture {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    authorization: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    body: std::sync::Arc<std::sync::Mutex<Option<Value>>>,
}

async fn spawn_mock_openai_probe(
    capture: ProviderProbeCapture,
) -> (String, tokio::task::JoinHandle<()>) {
    use axum::Json as AxumJson;
    use axum::extract::State as AxumState;
    use axum::http::HeaderMap;
    use std::sync::atomic::Ordering;

    async fn probe(
        AxumState(capture): AxumState<ProviderProbeCapture>,
        headers: HeaderMap,
        AxumJson(body): AxumJson<Value>,
    ) -> AxumJson<Value> {
        capture.calls.fetch_add(1, Ordering::SeqCst);
        *capture.authorization.lock().unwrap() = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        *capture.body.lock().unwrap() = Some(body);
        AxumJson(json!({
            "id": "draft-probe",
            "model": "gpt-5-draft",
            "choices": [{ "message": { "content": "{\"status\":\"ok\"}" } }]
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/chat/completions", post(probe))
        .with_state(capture);
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    (format!("http://{address}"), handle)
}

#[tokio::test]
async fn prompt_tester_and_document_chat_send_selected_provider_tuning_on_wire() {
    let prompt_capture = ProviderProbeCapture::default();
    let document_capture = ProviderProbeCapture::default();
    let (prompt_url, prompt_handle) = spawn_mock_openai_probe(prompt_capture.clone()).await;
    let (document_url, document_handle) = spawn_mock_openai_probe(document_capture.clone()).await;
    let state = api_text_test_state();

    let settings = api_provider_profile_settings(&document_url, &prompt_url);
    let mut prompt_provider = provider_by_name(&settings, "second").unwrap();
    prompt_provider.model = "gpt-5-prompt-override".to_owned();
    let prompt_input = TestPromptRequest {
        stage: Stage::Ocr,
        content: "prompt system".to_owned(),
        sample_text: Some("sample".to_owned()),
        paperless_document_id: None,
        provider_name: Some("second".to_owned()),
        model: Some(prompt_provider.model.clone()),
    };
    let mut prompt_request = build_ocr_prompt_test_chat_request("sample");
    apply_prompt_test_system_prompt(&mut prompt_request, &prompt_input.content);
    prompt_request.model = prompt_provider.model.clone();
    apply_api_provider_tuning(&prompt_provider, &mut prompt_request);
    chat_with_api_provider(&state, &prompt_provider, prompt_request)
        .await
        .expect("prompt tester wire call");

    let prompt_body = prompt_capture.body.lock().unwrap().clone().unwrap();
    assert_eq!(prompt_body["model"], "gpt-5-prompt-override");
    assert_eq!(prompt_body["reasoning_effort"], "high");
    assert_eq!(prompt_body["max_tokens"], 222);

    let document_provider = provider_for_default_text(&settings).unwrap();
    let document_request = build_document_chat_request(
        &document_provider,
        "document system".to_owned(),
        "document user".to_owned(),
    );
    chat_with_api_provider(&state, &document_provider, document_request)
        .await
        .expect("document chat wire call");

    let document_body = document_capture.body.lock().unwrap().clone().unwrap();
    assert_eq!(document_body["model"], "gpt-5-first");
    assert_eq!(document_body["reasoning_effort"], "low");
    assert_eq!(document_body["max_tokens"], 111);

    prompt_handle.abort();
    document_handle.abort();
}

#[derive(Default)]
struct MixedM3Capture {
    active: std::sync::atomic::AtomicUsize,
    max_active: std::sync::atomic::AtomicUsize,
    bodies: Mutex<Vec<Value>>,
}

async fn mixed_m3_handler(
    State(capture): State<Arc<MixedM3Capture>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    use std::sync::atomic::Ordering;

    let active = capture.active.fetch_add(1, Ordering::AcqRel) + 1;
    capture.max_active.fetch_max(active, Ordering::AcqRel);
    capture.bodies.lock().unwrap().push(body.clone());
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    capture.active.fetch_sub(1, Ordering::AcqRel);
    let content = if body.get("response_format").is_some() {
        r#"{"title":{"title":"Synthetic capacity document","confidence":1.0}}"#
    } else {
        "ARCHIVIST_CAPACITY_CHAT_OK"
    };
    Json(json!({ "choices": [{ "message": { "content": content } }] }))
}

#[tokio::test]
async fn worker_metadata_and_document_chat_m3_paths_share_endpoint_concurrently() {
    use std::sync::atomic::Ordering;

    let capture = Arc::new(MixedM3Capture::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/chat/completions", post(mixed_m3_handler))
        .with_state(capture.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    let mut settings = RuntimeSettings::default();
    settings.workflow.enabled_stages = vec![Stage::Metadata];
    settings.ai.default_provider = archivist_core::SGLANG_MINIMAX_M3_PROVIDER_NAME.to_owned();
    settings.ai.default_text_model = archivist_core::MINIMAX_M3_MODEL.to_owned();
    for provider in &mut settings.ai.providers {
        provider.enabled = false;
    }
    let m3 = settings
        .ai
        .providers
        .iter_mut()
        .find(|provider| provider.name == archivist_core::SGLANG_MINIMAX_M3_PROVIDER_NAME)
        .expect("built-in MiniMax M3 provider");
    m3.enabled = true;
    m3.base_url = format!("http://{address}");

    let state = api_text_test_state();
    let provider = provider_for_default_text(&settings).expect("M3 API provider");
    let mut metadata_request = build_metadata_prompt_test_chat_request(
        &settings,
        &provider.tuning,
        "SYNTHETIC-ONLY capacity document dated 2026-01-02.",
        MetadataPromptTestCatalog {
            correspondents: Vec::new(),
            document_types: Vec::new(),
            tags: Vec::new(),
            fields: Vec::new(),
        },
    )
    .expect("Worker-equivalent Metadata request");
    metadata_request.model = provider.model.clone();
    apply_api_provider_tuning(&provider, &mut metadata_request);
    let document_request = build_document_chat_request(
        &provider,
        "Answer only from the SYNTHETIC-ONLY document.".to_owned(),
        "Reply with exactly ARCHIVIST_CAPACITY_CHAT_OK.".to_owned(),
    );

    let (metadata_result, document_result) = tokio::join!(
        chat_with_api_provider(&state, &provider, metadata_request),
        chat_with_api_provider(&state, &provider, document_request)
    );
    assert!(metadata_result.is_ok());
    assert_eq!(
        document_result.expect("Document Chat call").text,
        "ARCHIVIST_CAPACITY_CHAT_OK"
    );
    assert_eq!(capture.max_active.load(Ordering::Acquire), 2);

    let bodies = capture.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies
            .iter()
            .all(|body| body["model"] == archivist_core::MINIMAX_M3_MODEL)
    );
    assert!(bodies.iter().all(|body| body["max_tokens"] == 4096));
    assert!(
        bodies
            .iter()
            .all(|body| { body["chat_template_kwargs"]["thinking_mode"] == "disabled" })
    );
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body.get("response_format").is_some())
            .count(),
        1
    );

    handle.abort();
}

#[tokio::test]
async fn api_text_chat_honors_selected_provider_request_timeout() {
    use axum::Json as AxumJson;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new().route(
        "/chat/completions",
        post(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            AxumJson(json!({ "choices": [{ "message": { "content": "late" } }] }))
        }),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    let base_url = format!("http://{address}");
    let mut settings = api_provider_profile_settings(&base_url, "https://unused.example/v1");
    settings.ai.providers[0].tuning.request_timeout_seconds = Some(1);
    let provider = provider_for_default_text(&settings).unwrap();
    let request = build_document_chat_request(
        &provider,
        "document system".to_owned(),
        "document user".to_owned(),
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        chat_with_api_provider(&api_text_test_state(), &provider, request),
    )
    .await
    .expect("configured one-second timeout must return before outer guard");
    assert!(result.is_err(), "slow provider must hit configured timeout");

    handle.abort();
}

#[tokio::test]
async fn provider_draft_probe_uses_draft_endpoint_tuning_and_transient_secret() {
    use archivist_core::{
        AiProviderSettings, ProviderTuning, ReasoningEffort, StructuredOutputMode,
    };
    use std::sync::atomic::Ordering;

    let saved_capture = ProviderProbeCapture::default();
    let draft_capture = ProviderProbeCapture::default();
    let (saved_url, saved_handle) = spawn_mock_openai_probe(saved_capture.clone()).await;
    let (draft_url, draft_handle) = spawn_mock_openai_probe(draft_capture.clone()).await;

    let mut saved = RuntimeSettings::default();
    saved.ai.default_provider = "draft-provider".to_owned();
    saved.ai.providers = vec![AiProviderSettings {
        name: "draft-provider".to_owned(),
        kind: AiProviderKind::OpenaiCompatible,
        base_url: saved_url,
        default_text_model: Some("saved-model".to_owned()),
        default_vision_model: None,
        cost_per_1m_input_tokens_usd: None,
        cost_per_1m_output_tokens_usd: None,
        secret_id: None,
        enabled: true,
        tuning: ProviderTuning::default(),
    }];
    let request = TestProviderRequest {
        name: "draft-provider".to_owned(),
        kind: AiProviderKind::OpenaiCompatible,
        base_url: draft_url,
        model: "gpt-5-draft".to_owned(),
        tuning: ProviderTuning {
            reasoning_effort: Some(ReasoningEffort::High),
            max_output_tokens: Some(777),
            structured_output: Some(StructuredOutputMode::JsonObject),
            request_timeout_seconds: Some(2),
            ..ProviderTuning::default()
        },
        secret_id: None,
        secret: Some("draft-super-secret".to_owned()),
    };

    let provider = provider_test_target(&saved, &request).unwrap();
    let transient_secret = SecretString::from(request.secret.clone().unwrap());
    let result = test_ai_provider(&provider, Some(transient_secret.clone())).await;
    let response = provider_test_response(&provider, result, Some(&transient_secret));

    assert_eq!(saved_capture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(draft_capture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        draft_capture.authorization.lock().unwrap().as_deref(),
        Some("Bearer draft-super-secret")
    );
    let body = draft_capture.body.lock().unwrap().clone().unwrap();
    assert_eq!(body["model"], "gpt-5-draft");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["max_tokens"], 777);
    assert_eq!(body["response_format"], json!({ "type": "json_object" }));
    assert_eq!(response["ok"], true);
    assert_eq!(response["provider"], "draft-provider");
    assert_eq!(response["model"], "gpt-5-draft");
    assert!(!response.to_string().contains("draft-super-secret"));

    let echoed_error = provider_test_response(
        &provider,
        Err(anyhow!("upstream echoed draft-super-secret")),
        Some(&transient_secret),
    );
    assert_eq!(echoed_error["ok"], false);
    assert_eq!(echoed_error["provider"], "draft-provider");
    assert_eq!(echoed_error["model"], "gpt-5-draft");
    assert!(!echoed_error.to_string().contains("draft-super-secret"));

    saved_handle.abort();
    draft_handle.abort();
}
