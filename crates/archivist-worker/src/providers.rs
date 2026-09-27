//! AI provider selection, chat/vision clients, vision-model fallback and model
//! discovery.

use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow};
use archivist_ai::{
    AiProviderError, AiResponse, AnthropicClient, ChatRequest, MineruClient, OllamaClient,
    OpenAiCompatibleClient, TextProvider, VisionProvider, VisionRequest,
};
use archivist_config::AppConfig;
use archivist_core::{
    AiProviderKind, AuditEventInput, ReasoningEffort, RuntimeSettings, Stage, StructuredOutputMode,
};
use archivist_db::{DbPool, JobRecord, append_audit, resolve_secret};
use secrecy::SecretString;
use serde_json::json;
use tracing::{info, warn};
use uuid::Uuid;

use crate::lease::{job_lease_seconds, run_after_lease_renewal};

pub(crate) fn provider_name_for_stage(settings: &RuntimeSettings, stage: Stage) -> Result<String> {
    let provider = provider_for_stage(settings, stage, false)?;
    Ok(provider.name)
}

/// Detect Ollama vision runtime crashes (GGML_ASSERT, llama runner aborts). These keep their
/// `Transient` classification — a different page input might still succeed — but we surface
/// the signal in worker logs so operators can swap the configured vision model rather than
/// burning attempts on a misconfigured runtime.
pub(crate) fn is_vision_model_runtime_crash(error: &anyhow::Error) -> bool {
    if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<AiProviderError>(),
            Some(AiProviderError::RunnerUnavailable(_))
        )
    }) {
        return true;
    }
    let message = error
        .chain()
        .map(|cause| cause.to_string().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    message.contains("ggml_assert")
        || message.contains("runner process no longer running")
        || message.contains("signal arrived during cgo execution")
}

/// Hardcoded safe-default chain walked when the primary vision model crashes and no explicit
/// `fallback_vision_model` is configured. Order matters — the worker picks the first entry
/// that is installed locally and not equal to the current primary. These names match the
/// public Ollama tags as of 2025; nothing experimental is included on purpose. If an entry
/// becomes unsafe (e.g. a tag is retracted) drop it here rather than relying on operators.
const VISION_FALLBACK_CHAIN: &[&str] = &[
    // Smaller-than-the-primary fallbacks first — these have been the actual
    // workhorses in production deployments and tend to be installed alongside
    // glm-ocr / qwen3-vl primaries. Adding them as auto-discovery candidates
    // lets the runtime fallback path fire without operators having to set
    // `ai.fallback_vision_model` explicitly.
    "qwen2.5vl:7b",
    "qwen2-vl:7b",
    "qwen3-vl:32b",
    "llava-llama3:8b",
    "llava:13b",
    "llava:latest",
];

/// Where a fallback candidate came from. Carried into log lines and audit metadata so
/// operators can tell whether the recovery used their explicit setting or the safe-default
/// chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisionFallbackSource {
    Explicit,
    AutoDiscovered,
}

impl VisionFallbackSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            VisionFallbackSource::Explicit => "explicit",
            VisionFallbackSource::AutoDiscovered => "auto_discovered",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisionFallbackChoice {
    model: String,
    source: VisionFallbackSource,
}

/// Pure selector for a vision-model fallback. Prefers the explicit setting when it is
/// set, non-empty, and not the same as the primary model. Otherwise walks
/// `VISION_FALLBACK_CHAIN` and picks the first entry that is in `installed_models` and
/// not equal to the primary. Case-insensitive match on model names.
///
/// `installed_models` may be empty (e.g. when the provider is not Ollama or the tag list
/// call failed) — in that case the chain cannot be walked and the function returns the
/// explicit choice if any, or `None`.
fn pick_vision_fallback_model(
    settings: &RuntimeSettings,
    primary_model: &str,
    installed_models: &[String],
) -> Option<VisionFallbackChoice> {
    if let Some(explicit) = settings
        .ai
        .fallback_vision_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.eq_ignore_ascii_case(primary_model))
    {
        return Some(VisionFallbackChoice {
            model: explicit.to_owned(),
            source: VisionFallbackSource::Explicit,
        });
    }

    let installed_lower: Vec<String> = installed_models
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    for candidate in VISION_FALLBACK_CHAIN {
        if candidate.eq_ignore_ascii_case(primary_model) {
            continue;
        }
        let candidate_lower = candidate.to_ascii_lowercase();
        if installed_lower.iter().any(|name| name == &candidate_lower) {
            return Some(VisionFallbackChoice {
                model: (*candidate).to_owned(),
                source: VisionFallbackSource::AutoDiscovered,
            });
        }
    }
    None
}

/// Best-effort fetch of locally-installed Ollama models for the given provider. Returns
/// an empty list (with a warn-level log) when the provider is not Ollama or the tag list
/// call fails — that downgrades the auto-discovered fallback path to a no-op without
/// crashing the worker tick.
async fn installed_ollama_models_for_provider(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
) -> Vec<String> {
    if provider.kind != AiProviderKind::Ollama {
        return Vec::new();
    }
    let secret = match provider_secret(pool, config, provider).await {
        Ok(secret) => secret,
        Err(error) => {
            warn!(
                error = %error,
                provider = %provider.name,
                "fallback chain skipped: could not resolve provider secret"
            );
            return Vec::new();
        }
    };
    let client = match OllamaClient::new_with_timeout(
        &provider.name,
        &provider.base_url,
        secret,
        ollama_discovery_timeout(provider),
    ) {
        Ok(client) => client,
        Err(error) => {
            warn!(
                error = %error,
                provider = %provider.name,
                "fallback chain skipped: could not construct Ollama client"
            );
            return Vec::new();
        }
    };
    match client.list_models().await {
        Ok(models) => models.into_iter().map(|model| model.name).collect(),
        Err(error) => {
            warn!(
                error = %error,
                provider = %provider.name,
                "fallback chain skipped: Ollama tag list call failed"
            );
            Vec::new()
        }
    }
}

fn ollama_discovery_timeout(provider: &StageProvider) -> Duration {
    Duration::from_secs(u64::from(provider.request_timeout_seconds))
}

/// Run a single vision request, transparently retrying on a vision-runtime-crash error
/// against a configured or auto-discovered fallback model. The return value carries the
/// model that actually produced the response so the caller can record the swap in
/// per-page logs / audit metadata.
///
/// Behaviour:
/// 1. Renew the owner-scoped job lease, then call the primary provider/model.
/// 2. On success, return immediately.
/// 3. On a runtime-crash error, renew before Ollama model discovery, choose a fallback,
///    renew again, emit the audit event, and retry the exact request once.
/// 4. If the fallback also fails, or no fallback is available, return the original error.
/// 5. If any renewal reports that ownership was lost, return `Ok(None)` before polling
///    the following provider future. The OCR caller then exits without cache/apply writes.
///
/// This function does NOT consume the job's attempt slot — both calls happen within the
/// same worker tick. Each high-level network call is independently bounded by the provider
/// timeout and covered by a fresh lease window. The orchestrator-driven retry budget only
/// kicks in if the fallback also fails (transient classification keeps current
/// retry+jitter behaviour intact).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_vision_with_fallback(
    pool: &DbPool,
    config: &AppConfig,
    client: &VisionClient,
    provider: &StageProvider,
    settings: &RuntimeSettings,
    job: &JobRecord,
    lease_owner: &str,
    page_index: usize,
    request: VisionRequest,
) -> Result<Option<(AiResponse, String, bool)>> {
    let lease_seconds = job_lease_seconds(settings);
    let mut renew_lease = || archivist_db::bump_job_lease(pool, job.id, lease_owner, lease_seconds);
    run_vision_with_fallback_with_lease_renewal(
        pool,
        config,
        client,
        provider,
        settings,
        job,
        page_index,
        request,
        &mut renew_lease,
    )
    .await
}

/// Orchestrates the production vision/fallback path with an injectable lease
/// renewal operation. Production supplies the owner-scoped database bump;
/// tests supply a scripted renewal while exercising the real HTTP clients.
#[allow(clippy::too_many_arguments)]
async fn run_vision_with_fallback_with_lease_renewal<RenewLease, RenewalFuture>(
    pool: &DbPool,
    config: &AppConfig,
    client: &VisionClient,
    provider: &StageProvider,
    settings: &RuntimeSettings,
    job: &JobRecord,
    page_index: usize,
    request: VisionRequest,
    renew_lease: &mut RenewLease,
) -> Result<Option<(AiResponse, String, bool)>>
where
    RenewLease: FnMut() -> RenewalFuture,
    RenewalFuture: Future<Output = Result<bool>>,
{
    let primary_model = provider.model.clone();
    let mut request_with_primary = request.clone();
    request_with_primary.model = primary_model.clone();
    let Some(primary_result) =
        run_after_lease_renewal(renew_lease(), client.vision(request_with_primary)).await?
    else {
        warn!(
            job_id = %job.id,
            document_id = job.paperless_document_id,
            page_index,
            vision_phase = "primary",
            "OCR lease lost before vision call; stopping before provider request"
        );
        return Ok(None);
    };
    match primary_result {
        Ok(response) => Ok(Some((response, primary_model, false))),
        Err(error) => {
            if !is_vision_model_runtime_crash(&error) {
                return Err(error);
            }
            let Some(installed) = run_after_lease_renewal(
                renew_lease(),
                installed_ollama_models_for_provider(pool, config, provider),
            )
            .await?
            else {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    page_index,
                    vision_phase = "model_discovery",
                    "OCR lease lost after primary failure; stopping before model discovery"
                );
                return Ok(None);
            };
            let Some(choice) = pick_vision_fallback_model(settings, &primary_model, &installed)
            else {
                return Err(error);
            };

            warn!(
                primary_model = %primary_model,
                fallback_model = %choice.model,
                fallback_source = choice.source.as_str(),
                page_index,
                document_id = job.paperless_document_id,
                stage = %job.stage,
                "vision model crashed; selected fallback and renewing lease before retry"
            );
            let mut fallback_request = request;
            fallback_request.model = choice.model.clone();
            let Some(fallback_result) =
                run_after_lease_renewal(renew_lease(), client.vision(fallback_request)).await?
            else {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    page_index,
                    vision_phase = "fallback",
                    "OCR lease lost after model discovery; stopping before vision fallback"
                );
                return Ok(None);
            };
            let response = fallback_result?;
            let auto_discovered = choice.source == VisionFallbackSource::AutoDiscovered;
            let audit_metadata = json!({
                "primary": primary_model,
                "fallback": choice.model,
                "fallback_source": choice.source.as_str(),
                "auto_discovered_fallback": auto_discovered,
                "stage": job.stage,
                "page_index": page_index,
                "document_id": job.paperless_document_id,
                "primary_error": error.to_string()
            });
            if let Err(audit_error) = append_audit(
                pool,
                AuditEventInput {
                    event_type: "worker.vision_model_fallback".to_owned(),
                    actor_type: "worker".to_owned(),
                    actor_id: None,
                    run_id: Some(job.run_id),
                    job_id: Some(job.id),
                    paperless_document_id: Some(job.paperless_document_id),
                    before: None,
                    after: None,
                    metadata: Some(audit_metadata),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await
            {
                warn!(error = %audit_error, "failed to record worker.vision_model_fallback audit event");
            }
            info!(
                primary_model = %primary_model,
                fallback_model = %choice.model,
                fallback_source = choice.source.as_str(),
                page_index,
                vision_model_fallback_used = true,
                document_id = job.paperless_document_id,
                stage = %job.stage,
                "vision fallback succeeded"
            );
            Ok(Some((response, choice.model, true)))
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StageProvider {
    pub(crate) name: String,
    pub(crate) kind: AiProviderKind,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) secret_id: Option<Uuid>,
    pub(crate) reasoning_effort: ReasoningEffort,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) structured_output: StructuredOutputMode,
    pub(crate) request_timeout_seconds: u32,
}

pub(crate) fn provider_for_stage(
    settings: &RuntimeSettings,
    stage: Stage,
    vision: bool,
) -> Result<StageProvider> {
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
        .ok_or_else(|| anyhow!("AI provider '{provider_name}' is not configured or disabled"))?;
    if provider.name.eq_ignore_ascii_case("ollama") {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings
        .ai
        .model_for_stage_provider(&provider, stage, vision);
    let base_url = provider_base_url(&provider.name, &provider.base_url)?;
    let reasoning_effort = provider.tuning.reasoning_effort.unwrap_or_default();
    let max_output_tokens = provider
        .tuning
        .max_output_tokens
        .filter(|tokens| *tokens > 0);
    let structured_output = provider.tuning.structured_output.unwrap_or_default();
    // Per-request AI timeout: a 0/unset value inherits the built-in default.
    let request_timeout_seconds = provider
        .tuning
        .request_timeout_seconds
        .filter(|secs| *secs > 0)
        .unwrap_or(archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS);
    Ok(StageProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
        reasoning_effort,
        max_output_tokens,
        structured_output,
        request_timeout_seconds,
    })
}

fn provider_base_url(provider_name: &str, configured: &str) -> Result<String> {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "AI provider '{provider_name}' has an empty base URL; repair the runtime settings"
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

/// v1.5.15 (#119) experiment-aware active-prompt loader. Picks the A or B
/// variant deterministically by `run_id`, falls
/// back to the experiment-group-less default. Returns
/// `(prompt_id, experiment_label)` so the caller can stamp the label
/// into the normalized output for downstream accuracy analysis.
pub(crate) async fn apply_active_prompt_with_experiment(
    pool: &DbPool,
    stage: Stage,
    run_id: Uuid,
    request: &mut ChatRequest,
) -> Result<(Option<Uuid>, Option<String>)> {
    let Some((prompt, label)) =
        archivist_db::get_active_prompt_with_experiment(pool, stage, run_id).await?
    else {
        return Ok((None, None));
    };
    request.system_prompt = prompt.content;
    Ok((Some(prompt.id), label))
}

/// #445: use the prompt version a review retry pinned instead of the active
/// one. Returns `None` (caller falls back to the active prompt) when no
/// version was pinned or it no longer exists / is not a metadata prompt.
pub(crate) async fn apply_retry_prompt(
    pool: &DbPool,
    prompt_id: Option<Uuid>,
    request: &mut ChatRequest,
) -> Result<Option<Uuid>> {
    let Some(prompt_id) = prompt_id else {
        return Ok(None);
    };
    match archivist_db::get_prompt_by_id(pool, prompt_id).await? {
        Some(prompt) if prompt.stage == Stage::Metadata => {
            request.system_prompt = prompt.content;
            Ok(Some(prompt.id))
        }
        _ => {
            warn!(%prompt_id, "retry prompt version is missing or not a metadata prompt; using the active prompt");
            Ok(None)
        }
    }
}

/// Cheap one-shot LLM pre-pass that classifies the document into one of
/// the `DocTypeCategory` values. Used to pick a doc-type-specific hint
/// snippet for the main metadata prompt (v1.5.13, Bundle C of milestone
/// v1.6.0).
///
/// Reuses the metadata stage's provider+model so operators don't have to
/// configure a separate classifier endpoint. Returns
/// `DocTypeCategory::Other` on empty content or any classifier error so
/// the main pipeline keeps draining; the caller logs the error.
pub(crate) async fn classify_document_type(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
    content: &str,
) -> Result<archivist_ai::DocTypeCategory> {
    if content.trim().is_empty() {
        return Ok(archivist_ai::DocTypeCategory::Other);
    }
    let request = archivist_ai::prompt_for_doc_type_classify(content);
    let response = chat_for_stage(pool, config, settings, Stage::Metadata, request).await?;
    Ok(archivist_ai::DocTypeCategory::parse(&response.text))
}

pub(crate) async fn chat_for_stage(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
    stage: Stage,
    mut request: ChatRequest,
) -> Result<AiResponse> {
    let provider = provider_for_stage(settings, stage, false)?;
    request.model = provider.model.clone();
    // Local-runner context window: only applies to Ollama. Remote providers
    // (OpenAI / Anthropic / OpenAI-compatible) ignore the field — see
    // `build_ollama_chat_payload`. Floored at the point of use so a
    // too-small per-provider override can't truncate the metadata prompts.
    request.num_ctx =
        ollama_text_num_ctx_for_provider(&provider, settings.effective_tuning().text_num_ctx);
    request.reasoning_effort = Some(provider.reasoning_effort);
    request.max_output_tokens = provider.max_output_tokens;
    request.structured_output = Some(provider.structured_output);
    chat_with_provider(pool, config, &provider, request).await
}

/// Returns `Some(num_ctx)` when the provider is the local Ollama runner AND
/// a value is configured, else `None`. Wrapping the lookup keeps the call
/// sites symmetrical between the vision and chat paths and ensures we never
/// push the override onto remote providers (which would either ignore it or
/// reject the field).
fn ollama_num_ctx_for_provider(provider: &StageProvider, configured: Option<i64>) -> Option<i64> {
    match provider.kind {
        AiProviderKind::Ollama => configured,
        _ => None,
    }
}

/// Minimum Ollama text `num_ctx`: metadata prompts embed up to 16k chars of
/// document content plus the candidate correspondent/type/tag allowlists,
/// few-shots, and the JSON shape, which on a long document exceed even a
/// 16384-token window and fail with `exceed_context_size_error` (observed in
/// production at 18962 tokens; the original v1.12.2 incident was at 4096). The
/// startup bump only raises the GLOBAL `ai.ollama_text_num_ctx`; a per-provider
/// tuning override wins over the global in resolution and would smuggle a
/// too-small value through, so floor it at the point of use. 32768 matches the
/// effective vision num_ctx. #304
const OLLAMA_TEXT_NUM_CTX_FLOOR: i64 = 32768;

/// Resolve the Ollama text `num_ctx`, never returning a value below the
/// prompt-safe floor.
pub(crate) fn ollama_text_num_ctx_for_provider(
    provider: &StageProvider,
    configured: Option<i64>,
) -> Option<i64> {
    ollama_num_ctx_for_provider(provider, configured).map(|n| n.max(OLLAMA_TEXT_NUM_CTX_FLOOR))
}

/// Minimum Ollama vision `num_ctx`: below this, glm-ocr-class models crash the
/// runtime (GGML_ASSERT). The startup bump only raises the GLOBAL
/// `ai.ollama_vision_num_ctx`; a per-provider tuning override (e.g. the
/// local-Ollama preset pins 4096) wins over the global in resolution and would
/// smuggle a too-small value through, so floor it at the point of use. #293
const OLLAMA_VISION_NUM_CTX_FLOOR: i64 = 16384;

/// Resolve the Ollama vision `num_ctx`, never returning a value below the
/// GGML-safe floor.
pub(crate) fn ollama_vision_num_ctx_for_provider(
    provider: &StageProvider,
    configured: Option<i64>,
) -> Option<i64> {
    ollama_num_ctx_for_provider(provider, configured).map(|n| n.max(OLLAMA_VISION_NUM_CTX_FLOOR))
}

pub(crate) async fn chat_with_provider(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
    request: ChatRequest,
) -> Result<AiResponse> {
    let timeout = Duration::from_secs(u64::from(provider.request_timeout_seconds));
    match provider.kind {
        AiProviderKind::Ollama => {
            let client = OllamaClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Anthropic => {
            let secret = provider_secret(pool, config, provider)
                .await?
                .ok_or_else(|| {
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

/// A vision client built ONCE per OCR job and reused across every page (and
/// the crash fallback). Previously the worker constructed a brand-new reqwest
/// client and re-resolved+decrypted the provider secret (Postgres roundtrip +
/// AES-256-GCM) on every page — discarding the connection pool / TLS session
/// each time even though the provider and secret are fixed for the document.
/// Holding the typed client keeps the keep-alive pool and TLS session warm
/// across pages. The fallback only swaps the model (carried on the request),
/// not the provider, so a single client covers primary and fallback.
pub(crate) enum VisionClient {
    Ollama(OllamaClient),
    OpenAiCompatible(OpenAiCompatibleClient),
    Anthropic(AnthropicClient),
    Mineru(MineruClient),
}

impl VisionClient {
    async fn vision(&self, request: VisionRequest) -> Result<AiResponse> {
        match self {
            VisionClient::Ollama(client) => client.vision(request).await,
            VisionClient::OpenAiCompatible(client) => client.vision(request).await,
            VisionClient::Anthropic(client) => client.vision(request).await,
            VisionClient::Mineru(client) => client.vision(request).await,
        }
    }
}

pub(crate) async fn build_vision_client(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
) -> Result<VisionClient> {
    let timeout = Duration::from_secs(u64::from(provider.request_timeout_seconds));
    match provider.kind {
        AiProviderKind::Ollama => Ok(VisionClient::Ollama(OllamaClient::new_with_timeout(
            &provider.name,
            &provider.base_url,
            provider_secret(pool, config, provider).await?,
            timeout,
        )?)),
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => Ok(
            VisionClient::OpenAiCompatible(OpenAiCompatibleClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
                timeout,
            )?),
        ),
        AiProviderKind::Anthropic => {
            let secret = provider_secret(pool, config, provider)
                .await?
                .ok_or_else(|| {
                    anyhow!("AI provider '{}' requires an API key secret", provider.name)
                })?;
            Ok(VisionClient::Anthropic(AnthropicClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?))
        }
        AiProviderKind::Mineru => Ok(VisionClient::Mineru(MineruClient::new_with_timeout(
            &provider.name,
            &provider.base_url,
            provider_secret(pool, config, provider).await?,
            timeout,
        )?)),
    }
}

async fn provider_secret(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
) -> Result<Option<SecretString>> {
    let Some(secret_id) = provider.secret_id else {
        return Ok(None);
    };
    resolve_secret(pool, &config.secret_key, secret_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use tokio::time::sleep;

    use crate::failure::{ProcessingFailureClass, classify_processing_failure};
    use crate::test_support::{test_app_config, vision_test_job};

    #[derive(Clone)]
    struct MockOllamaState {
        chat_calls: Arc<AtomicU32>,
        tag_calls: Arc<AtomicU32>,
        tag_delay: Duration,
    }

    async fn mock_ollama_chat(State(state): State<MockOllamaState>) -> Response {
        let call = state.chat_calls.fetch_add(1, Ordering::SeqCst);
        sleep(Duration::from_millis(10)).await;
        if call == 0 {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "runner process no longer running",
            )
                .into_response();
        }
        Json(json!({"message": {"content": "fallback response"}})).into_response()
    }

    async fn mock_ollama_tags(State(state): State<MockOllamaState>) -> Response {
        state.tag_calls.fetch_add(1, Ordering::SeqCst);
        sleep(state.tag_delay).await;
        Json(json!({"models": [{"name": "qwen2.5vl:7b"}]})).into_response()
    }

    async fn spawn_mock_ollama(
        tag_delay: Duration,
    ) -> (String, MockOllamaState, tokio::task::JoinHandle<()>) {
        let state = MockOllamaState {
            chat_calls: Arc::new(AtomicU32::new(0)),
            tag_calls: Arc::new(AtomicU32::new(0)),
            tag_delay,
        };
        let app = Router::new()
            .route("/api/chat", post(mock_ollama_chat))
            .route("/api/tags", get(mock_ollama_tags))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock Ollama");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock Ollama");
        });
        (base_url, state, task)
    }

    #[test]
    fn provider_base_url_rejects_empty_legacy_configuration() {
        let error = provider_base_url("mineru", "")
            .expect_err("corrupt settings must not silently target localhost");
        assert!(error.to_string().contains("empty base URL"));
        assert!(error.to_string().contains("mineru"));
        assert_eq!(
            provider_base_url("mineru", "http://omega:8001/").unwrap(),
            "http://omega:8001"
        );
    }

    fn stage_provider(kind: AiProviderKind) -> StageProvider {
        StageProvider {
            name: "p".to_owned(),
            kind,
            base_url: "http://x".to_owned(),
            model: "m".to_owned(),
            secret_id: None,
            reasoning_effort: ReasoningEffort::default(),
            max_output_tokens: None,
            structured_output: StructuredOutputMode::default(),
            request_timeout_seconds: archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS,
        }
    }

    #[test]
    fn ollama_vision_num_ctx_floors_below_ggml_minimum() {
        let ollama = stage_provider(AiProviderKind::Ollama);
        // A preset pinning 4096 is floored up to the GGML-safe minimum.
        assert_eq!(
            ollama_vision_num_ctx_for_provider(&ollama, Some(4096)),
            Some(OLLAMA_VISION_NUM_CTX_FLOOR)
        );
        // A value already at/above the floor passes through.
        assert_eq!(
            ollama_vision_num_ctx_for_provider(&ollama, Some(32768)),
            Some(32768)
        );
        // None stays None (use the client default); non-Ollama is always None.
        assert_eq!(ollama_vision_num_ctx_for_provider(&ollama, None), None);
        assert_eq!(
            ollama_vision_num_ctx_for_provider(&stage_provider(AiProviderKind::Openai), Some(4096)),
            None
        );
    }

    #[test]
    fn ollama_text_num_ctx_floors_below_prompt_minimum() {
        // A per-provider override pinning 4096 (the pre-#304 Ollama preset)
        // must be floored at the point of use, mirroring the vision path —
        // `resolve_tuning` prefers the provider value over the bumped global.
        let ollama = stage_provider(AiProviderKind::Ollama);
        assert_eq!(
            ollama_text_num_ctx_for_provider(&ollama, Some(4096)),
            Some(OLLAMA_TEXT_NUM_CTX_FLOOR)
        );
        // A value already at/above the floor passes through.
        assert_eq!(
            ollama_text_num_ctx_for_provider(&ollama, Some(32768)),
            Some(32768)
        );
        // None stays None (use the client default); non-Ollama is always None.
        assert_eq!(ollama_text_num_ctx_for_provider(&ollama, None), None);
        assert_eq!(
            ollama_text_num_ctx_for_provider(
                &stage_provider(AiProviderKind::Anthropic),
                Some(4096)
            ),
            None
        );
    }

    #[test]
    fn detects_ollama_vision_runtime_crashes() {
        // Real-world payloads observed from Ollama when the llama runner aborts on a vision
        // input. All three should trip the operator hint, even though the classifier still
        // marks them transient (retry on a different page may still succeed).
        // Real-world Ollama crash payloads always come wrapped in a 500-internal-server-error
        // envelope, which the classifier reads as Transient. We assert the combined detect +
        // retry behaviour on the wrapped form, plus the bare "signal arrived during cgo
        // execution" string for the detector alone (used in stack traces that bypass the HTTP
        // envelope, e.g. in tests that feed the runtime crash directly).
        let crash_cases = [
            anyhow!(
                "Ollama vision returned 500 Internal Server Error: GGML_ASSERT(a->ne[2] * 4 == b->ne[0]) failed"
            ),
            anyhow!(
                "Ollama vision returned 500 Internal Server Error: llama runner process no longer running: 2"
            ),
        ];
        for error in crash_cases {
            assert!(is_vision_model_runtime_crash(&error), "case: {error:?}");
            assert_eq!(
                classify_processing_failure(&error),
                ProcessingFailureClass::Transient,
                "crash should still retry: {error:?}"
            );
        }
        assert!(is_vision_model_runtime_crash(&anyhow!(
            "signal arrived during cgo execution"
        )));

        // Regular transient errors must NOT trip the vision-crash hint — that would mislead
        // operators into swapping a healthy model when the actual cause is networking.
        let non_crash_cases = [
            anyhow!("Paperless request timed out while downloading original"),
            anyhow!("PostgreSQL database pool timed out while claiming jobs"),
        ];
        for error in non_crash_cases {
            assert!(!is_vision_model_runtime_crash(&error), "case: {error:?}");
        }
    }

    #[test]
    fn vision_fallback_prefers_explicit_setting_when_different_from_primary() {
        let mut settings = RuntimeSettings::default();
        settings.ai.fallback_vision_model = Some("llava:13b".to_owned());
        let choice = pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &[]).unwrap();
        assert_eq!(choice.model, "llava:13b");
        assert_eq!(choice.source, VisionFallbackSource::Explicit);
    }

    #[test]
    fn vision_fallback_ignores_explicit_setting_that_equals_primary() {
        let mut settings = RuntimeSettings::default();
        settings.ai.fallback_vision_model = Some("QWEN2.5VL:7B".to_owned());
        // Same model (case-insensitive) → don't use it; fall through to chain. With no
        // installed models in the test list, the chain cannot be walked either.
        assert!(pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &[]).is_none());
    }

    #[test]
    fn vision_fallback_walks_safe_default_chain_when_no_explicit_setting() {
        let settings = RuntimeSettings::default();
        let installed = vec![
            "llava-llama3:8b".to_owned(),
            "qwen3:8b".to_owned(),
            "llava:13b".to_owned(),
        ];
        // Chain order: qwen2-vl:7b (not installed), llava-llama3:8b (installed) → picked.
        let choice = pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &installed).unwrap();
        assert_eq!(choice.model, "llava-llama3:8b");
        assert_eq!(choice.source, VisionFallbackSource::AutoDiscovered);
    }

    #[test]
    fn vision_fallback_safe_default_skips_primary_even_if_installed() {
        let settings = RuntimeSettings::default();
        // Primary IS in the chain; auto-discovery must skip it and pick the next entry.
        let installed = vec!["llava:13b".to_owned(), "llava-llama3:8b".to_owned()];
        let choice = pick_vision_fallback_model(&settings, "llava-llama3:8b", &installed).unwrap();
        assert_eq!(choice.model, "llava:13b");
        assert_eq!(choice.source, VisionFallbackSource::AutoDiscovered);
    }

    #[test]
    fn vision_fallback_returns_none_when_chain_has_no_installed_match() {
        let settings = RuntimeSettings::default();
        // No installed models from the chain → no fallback possible.
        let installed = vec!["qwen3:8b".to_owned(), "phi3:mini".to_owned()];
        assert!(pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &installed).is_none());
    }

    #[test]
    fn vision_fallback_returns_none_when_no_explicit_and_no_installed() {
        let settings = RuntimeSettings::default();
        assert!(pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &[]).is_none());
    }

    #[test]
    fn vision_fallback_explicit_trims_whitespace_and_skips_empty() {
        let mut settings = RuntimeSettings::default();
        settings.ai.fallback_vision_model = Some("   ".to_owned());
        // Whitespace-only explicit is treated as unset.
        assert!(pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &[]).is_none());
    }

    fn vision_test_pool() -> DbPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(20))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("build lazy test pool")
    }

    async fn assert_production_vision_fencing(
        lease_results: Vec<bool>,
        expected_chat_calls: u32,
        expected_tag_calls: u32,
        expect_fallback_success: bool,
    ) {
        let (base_url, state, server) = spawn_mock_ollama(Duration::from_millis(10)).await;
        let pool = vision_test_pool();
        let config = test_app_config();
        let mut provider = stage_provider(AiProviderKind::Ollama);
        provider.name = "mock-ollama".to_owned();
        provider.base_url = base_url;
        provider.model = "primary-model".to_owned();
        provider.request_timeout_seconds = 2;
        let client = VisionClient::Ollama(
            OllamaClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                None,
                Duration::from_secs(2),
            )
            .unwrap(),
        );
        let settings = RuntimeSettings::default();
        let job = vision_test_job();
        let request = VisionRequest {
            model: provider.model.clone(),
            temperature: 0.0,
            num_ctx: Some(OLLAMA_VISION_NUM_CTX_FLOOR),
            reasoning_effort: None,
            max_output_tokens: None,
            prompt: "synthetic page".to_owned(),
            images: Vec::new(),
        };
        let lease_results = Arc::new(lease_results);
        let lease_index = Arc::new(AtomicU32::new(0));
        let results_for_renewal = Arc::clone(&lease_results);
        let index_for_renewal = Arc::clone(&lease_index);
        let mut renew_lease = move || {
            let results = Arc::clone(&results_for_renewal);
            let index = Arc::clone(&index_for_renewal);
            async move {
                let index = index.fetch_add(1, Ordering::SeqCst) as usize;
                Ok::<bool, anyhow::Error>(results.get(index).copied().unwrap_or(false))
            }
        };

        let outcome = run_vision_with_fallback_with_lease_renewal(
            &pool,
            &config,
            &client,
            &provider,
            &settings,
            &job,
            0,
            request,
            &mut renew_lease,
        )
        .await
        .unwrap();

        if expect_fallback_success {
            let (response, model, fallback_used) =
                outcome.expect("three successful renewals must run the fallback");
            assert_eq!(response.text, "fallback response");
            assert_eq!(model, "qwen2.5vl:7b");
            assert!(fallback_used);
        } else {
            assert!(outcome.is_none(), "lease loss must stop OCR cleanly");
        }
        assert_eq!(
            lease_index.load(Ordering::SeqCst) as usize,
            lease_results.len()
        );
        assert_eq!(state.chat_calls.load(Ordering::SeqCst), expected_chat_calls);
        assert_eq!(state.tag_calls.load(Ordering::SeqCst), expected_tag_calls);
        server.abort();
    }

    #[tokio::test]
    async fn production_vision_path_fences_primary_discovery_and_fallback_calls() {
        // Lost before primary: no provider request at all.
        assert_production_vision_fencing(vec![false], 0, 0, false).await;
        // Slow primary crashes, then ownership is lost: discovery is never called.
        assert_production_vision_fencing(vec![true, false], 1, 0, false).await;
        // Primary crashes and slow discovery succeeds, then ownership is lost:
        // the selected fallback is never called and no success audit can run.
        assert_production_vision_fencing(vec![true, true, false], 1, 1, false).await;
        // All three renewals succeed: the real second /api/chat request runs,
        // its response/model are returned, and only then can the best-effort
        // success audit be attempted.
        assert_production_vision_fencing(vec![true, true, true], 2, 1, true).await;
    }

    #[tokio::test]
    async fn ollama_discovery_client_enforces_resolved_provider_timeout_on_wire() {
        let (base_url, state, server) = spawn_mock_ollama(Duration::from_millis(1_200)).await;
        let pool = vision_test_pool();
        let config = test_app_config();
        let mut provider = stage_provider(AiProviderKind::Ollama);
        provider.base_url = base_url;
        provider.request_timeout_seconds = 1;

        let models = installed_ollama_models_for_provider(&pool, &config, &provider).await;

        assert!(
            models.is_empty(),
            "the 1s provider timeout must cancel a 1.2s /api/tags response"
        );
        assert_eq!(state.tag_calls.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[test]
    fn provider_for_stage_carries_max_output_tokens_and_structured_output() {
        let mut settings = RuntimeSettings::default();
        settings.ai.ensure_default_providers();
        settings.ai.default_provider = "openai-compatible".to_owned();
        let provider = settings
            .ai
            .providers
            .iter_mut()
            .find(|provider| provider.name == "openai-compatible")
            .expect("preset exists");
        provider.enabled = true;
        provider.tuning.max_output_tokens = Some(8192);
        provider.tuning.structured_output = Some(StructuredOutputMode::JsonObject);

        let resolved =
            provider_for_stage(&settings, Stage::Metadata, false).expect("provider resolves");
        assert_eq!(resolved.max_output_tokens, Some(8192));
        assert_eq!(resolved.structured_output, StructuredOutputMode::JsonObject);
    }

    #[test]
    fn provider_for_stage_uses_mineru_override_for_ocr() {
        let mut settings = RuntimeSettings::default();
        settings.ai.ensure_default_providers();
        let provider = settings
            .ai
            .providers
            .iter_mut()
            .find(|provider| provider.name == "mineru")
            .expect("mineru preset exists");
        provider.enabled = true;
        settings
            .ai
            .stage_models
            .push(archivist_core::StageModelOverride {
                stage: Stage::Ocr,
                provider: "mineru".to_owned(),
                model: "mineru".to_owned(),
            });

        let resolved = provider_for_stage(&settings, Stage::Ocr, true).expect("provider resolves");
        assert_eq!(resolved.kind, AiProviderKind::Mineru);
        assert_eq!(resolved.model, "mineru");
        assert_eq!(resolved.base_url, "http://localhost:8001");
    }
}
