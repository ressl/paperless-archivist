//! Runtime settings, secret references, connectivity tests and notification webhooks.

use crate::*;

pub(crate) async fn settings(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<RuntimeSettings>> {
    Ok(Json(get_runtime_settings(&state.pool).await?))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateSettingsRequest {
    pub(crate) settings: RuntimeSettings,
    pub(crate) paperless_token: Option<String>,
    pub(crate) notification_webhook_url: Option<String>,
    pub(crate) provider_secrets: Option<HashMap<String, String>>,
}

pub(crate) fn canonicalize_provider_secrets(
    settings: &RuntimeSettings,
    provider_secrets: HashMap<String, String>,
) -> ApiResult<HashMap<String, String>> {
    let mut canonical = HashMap::with_capacity(provider_secrets.len());
    for (submitted_name, secret) in provider_secrets {
        let submitted_name = submitted_name.trim();
        let provider = settings
            .ai
            .providers
            .iter()
            .find(|provider| provider.name.eq_ignore_ascii_case(submitted_name))
            .ok_or_else(|| {
                ApiError::bad_request(format!(
                    "AI provider secret target '{submitted_name}' is not configured"
                ))
            })?;
        if canonical.insert(provider.name.clone(), secret).is_some() {
            return Err(ApiError::bad_request(format!(
                "multiple AI provider secrets resolve to '{}'",
                provider.name
            )));
        }
    }
    Ok(canonical)
}

pub(crate) fn prepare_settings_update(request: &mut UpdateSettingsRequest) -> ApiResult<()> {
    // Runtime normalization can append provider presets, so it must happen
    // before provider validation and the enabled-URL preflight. Performing it
    // again after those checks could create unchecked persisted providers.
    request.settings = std::mem::take(&mut request.settings).normalized();
    request
        .settings
        .ai
        .normalize_and_validate_providers()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    if let Some(provider_secrets) = request.provider_secrets.take() {
        request.provider_secrets = Some(canonicalize_provider_secrets(
            &request.settings,
            provider_secrets,
        )?);
    }
    Ok(())
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty)
)]
pub(crate) async fn update_settings(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(mut request): Json<UpdateSettingsRequest>,
) -> ApiResult<Json<RuntimeSettings>> {
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));

    // Validate and canonicalize every name-based AI reference before the
    // first secret or settings write. Any failure therefore leaves both the
    // runtime settings and all encrypted-secret mappings untouched.
    prepare_settings_update(&mut request)?;
    let current_settings = get_runtime_settings(&state.pool).await?;
    let new_provider_secrets: HashSet<String> = request
        .provider_secrets
        .iter()
        .flatten()
        .filter(|(_, secret)| !secret.trim().is_empty())
        .map(|(name, _)| name.clone())
        .collect();
    validate_secret_bindings(
        &current_settings,
        &request.settings,
        &new_provider_secrets,
        request
            .paperless_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty()),
        request
            .notification_webhook_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty()),
    )?;

    // Config-time SSRF guard (SECURITY_DESIGN §4.3): outbound URLs are
    // validated when they are PERSISTED, not only when the operator happens
    // to press a "test" button — the worker consumes them verbatim on its
    // hot path without re-validating.
    let paperless_url = request.settings.paperless.base_url.trim();
    if !paperless_url.is_empty() {
        validate_outbound_url_for_save(paperless_url)
            .await
            .map_err(|error| {
                ApiError::bad_request(format!("Paperless base URL: {}", error.message))
            })?;
    }
    // public_url is rendered as a clickable link in the browser (not an
    // outbound server request), so it needs http/https scheme validation — not
    // the SSRF/dangerous-IP check (an intranet public_url is legitimate) — to
    // stop a settings admin planting a `javascript:` URL that executes for
    // lower-privileged users viewing the inventory. #290
    if let Some(public_url) = request.settings.paperless.public_url.as_deref() {
        let public_url = public_url.trim();
        if !public_url.is_empty() {
            let parsed = Url::parse(public_url)
                .map_err(|_| ApiError::bad_request("Paperless public URL is not a valid URL"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(ApiError::bad_request(
                    "Paperless public URL scheme must be http or https",
                ));
            }
        }
    }
    validate_cost_budget_settings(&request.settings.ui)?;
    // Disabled providers/profiles may carry placeholder URLs (the seeded
    // `openai-compatible` example points at localhost); they are not active
    // outbound targets, and enabling one is itself a settings save — the
    // guard fires then.
    for profile in &request.settings.paperless.archive_profiles {
        let base_url = profile.base_url.trim();
        if !profile.enabled || base_url.is_empty() {
            continue;
        }
        validate_outbound_url_for_save(base_url)
            .await
            .map_err(|error| {
                ApiError::bad_request(format!(
                    "archive profile '{}' base URL: {}",
                    profile.name, error.message
                ))
            })?;
    }
    for provider in &request.settings.ai.providers {
        if !provider.enabled {
            continue;
        }
        let base_url = if provider.name.eq_ignore_ascii_case("ollama") {
            request.settings.ai.ollama_base_url.trim()
        } else {
            provider.base_url.trim()
        };
        validate_outbound_url_for_save(base_url)
            .await
            .map_err(|error| {
                ApiError::bad_request(format!(
                    "AI provider '{}' base URL: {}",
                    provider.name, error.message
                ))
            })?;
    }
    if let Some(webhook_url) = request.notification_webhook_url.as_deref() {
        let webhook_url = webhook_url.trim();
        if !webhook_url.is_empty() {
            validate_outbound_url_for_save(webhook_url)
                .await
                .map_err(|error| {
                    ApiError::bad_request(format!("notification webhook URL: {}", error.message))
                })?;
        }
    }

    if let Some(token) = request
        .paperless_token
        .filter(|token| !token.trim().is_empty())
    {
        let secret_id = upsert_encrypted_secret(
            &state.pool,
            &state.config.secret_key,
            "paperless-api-token",
            &SecretString::from(token),
            actor_id,
        )
        .await?;
        request.settings.paperless.token_secret_id = Some(secret_id);
    }
    if let Some(provider_secrets) = request.provider_secrets.take() {
        for (provider_name, secret) in provider_secrets {
            if secret.trim().is_empty() {
                continue;
            }
            let secret_id = upsert_encrypted_secret(
                &state.pool,
                &state.config.secret_key,
                &format!("ai-provider-{provider_name}-api-key"),
                &SecretString::from(secret),
                actor_id,
            )
            .await?;
            if let Some(provider) = request
                .settings
                .ai
                .providers
                .iter_mut()
                .find(|provider| provider.name == provider_name)
            {
                provider.secret_id = Some(secret_id);
            }
        }
    }
    if let Some(webhook_url) = request
        .notification_webhook_url
        .take()
        .filter(|value| !value.trim().is_empty())
    {
        let secret_id = upsert_encrypted_secret(
            &state.pool,
            &state.config.secret_key,
            "notification-webhook-url",
            &SecretString::from(webhook_url),
            actor_id,
        )
        .await?;
        request.settings.notifications.webhook_url_secret_id = Some(secret_id);
    }
    // Capture the AI model identity before the save so we can detect a switch
    // and react to it below. A failed read just disables the optimization.
    let previous_models = get_runtime_settings(&state.pool)
        .await
        .ok()
        .map(|settings| {
            (
                settings.ai.default_provider,
                settings.ai.default_text_model,
                settings.ai.default_vision_model,
            )
        });

    // The preflight normalized the final provider inventory before any URL or
    // secret checks. Persist — and below, return — that same normalized object
    // so the PUT response, audit payload and next GET agree (#313).
    update_runtime_settings(&state.pool, &request.settings, actor_id).await?;
    info!(%actor_id, "runtime settings updated");

    // When the operator switches the AI model/provider, a backlog that an old
    // provider's cooldown parked (run_after pushed far into the future) would
    // otherwise keep waiting out that now-irrelevant cooldown. Operators expect
    // a model switch to take effect immediately, so drop the stale cooldowns
    // and wake the parked jobs to rerun under the new model right away. The
    // release is scoped to cooldown-parked jobs (run_after beyond the regular
    // retry-backoff horizon) so a model switch does not also collapse the
    // backoff+jitter spacing of unrelated transient retries (#313).
    let new_ai = &request.settings.ai;
    let model_changed = previous_models
        .as_ref()
        .is_some_and(|(provider, text, vision)| {
            provider != &new_ai.default_provider
                || text != &new_ai.default_text_model
                || vision != &new_ai.default_vision_model
        });
    if model_changed {
        let cleared = archivist_db::clear_all_provider_cooldowns(&state.pool)
            .await
            .unwrap_or_else(|error| {
                warn!(error = %error, "failed to clear cooldowns after model change");
                0
            });
        let released = archivist_db::release_cooldown_parked_retries(&state.pool)
            .await
            .unwrap_or_else(|error| {
                warn!(error = %error, "failed to release parked jobs after model change");
                0
            });
        info!(
            %actor_id,
            cleared,
            released,
            "AI model changed; cleared provider cooldowns and released cooldown-parked jobs"
        );
    }

    Ok(Json(request.settings))
}

pub(crate) async fn secret_references(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "items": list_secret_references(&state.pool).await? }),
    ))
}

pub(crate) async fn test_paperless(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let result = async {
        let settings = get_runtime_settings(&state.pool).await?;
        let (base_url, _) = settings.paperless.active_connection();
        if let Err(error) = validate_outbound_url(base_url).await {
            return Err(anyhow!("Paperless base URL rejected: {}", error.message));
        }
        let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
        client.test_connection().await
    }
    .await;
    match result {
        Ok(value) => Ok(Json(json!({ "ok": value.ok }))),
        Err(error) => Ok(Json(json!({ "ok": false, "error": error.to_string() }))),
    }
}

#[derive(Deserialize)]
pub(crate) struct TestProviderRequest {
    pub(crate) name: String,
    pub(crate) kind: AiProviderKind,
    pub(crate) base_url: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) tuning: ProviderTuning,
    pub(crate) secret_id: Option<Uuid>,
    pub(crate) secret: Option<String>,
}

pub(crate) async fn test_provider(
    State(state): State<AppState>,
    _auth: Authenticated,
    Json(request): Json<TestProviderRequest>,
) -> ApiResult<Json<Value>> {
    let settings = get_runtime_settings(&state.pool).await?;
    let provider = provider_test_target(&settings, &request)?;
    let secret = provider_test_secret(&state, &settings, &provider, request.secret).await;
    let response_secret = secret.as_ref().ok().and_then(|secret| secret.clone());
    let result = async {
        if let Err(error) = validate_outbound_url(&provider.base_url).await {
            return Err(anyhow!("AI provider base URL rejected: {}", error.message));
        }
        let secret = secret?;
        test_ai_provider(&provider, secret.clone()).await
    }
    .await;
    Ok(Json(provider_test_response(
        &provider,
        result,
        response_secret.as_ref(),
    )))
}

pub(crate) async fn test_notification(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let settings = get_runtime_settings(&state.pool).await?;
    let result = async {
        let webhook_url = notification_webhook_url(&state, &settings).await?;
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "notification.test",
                "severity": "info",
                "title": "Paperless Archivist notification test",
                "description": "Webhook delivery is configured. This payload contains no document content or secrets.",
                "metadata": {
                    "source": "settings-test"
                }
            }),
        )
        .await
    }
    .await;
    match result {
        Ok(()) => Ok(Json(json!({ "ok": true }))),
        Err(error) => Ok(Json(json!({ "ok": false, "error": error.to_string() }))),
    }
}

pub(crate) async fn notification_webhook_url(
    state: &AppState,
    settings: &RuntimeSettings,
) -> Result<String> {
    let secret_id = settings
        .notifications
        .webhook_url_secret_id
        .ok_or_else(|| anyhow!("Notification webhook URL is not configured"))?;
    let webhook_url = resolve_secret(&state.pool, &state.config.secret_key, secret_id)
        .await?
        .ok_or_else(|| anyhow!("Notification webhook secret reference does not exist"))?;
    validate_outbound_url(webhook_url.expose_secret())
        .await
        .map_err(|error| anyhow!(error.message))?;
    Ok(webhook_url.expose_secret().to_owned())
}

pub(crate) async fn send_notification_webhook(webhook_url: &str, payload: Value) -> Result<()> {
    let response = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(10))
        // Refuse redirects so a 3xx to an internal address (e.g. IMDS at
        // 169.254.169.254 / loopback) can't bypass the SSRF guard that only
        // validated the originally supplied URL.
        .redirect(reqwest::redirect::Policy::none())
        // NB: the caller is expected to have run `validate_outbound_url` first.
        // There is no connect-time IP-pinning (see that fn's docs / #183 for
        // why the resolver was reverted); the DNS-rebinding TOCTOU is an
        // accepted residual risk for these operator-configured targets.
        .build()?
        .post(webhook_url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "Notification webhook request failed: {}",
                error.without_url()
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("Notification webhook returned {status}"));
    }
    Ok(())
}
