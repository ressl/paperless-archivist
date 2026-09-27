//! AI provider resolution, model discovery, runtime hints and provider test calls.

use crate::*;

#[derive(Debug, Serialize)]
pub(crate) struct OllamaInstalledModelsResponse {
    pub(crate) provider: String,
    pub(crate) models: Vec<OllamaInstalledModel>,
}

#[derive(Debug, Serialize)]
pub(crate) struct OllamaInstalledModel {
    pub(crate) name: String,
    pub(crate) parameter_size: Option<String>,
    pub(crate) quantization_level: Option<String>,
    pub(crate) size_bytes: Option<u64>,
    pub(crate) size_gb: Option<f64>,
    pub(crate) modified_at: Option<String>,
    pub(crate) digest: Option<String>,
}

pub(crate) async fn model_provider_models(
    State(state): State<AppState>,
    _auth: Authenticated,
    Path(name): Path<String>,
) -> ApiResult<Json<OllamaInstalledModelsResponse>> {
    let settings = get_runtime_settings(&state.pool).await?;
    let provider = provider_by_name(&settings, &name)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    validate_outbound_url(&provider.base_url)
        .await
        .map_err(|error| {
            ApiError::bad_request(format!("provider base URL rejected: {}", error.message))
        })?;
    let secret = provider_secret(&state, &provider).await?;
    let models = discover_provider_models(&provider, secret).await?;
    Ok(Json(OllamaInstalledModelsResponse {
        provider: provider.name,
        models,
    }))
}

/// True for an Ollama provider that points at the hosted cloud (ollama.com),
/// whose model catalog is exposed through the OpenAI-compatible
/// `/v1/models` endpoint rather than the local-runner `/api/tags`.
pub(crate) fn is_ollama_cloud(base_url: &str) -> bool {
    base_url.to_ascii_lowercase().contains("ollama.com")
}

/// Heuristic filter for OpenAI's noisy `/models` list: keep the chat/vision
/// families (gpt-*, chatgpt-*, o-series) and drop embeddings, audio, image,
/// moderation, and search models that can't drive the metadata/OCR stages.
pub(crate) fn openai_id_is_chat_capable(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let keep = id.starts_with("gpt-")
        || id.starts_with("chatgpt")
        || id.starts_with("o1")
        || id.starts_with("o3")
        || id.starts_with("o4");
    let drop = id.contains("embedding")
        || id.contains("whisper")
        || id.contains("tts")
        || id.contains("audio")
        || id.contains("realtime")
        || id.contains("transcribe")
        || id.contains("image")
        || id.contains("dall-e")
        || id.contains("moderation")
        || id.contains("search");
    keep && !drop
}

/// Runs a provider model-listing future under a short timeout so the
/// interactive "sync" button never hangs the settings page.
pub(crate) async fn list_with_timeout<F>(fut: F) -> ApiResult<Vec<String>>
where
    F: std::future::Future<Output = anyhow::Result<Vec<String>>>,
{
    let ids = tokio::time::timeout(std::time::Duration::from_secs(12), fut)
        .await
        .map_err(|_| ApiError::bad_request("model discovery timed out"))??;
    Ok(ids)
}

/// Discovers the available models for any provider kind. Ollama local uses
/// `/api/tags`; Ollama Cloud, OpenAI, OpenAI-compatible and Anthropic all use
/// their `/v1/models`-style listing (Anthropic via its Models API). The result
/// is normalised to the `OllamaInstalledModel` shape — remote providers fill
/// only `name`, since their listings carry no size/quant metadata.
pub(crate) async fn discover_provider_models(
    provider: &ApiProvider,
    secret: Option<SecretString>,
) -> ApiResult<Vec<OllamaInstalledModel>> {
    let base = provider.base_url.trim_end_matches('/');
    match provider.kind {
        AiProviderKind::Ollama if !is_ollama_cloud(base) => {
            let client = OllamaClient::new_with_timeout(
                &provider.name,
                base,
                secret,
                std::time::Duration::from_secs(12),
            )?;
            let models = client.list_models().await.map_err(|error| {
                ApiError::internal(format!("Ollama model discovery failed: {error}"))
            })?;
            Ok(models.into_iter().map(OllamaInstalledModel::from).collect())
        }
        AiProviderKind::Ollama => {
            // Ollama Cloud: the catalog lives at ollama.com/v1/models.
            let client =
                OpenAiCompatibleClient::new(&provider.name, &format!("{base}/v1"), secret)?;
            let ids = list_with_timeout(client.list_models()).await?;
            Ok(ids.into_iter().map(OllamaInstalledModel::from_id).collect())
        }
        AiProviderKind::Openai => {
            if secret.is_none() {
                return Err(ApiError::bad_request(
                    "OpenAI model discovery requires an API key — enter and save the provider's API key first.",
                ));
            }
            let client = OpenAiCompatibleClient::new(&provider.name, base, secret)?;
            let ids = list_with_timeout(client.list_models()).await?;
            Ok(ids
                .into_iter()
                .filter(|id| openai_id_is_chat_capable(id))
                .map(OllamaInstalledModel::from_id)
                .collect())
        }
        AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new(&provider.name, base, secret)?;
            let ids = list_with_timeout(client.list_models()).await?;
            Ok(ids.into_iter().map(OllamaInstalledModel::from_id).collect())
        }
        AiProviderKind::Anthropic => {
            let key = secret.ok_or_else(|| {
                ApiError::bad_request("Anthropic model discovery requires an API key")
            })?;
            let client = AnthropicClient::new(&provider.name, base, key)?;
            let ids = list_with_timeout(client.list_models()).await?;
            Ok(ids.into_iter().map(OllamaInstalledModel::from_id).collect())
        }
        AiProviderKind::Mineru => {
            // MinerU serves one implicit model; there is no /models endpoint.
            Ok(vec![OllamaInstalledModel::from_id("mineru".to_owned())])
        }
    }
}

impl OllamaInstalledModel {
    /// Builds an entry from a bare model id (remote providers' listings carry
    /// no size/quantisation metadata).
    pub(crate) fn from_id(id: String) -> Self {
        Self {
            name: id,
            parameter_size: None,
            quantization_level: None,
            size_bytes: None,
            size_gb: None,
            modified_at: None,
            digest: None,
        }
    }
}

impl From<OllamaModel> for OllamaInstalledModel {
    fn from(model: OllamaModel) -> Self {
        let details = model.details;
        let size_gb = model.size.map(|size| size as f64 / 1024_f64.powi(3));
        Self {
            name: model.name,
            parameter_size: details
                .as_ref()
                .and_then(|details| details.parameter_size.clone()),
            quantization_level: details
                .as_ref()
                .and_then(|details| details.quantization_level.clone()),
            size_bytes: model.size,
            size_gb,
            modified_at: model.modified_at,
            digest: model.digest,
        }
    }
}

// ----- /api/ai/runtime-hints --------------------------------------------
//
// v1.6.2 issue #127: live runtime hints for the active (or queried) AI
// provider. For Ollama this hits `/api/version` and `/api/ps` and exposes
// the loaded-model VRAM footprint plus a hint about the env-only knobs
// (NUM_PARALLEL, MAX_LOADED_MODELS, KEEP_ALIVE) that Ollama doesn't surface
// in its HTTP API. For non-Ollama providers we return a stub so the
// frontend can render a uniform card.

#[derive(Debug, Deserialize)]
pub(crate) struct AiRuntimeHintsQuery {
    /// Optional explicit provider name. Defaults to
    /// `ai.default_provider` when omitted.
    pub(crate) provider: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AiRuntimeHintsResponse {
    pub(crate) provider: String,
    pub(crate) reachable: bool,
    pub(crate) version: Option<String>,
    pub(crate) loaded_models: Vec<AiLoadedModelResponse>,
    /// Ollama-deploy-time-only knobs (env vars on the Ollama pod). Always
    /// `None` — the `hint` field explains where to set them.
    pub(crate) num_parallel: Option<i64>,
    pub(crate) max_loaded_models: Option<i64>,
    pub(crate) keep_alive: Option<String>,
    pub(crate) hint: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct AiLoadedModelResponse {
    pub(crate) name: String,
    pub(crate) size_vram_bytes: Option<u64>,
    /// Optional last-used timestamp; pass-through of Ollama's `expires_at`.
    /// Always serialized so the frontend can show it when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_used_at: Option<String>,
}

pub(crate) const OLLAMA_RUNTIME_HINT: &str = "NUM_PARALLEL, MAX_LOADED_MODELS, KEEP_ALIVE are set on the Ollama deployment, not in Archivist. Edit the Ollama k8s manifest to change them.";

pub(crate) async fn ai_runtime_hints(
    State(state): State<AppState>,
    _auth: Authenticated,
    Query(query): Query<AiRuntimeHintsQuery>,
) -> ApiResult<Json<AiRuntimeHintsResponse>> {
    let settings = get_runtime_settings(&state.pool).await?;
    // Pick the requested provider, falling back to the active default.
    let provider_name = query
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(settings.ai.default_provider.as_str())
        .to_owned();
    let provider = match provider_by_name(&settings, &provider_name) {
        Ok(provider) => provider,
        Err(error) => {
            // Unknown provider name → return the stub shape rather than 4xx
            // so the frontend can render an error card consistently.
            return Ok(Json(AiRuntimeHintsResponse {
                provider: provider_name,
                reachable: false,
                version: None,
                loaded_models: Vec::new(),
                num_parallel: None,
                max_loaded_models: None,
                keep_alive: None,
                hint: Some(error.to_string()),
            }));
        }
    };
    let response = match provider.kind {
        AiProviderKind::Ollama => fetch_ollama_runtime_hints(&state, &provider).await,
        _ => non_ollama_runtime_hints(&provider),
    };
    Ok(Json(response))
}

pub(crate) async fn fetch_ollama_runtime_hints(
    state: &AppState,
    provider: &ApiProvider,
) -> AiRuntimeHintsResponse {
    let secret = match provider_secret(state, provider).await {
        Ok(secret) => secret,
        Err(error) => {
            return AiRuntimeHintsResponse {
                provider: provider.name.clone(),
                reachable: false,
                version: None,
                loaded_models: Vec::new(),
                num_parallel: None,
                max_loaded_models: None,
                keep_alive: None,
                hint: Some(format!("Ollama secret resolution failed: {error}")),
            };
        }
    };
    if let Err(error) = validate_outbound_url(&provider.base_url).await {
        return AiRuntimeHintsResponse {
            provider: provider.name.clone(),
            reachable: false,
            version: None,
            loaded_models: Vec::new(),
            num_parallel: None,
            max_loaded_models: None,
            keep_alive: None,
            hint: Some(format!("provider base URL rejected: {}", error.message)),
        };
    }
    let client = match OllamaClient::new_with_timeout(
        &provider.name,
        &provider.base_url,
        secret,
        std::time::Duration::from_secs(5),
    ) {
        Ok(client) => client,
        Err(error) => {
            return AiRuntimeHintsResponse {
                provider: provider.name.clone(),
                reachable: false,
                version: None,
                loaded_models: Vec::new(),
                num_parallel: None,
                max_loaded_models: None,
                keep_alive: None,
                hint: Some(format!("failed to build Ollama client: {error}")),
            };
        }
    };
    fetch_ollama_runtime_hints_with_client(&provider.name, &client).await
}

/// Inner Ollama probe split out for testability. Takes an `OllamaClient`
/// already wired to the runtime URL, hits `/api/version` and `/api/ps`,
/// composes the response. Independent of `AppState` so unit tests can
/// point a real `OllamaClient` at a mock HTTP server.
pub(crate) async fn fetch_ollama_runtime_hints_with_client(
    provider_name: &str,
    client: &OllamaClient,
) -> AiRuntimeHintsResponse {
    let mut response = AiRuntimeHintsResponse {
        provider: provider_name.to_owned(),
        reachable: false,
        version: None,
        loaded_models: Vec::new(),
        num_parallel: None,
        max_loaded_models: None,
        keep_alive: None,
        hint: Some(OLLAMA_RUNTIME_HINT.to_owned()),
    };
    // `/api/version` first — gates `reachable`. If even the version probe
    // fails we surface the error in `hint` and skip the loaded-models call.
    match client.version().await {
        Ok(version) => {
            response.reachable = true;
            response.version = Some(version);
        }
        Err(error) => {
            response.reachable = false;
            response.hint = Some(format!("Ollama unreachable: {error}"));
            return response;
        }
    }
    match client.loaded_models().await {
        Ok(models) => {
            response.loaded_models = models
                .into_iter()
                .map(AiLoadedModelResponse::from)
                .collect();
        }
        Err(error) => {
            // Reachable, but /api/ps failed — keep `reachable: true`, surface
            // the partial failure in the hint so the UI can still show the
            // version while explaining why loaded-models is blank.
            response.hint = Some(format!("Ollama /api/ps failed: {error}"));
        }
    }
    response
}

pub(crate) fn non_ollama_runtime_hints(provider: &ApiProvider) -> AiRuntimeHintsResponse {
    let kind = match provider.kind {
        AiProviderKind::Openai => "openai",
        AiProviderKind::Anthropic => "anthropic",
        AiProviderKind::OpenaiCompatible => "openai_compatible",
        AiProviderKind::Ollama => "ollama",
        AiProviderKind::Mineru => "mineru",
    };
    AiRuntimeHintsResponse {
        provider: provider.name.clone(),
        reachable: true,
        version: None,
        loaded_models: Vec::new(),
        num_parallel: None,
        max_loaded_models: None,
        keep_alive: None,
        hint: Some(format!(
            "{kind}-specific tuning is not server-side observable from Archivist."
        )),
    }
}

impl From<OllamaLoadedModel> for AiLoadedModelResponse {
    fn from(model: OllamaLoadedModel) -> Self {
        Self {
            name: model.name,
            size_vram_bytes: model.size_vram,
            last_used_at: model.expires_at,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ApiProvider {
    pub(crate) name: String,
    pub(crate) kind: AiProviderKind,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) secret_id: Option<Uuid>,
    pub(crate) tuning: EffectiveTuning,
}

pub(crate) fn provider_test_target(
    settings: &RuntimeSettings,
    request: &TestProviderRequest,
) -> Result<ApiProvider, ApiError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("provider name must not be blank"));
    }
    let model = request.model.trim();
    if model.is_empty() && !matches!(request.kind, AiProviderKind::Mineru) {
        return Err(ApiError::bad_request("provider model must not be blank"));
    }
    let model = if model.is_empty() { "mineru" } else { model };
    let base_url = provider_base_url(name, &request.base_url).map_err(|error| {
        ApiError::bad_request(format!("AI provider '{name}' base URL: {error}"))
    })?;
    let draft = AiProviderSettings {
        name: name.to_owned(),
        kind: request.kind.clone(),
        base_url: base_url.clone(),
        default_text_model: Some(model.to_owned()),
        default_vision_model: None,
        cost_per_1m_input_tokens_usd: None,
        cost_per_1m_output_tokens_usd: None,
        secret_id: request.secret_id,
        enabled: true,
        tuning: request.tuning.clone(),
    };
    let mut effective_settings = settings.clone();
    effective_settings.ai.default_provider = draft.name.clone();
    if let Some(index) = effective_settings
        .ai
        .providers
        .iter()
        .position(|provider| provider.name == draft.name)
    {
        effective_settings.ai.providers[index] = draft.clone();
    } else {
        effective_settings.ai.providers.push(draft.clone());
    }
    let tuning = effective_settings.effective_tuning_for_provider(&draft);
    Ok(ApiProvider {
        name: draft.name,
        kind: draft.kind,
        base_url,
        model: model.to_owned(),
        secret_id: draft.secret_id,
        tuning,
    })
}

pub(crate) async fn provider_test_secret(
    state: &AppState,
    settings: &RuntimeSettings,
    provider: &ApiProvider,
    transient: Option<String>,
) -> Result<Option<SecretString>> {
    if let Some(secret) = transient.filter(|secret| !secret.trim().is_empty()) {
        return Ok(Some(SecretString::from(secret)));
    }
    if let Some(secret_id) = provider.secret_id
        && !saved_provider_secret_matches(
            settings,
            &provider.name,
            &provider.kind,
            &provider.base_url,
            secret_id,
        )
    {
        // Secrets are write-only: a stored key may only be sent to the exact
        // endpoint it was saved for, otherwise a settings admin could point
        // a draft at their own URL and exfiltrate it. #397
        return Err(anyhow!(
            "stored provider secret can only be tested against its saved provider endpoint; \
             enter the key again to test a different endpoint"
        ));
    }
    provider_secret(state, provider).await
}

/// The base URL a saved provider actually talks to (the built-in Ollama
/// provider uses `ai.ollama_base_url`), normalized like [`provider_base_url`].
pub(crate) fn saved_provider_endpoint<'a>(
    settings: &'a RuntimeSettings,
    provider: &'a AiProviderSettings,
) -> &'a str {
    let base_url = if provider.name.eq_ignore_ascii_case("ollama") {
        settings.ai.ollama_base_url.as_str()
    } else {
        provider.base_url.as_str()
    };
    base_url.trim().trim_end_matches('/')
}

/// Whether `secret_id` is the stored secret of the saved provider with this
/// name, kind and endpoint. #397
pub(crate) fn saved_provider_secret_matches(
    settings: &RuntimeSettings,
    name: &str,
    kind: &AiProviderKind,
    base_url: &str,
    secret_id: Uuid,
) -> bool {
    settings.ai.providers.iter().any(|saved| {
        saved.secret_id == Some(secret_id)
            && saved.name.eq_ignore_ascii_case(name.trim())
            && &saved.kind == kind
            && saved_provider_endpoint(settings, saved) == base_url.trim().trim_end_matches('/')
    })
}

/// Reject a settings update that would re-bind a stored secret to another
/// target: every secret reference must either be unchanged for the same
/// provider/profile *and* endpoint, or be freshly supplied in this request.
/// Otherwise a settings admin could move a stored OpenAI/Anthropic key (or a
/// Paperless token) onto a URL they control. #397, #396
pub(crate) fn validate_secret_bindings(
    current: &RuntimeSettings,
    next: &RuntimeSettings,
    new_provider_secrets: &HashSet<String>,
    new_paperless_token: bool,
    new_webhook_url: bool,
) -> ApiResult<()> {
    for provider in &next.ai.providers {
        let Some(secret_id) = provider.secret_id else {
            continue;
        };
        if new_provider_secrets.contains(&provider.name) {
            continue;
        }
        if !saved_provider_secret_matches(
            current,
            &provider.name,
            &provider.kind,
            saved_provider_endpoint(next, provider),
            secret_id,
        ) {
            return Err(ApiError::bad_request(format!(
                "AI provider '{}': the stored API key is bound to the saved provider and \
                 endpoint; enter the key again after changing the provider or its base URL",
                provider.name
            )));
        }
    }

    let paperless_token_matches = |secret_id: Uuid, base_url: &str| {
        (current.paperless.token_secret_id == Some(secret_id)
            && same_http_origin(&current.paperless.base_url, base_url))
            || current.paperless.archive_profiles.iter().any(|profile| {
                profile.token_secret_id == Some(secret_id)
                    && same_http_origin(&profile.base_url, base_url)
            })
    };
    if !new_paperless_token
        && let Some(secret_id) = next.paperless.token_secret_id
        && !paperless_token_matches(secret_id, &next.paperless.base_url)
    {
        return Err(ApiError::bad_request(
            "Paperless: the stored token is bound to its saved instance; enter the token \
             again after changing the Paperless base URL",
        ));
    }
    for profile in &next.paperless.archive_profiles {
        let Some(secret_id) = profile.token_secret_id else {
            continue;
        };
        // A profile may reuse the global token on the global instance
        // (normalization seeds the default profile that way).
        let reuses_global_token = next.paperless.token_secret_id == Some(secret_id)
            && same_http_origin(&next.paperless.base_url, &profile.base_url);
        if !reuses_global_token && !paperless_token_matches(secret_id, &profile.base_url) {
            return Err(ApiError::bad_request(format!(
                "archive profile '{}': the stored token is bound to its saved instance; \
                 enter the token again after changing the profile base URL",
                profile.name
            )));
        }
    }

    if !new_webhook_url
        && let Some(secret_id) = next.notifications.webhook_url_secret_id
        && current.notifications.webhook_url_secret_id != Some(secret_id)
    {
        return Err(ApiError::bad_request(
            "notification webhook secret reference does not belong to the saved settings",
        ));
    }
    Ok(())
}

pub(crate) fn provider_test_response(
    provider: &ApiProvider,
    result: Result<Value>,
    secret: Option<&SecretString>,
) -> Value {
    match result {
        Ok(_) => json!({
            "ok": true,
            "provider": provider.name,
            "model": provider.model,
        }),
        Err(error) => {
            let mut message = error.to_string();
            if let Some(secret) = secret {
                let exposed = secret.expose_secret();
                if !exposed.is_empty() {
                    message = message.replace(exposed, "[REDACTED]");
                }
            }
            json!({
                "ok": false,
                "provider": provider.name,
                "model": provider.model,
                "error": message,
            })
        }
    }
}

pub(crate) fn provider_by_name(settings: &RuntimeSettings, name: &str) -> Result<ApiProvider> {
    let mut provider = settings
        .ai
        .providers
        .iter()
        .find(|provider| provider.name.eq_ignore_ascii_case(name))
        .cloned()
        .or_else(|| {
            if name.eq_ignore_ascii_case("ollama") {
                Some(archivist_core::AiProviderSettings::ollama_default())
            } else {
                None
            }
        })
        .ok_or_else(|| not_configured(format!("AI provider '{name}' is not configured")))?;
    if provider.name.eq_ignore_ascii_case("ollama") {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings.ai.default_model_for_provider(&provider, false);
    let base_url = provider_base_url(&provider.name, &provider.base_url)?;
    let tuning = settings.effective_tuning_for_provider(&provider);
    Ok(ApiProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
        tuning,
    })
}

pub(crate) fn provider_for_default_text(settings: &RuntimeSettings) -> Result<ApiProvider> {
    let mut provider = settings
        .ai
        .providers
        .iter()
        .find(|provider| {
            provider.enabled
                && provider
                    .name
                    .eq_ignore_ascii_case(&settings.ai.default_provider)
        })
        .cloned()
        .or_else(|| {
            if settings.ai.default_provider.eq_ignore_ascii_case("ollama") {
                Some(archivist_core::AiProviderSettings::ollama_default())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            not_configured(format!(
                "AI provider '{}' is not configured or disabled",
                settings.ai.default_provider
            ))
        })?;
    if provider.name.eq_ignore_ascii_case("ollama") {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings.ai.default_model_for_provider(&provider, false);
    let base_url = provider_base_url(&provider.name, &provider.base_url)?;
    let tuning = settings.effective_tuning_for_provider(&provider);
    Ok(ApiProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
        tuning,
    })
}

pub(crate) fn provider_for_stage_text(
    settings: &RuntimeSettings,
    stage: Stage,
) -> Result<ApiProvider> {
    let stage_override = settings
        .ai
        .stage_models
        .iter()
        .find(|override_model| override_model.stage == stage);
    let provider_name = stage_override
        .map(|override_model| override_model.provider.as_str())
        .unwrap_or(&settings.ai.default_provider);
    let mut provider = settings
        .ai
        .providers
        .iter()
        .find(|provider| provider.enabled && provider.name.eq_ignore_ascii_case(provider_name))
        .cloned()
        .or_else(|| {
            if provider_name.eq_ignore_ascii_case("ollama") {
                Some(archivist_core::AiProviderSettings::ollama_default())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            not_configured(format!(
                "AI provider '{provider_name}' is not configured or disabled"
            ))
        })?;
    if provider.name.eq_ignore_ascii_case("ollama") {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings
        .ai
        .model_for_stage_provider(&provider, stage, false);
    let base_url = provider_base_url(&provider.name, &provider.base_url)?;
    let tuning = settings.effective_tuning_for_provider(&provider);
    Ok(ApiProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
        tuning,
    })
}

pub(crate) fn provider_base_url(provider_name: &str, configured: &str) -> Result<String> {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "AI provider '{provider_name}' has an empty base URL; repair the runtime settings"
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

pub(crate) async fn provider_secret(
    state: &AppState,
    provider: &ApiProvider,
) -> Result<Option<SecretString>> {
    let Some(secret_id) = provider.secret_id else {
        return Ok(None);
    };
    resolve_secret(&state.pool, &state.config.secret_key, secret_id).await
}

pub(crate) fn apply_api_provider_tuning(provider: &ApiProvider, request: &mut ChatRequest) {
    request.num_ctx = provider.tuning.text_num_ctx;
    request.reasoning_effort = Some(provider.tuning.reasoning_effort);
    request.max_output_tokens = provider.tuning.max_output_tokens;
    request.structured_output = Some(provider.tuning.structured_output);
}

pub(crate) fn provider_test_chat_request(provider: &ApiProvider) -> ChatRequest {
    let mut request = ChatRequest {
        model: provider.model.clone(),
        system_prompt: "Return only a short JSON provider health result.".to_owned(),
        user_prompt: "Return {\"status\":\"ok\"}.".to_owned(),
        temperature: 0.0,
        num_ctx: None,
        response_schema: Some(json!({
            "type": "object",
            "properties": { "status": { "type": "string" } },
            "required": ["status"],
            "additionalProperties": false
        })),
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    };
    apply_api_provider_tuning(provider, &mut request);
    request
}

pub(crate) async fn test_ai_provider(
    provider: &ApiProvider,
    secret: Option<SecretString>,
) -> Result<Value> {
    let timeout =
        std::time::Duration::from_secs(u64::from(provider.tuning.request_timeout_seconds));
    match provider.kind {
        AiProviderKind::Ollama => {
            let client = OllamaClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            let response = client.chat(provider_test_chat_request(provider)).await?;
            Ok(json!({
                "provider": response.provider,
                "model": response.model,
                "text": response.text,
            }))
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            let response = client.chat(provider_test_chat_request(provider)).await?;
            Ok(
                json!({ "provider": response.provider, "model": response.model, "text": response.text }),
            )
        }
        AiProviderKind::Anthropic => {
            let secret = secret.ok_or_else(|| {
                anyhow!("AI provider '{}' requires an API key secret", provider.name)
            })?;
            let client = AnthropicClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            let response = client.chat(provider_test_chat_request(provider)).await?;
            Ok(
                json!({ "provider": response.provider, "model": response.model, "text": response.text }),
            )
        }
        AiProviderKind::Mineru => {
            let client = MineruClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            client.test_connection().await
        }
    }
}

pub(crate) async fn chat_with_api_provider(
    state: &AppState,
    provider: &ApiProvider,
    request: ChatRequest,
) -> Result<AiResponse> {
    let timeout =
        std::time::Duration::from_secs(u64::from(provider.tuning.request_timeout_seconds));
    match provider.kind {
        AiProviderKind::Ollama => {
            let client = OllamaClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(state, provider).await?,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(state, provider).await?,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Anthropic => {
            let secret = provider_secret(state, provider).await?.ok_or_else(|| {
                anyhow!("AI provider '{}' requires an API key secret", provider.name)
            })?;
            let client = AnthropicClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Mineru => Err(anyhow!(
            "AI provider '{}' uses kind \"mineru\" which is vision-only (OCR); \
             select a text-capable provider for this stage",
            provider.name
        )),
    }
}
