//! Prompt library, prompt experiments and the prompt tester.

use crate::*;

pub(crate) async fn prompts(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "items": list_prompts(&state.pool).await? })))
}

pub(crate) async fn prompt_usage(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "items": list_prompt_usage(&state.pool).await? }),
    ))
}

pub(crate) async fn prompt_experiments(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "items": list_prompt_experiments(&state.pool).await? }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreatePromptRequest {
    pub(crate) stage: Stage,
    pub(crate) name: String,
    pub(crate) content: String,
    pub(crate) output_schema: Option<Value>,
    pub(crate) activate: Option<bool>,
}

pub(crate) async fn create_prompt_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<CreatePromptRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    let id = archivist_db::create_prompt(
        &state.pool,
        request.stage,
        &request.name,
        &request.content,
        request.output_schema,
        actor_id,
    )
    .await?;
    if request.activate.unwrap_or(false) {
        archivist_db::activate_prompt(&state.pool, id, actor_id).await?;
    }
    Ok(Json(json!({ "id": id })))
}

pub(crate) async fn activate_prompt_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    archivist_db::activate_prompt(&state.pool, id, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct TestPromptRequest {
    pub(crate) stage: Stage,
    pub(crate) content: String,
    pub(crate) sample_text: Option<String>,
    pub(crate) paperless_document_id: Option<i32>,
    pub(crate) provider_name: Option<String>,
    pub(crate) model: Option<String>,
}

pub(crate) async fn test_prompt_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<TestPromptRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    if request.content.trim().is_empty() {
        return Err(ApiError::bad_request("prompt content must not be empty"));
    }

    let settings = get_runtime_settings(&state.pool).await?;
    let sample_text = prompt_test_sample_text(&state, &settings, &request).await?;
    let provider = prompt_test_provider(&settings, &request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    let mut chat_request = match request.stage {
        Stage::Ocr => build_ocr_prompt_test_chat_request(&sample_text),
        Stage::Metadata => {
            let enabled =
                MetadataFieldFlags::from_enabled_stages(&settings.workflow.enabled_stages);
            let catalog = load_metadata_prompt_test_catalog(&state.pool, enabled).await?;
            build_metadata_prompt_test_chat_request(
                &settings,
                &provider.tuning,
                &sample_text,
                catalog,
            )
            .map_err(|error| ApiError::bad_request(error.to_string()))?
        }
        Stage::Apply => {
            return Err(ApiError::bad_request(format!(
                "prompt testing is not supported for stage {}",
                request.stage
            )));
        }
    };
    apply_prompt_test_system_prompt(&mut chat_request, &request.content);
    chat_request.model = provider.model.clone();
    apply_api_provider_tuning(&provider, &mut chat_request);

    let response = chat_with_api_provider(&state, &provider, chat_request.clone()).await?;
    let parsed = parse_prompt_test_output(request.stage, &response.text);
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "prompt.tested".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: request.paperless_document_id,
            before: None,
            after: None,
            metadata: Some(json!({
                "stage": request.stage,
                "provider": provider.name,
                "model": provider.model,
                "sample_chars": sample_text.chars().count(),
                "duration_ms": response.duration_ms,
                "valid": parsed.validation_errors.is_empty()
            })),
            outcome: if parsed.validation_errors.is_empty() {
                "success".to_owned()
            } else {
                "validation_failed".to_owned()
            },
            error_message: parsed.validation_errors.first().cloned(),
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    Ok(Json(json!({
        "provider": response.provider,
        "model": response.model,
        "stage": request.stage,
        "raw_text": response.text,
        "parsed": parsed.parsed,
        "validation_errors": parsed.validation_errors,
        "warnings": parsed.warnings,
        "duration_ms": response.duration_ms
    })))
}

pub(crate) fn prompt_test_provider(
    settings: &RuntimeSettings,
    request: &TestPromptRequest,
) -> Result<ApiProvider> {
    let mut provider = if let Some(provider_name) = request
        .provider_name
        .as_deref()
        .filter(|provider_name| !provider_name.trim().is_empty())
    {
        provider_by_name(settings, provider_name)?
    } else {
        match request.stage {
            Stage::Metadata => provider_for_stage_text(settings, Stage::Metadata)?,
            Stage::Ocr | Stage::Apply => provider_for_default_text(settings)?,
        }
    };
    if let Some(model) = request
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
    {
        provider.model = model.trim().to_owned();
    }
    Ok(provider)
}

#[derive(Debug)]
pub(crate) struct PromptTestParsed {
    pub(crate) parsed: Value,
    pub(crate) validation_errors: Vec<String>,
    pub(crate) warnings: Vec<String>,
}

pub(crate) async fn prompt_test_sample_text(
    state: &AppState,
    settings: &RuntimeSettings,
    request: &TestPromptRequest,
) -> ApiResult<String> {
    if let Some(sample_text) = request
        .sample_text
        .as_deref()
        .filter(|sample_text| !sample_text.trim().is_empty())
    {
        return Ok(sample_text.trim().chars().take(20_000).collect());
    }
    if let Some(document_id) = request.paperless_document_id {
        if document_id <= 0 {
            return Err(ApiError::bad_request(
                "paperless_document_id must be positive",
            ));
        }
        let paperless =
            paperless_client_from_settings(&state.pool, &state.config, settings).await?;
        let document = paperless.get_document(document_id).await?;
        if let Some(content) = document
            .content
            .filter(|content| !content.trim().is_empty())
        {
            return Ok(content.trim().chars().take(20_000).collect());
        }
        return Err(ApiError::bad_request(
            "selected Paperless document has no content to test against",
        ));
    }
    Err(ApiError::bad_request(
        "provide sample_text or paperless_document_id",
    ))
}

pub(crate) fn build_ocr_prompt_test_chat_request(sample_text: &str) -> ChatRequest {
    ChatRequest {
        model: String::new(),
        system_prompt: String::new(),
        user_prompt: format!(
            "Test this OCR prompt against sample text. Return the best OCR text only.\n\nSample text:\n{}",
            sample_text.chars().take(12_000).collect::<String>()
        ),
        temperature: 0.0,
        num_ctx: None,
        response_schema: None,
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    }
}

#[derive(Debug)]
pub(crate) struct MetadataPromptTestCatalog {
    pub(crate) correspondents: Vec<String>,
    pub(crate) document_types: Vec<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) fields: Vec<(String, Option<String>)>,
}

pub(crate) async fn load_metadata_prompt_test_catalog(
    pool: &DbPool,
    enabled: MetadataFieldFlags,
) -> ApiResult<MetadataPromptTestCatalog> {
    let correspondents = if enabled.correspondent {
        list_allowed_named_entities(pool, "paperless_correspondents").await?
    } else {
        Vec::new()
    };
    let document_types = if enabled.document_type {
        list_allowed_named_entities(pool, "paperless_document_types").await?
    } else {
        Vec::new()
    };
    let tags = if enabled.tags {
        list_allowed_tag_names(pool).await?
    } else {
        Vec::new()
    };
    let fields = if enabled.fields {
        list_custom_fields(pool)
            .await?
            .into_iter()
            .map(|field| (field.name, field.data_type))
            .collect()
    } else {
        Vec::new()
    };
    Ok(MetadataPromptTestCatalog {
        correspondents,
        document_types,
        tags,
        fields,
    })
}

pub(crate) fn build_metadata_prompt_test_chat_request(
    settings: &RuntimeSettings,
    tuning: &EffectiveTuning,
    sample_text: &str,
    catalog: MetadataPromptTestCatalog,
) -> Result<ChatRequest> {
    let enabled = MetadataFieldFlags::from_enabled_stages(&settings.workflow.enabled_stages);
    if !enabled.any() {
        return Err(anyhow!(
            "metadata prompt testing requires the metadata workflow stage to be enabled"
        ));
    }

    let content_lower = sample_text.to_lowercase();
    let allowed_list_max = tuning.allowed_list_max as usize;
    let correspondents =
        prefilter_allowed_list_lower(&content_lower, &catalog.correspondents, allowed_list_max);
    let document_types =
        prefilter_allowed_list_lower(&content_lower, &catalog.document_types, allowed_list_max);
    let tags = prefilter_allowed_list_lower(&content_lower, &catalog.tags, allowed_list_max);
    let fields = catalog
        .fields
        .into_iter()
        .filter(|(name, _)| settings.fields.field_enabled(name))
        .collect::<Vec<_>>();
    let field_names = fields
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let detection = detect_document_language(sample_text);
    let language = PromptLanguageContext::new(&detection, &settings.tagging.tag_output_language);
    let mut request = prompt_for_metadata(
        sample_text,
        &correspondents,
        &document_types,
        &tags,
        &fields,
        &enabled,
        &language,
        tuning.max_tags as usize,
        settings.fields.max_fields,
        "",
    );
    request.response_schema = schema_for_metadata(
        &correspondents,
        &document_types,
        &tags,
        &field_names,
        &enabled,
        tuning.max_tags as usize,
        settings.fields.max_fields,
    );
    Ok(request)
}

pub(crate) fn apply_prompt_test_system_prompt(request: &mut ChatRequest, content: &str) {
    request.system_prompt = content.trim().to_owned();
}

pub(crate) fn parse_prompt_test_output(stage: Stage, text: &str) -> PromptTestParsed {
    match stage {
        Stage::Ocr => PromptTestParsed {
            parsed: json!({ "content": text }),
            validation_errors: Vec::new(),
            warnings: Vec::new(),
        },
        Stage::Metadata => parse_metadata_prompt_test_output(text),
        Stage::Apply => PromptTestParsed {
            parsed: Value::Null,
            validation_errors: vec![format!("unsupported stage: {stage}")],
            warnings: Vec::new(),
        },
    }
}

pub(crate) fn parse_metadata_prompt_test_output(text: &str) -> PromptTestParsed {
    let parsed = parse_metadata_suggestion(text);
    let mut validation_errors = Vec::new();
    if let Some(envelope_error) = parsed.diagnostics.envelope_error {
        validation_errors.push(match envelope_error {
            MetadataEnvelopeError::NoJson => {
                "metadata response envelope is not valid JSON".to_owned()
            }
            MetadataEnvelopeError::NonObject => {
                "metadata response must be a JSON object".to_owned()
            }
        });
    }
    if !parsed.diagnostics.invalid_fields.is_empty() {
        validation_errors.push(format!(
            "metadata field(s) have wrong types or unknown nested properties: {}",
            parsed.diagnostics.invalid_fields.join(", ")
        ));
    }
    if parsed.diagnostics.unknown_field_count > 0 {
        validation_errors.push(format!(
            "metadata response contains {} unknown field(s)",
            parsed.diagnostics.unknown_field_count
        ));
    }

    let mut warnings = parsed
        .suggestion
        .document_date
        .as_ref()
        .map(|date| date.warnings.clone())
        .unwrap_or_default();
    if parsed.diagnostics.status == MetadataParseStatus::Omitted {
        warnings.push("metadata response omitted every requested field".to_owned());
    }

    PromptTestParsed {
        parsed: json!(parsed),
        validation_errors,
        warnings,
    }
}
