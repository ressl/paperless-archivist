use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use archivist_ai::{
    AiProviderError, AiResponse, AnthropicClient, ChatRequest, DEFAULT_OCR_SYSTEM_PROMPT,
    ImageInput, OllamaClient, OpenAiCompatibleClient, PromptLanguageContext, TextProvider,
    VisionProvider, VisionRequest, parse_choice_suggestion, parse_field_suggestion,
    parse_metadata_suggestion, parse_tag_suggestion, parse_title_suggestion, prompt_for_choice,
    prompt_for_fields, prompt_for_metadata, prompt_for_tags, prompt_for_title,
};
use archivist_config::AppConfig;
use archivist_core::{
    AiProviderKind, AuditEventInput, ChoiceSuggestion, DocumentPatch, LanguageDetection,
    MetadataFieldFlags, MetadataSuggestion, OldTagStrategy, ProcessingMode, RuntimeSettings, Stage,
    TagSuggestion, TitleSuggestion, detect_document_language, extract_issue_date_suggestion,
    validate_choice_suggestion, validate_document_date_suggestion, validate_field_suggestion,
    validate_tag_suggestion, validate_title_suggestion,
};
use archivist_db::{
    AiArtifactInput, DbPool, JobRecord, ReviewItemRecord, append_audit, claim_jobs,
    claim_notification_delivery, claim_pending_review_for_autopilot_drain, complete_job, connect,
    create_review_item, create_run_with_jobs, custom_field_ids_for_names, fail_job,
    get_active_prompt, get_backlog_counts, get_dashboard_live_status, get_runtime_settings,
    get_workflow_safety_status, insert_ai_artifact, is_last_active_job,
    list_allowed_named_entities, list_allowed_tag_names, list_custom_fields,
    list_pending_review_items_for_autopilot_drain, mark_review_auto_applied,
    named_entity_id_for_name, queue_missing_pipeline, record_dashboard_snapshot,
    record_document_language, resolve_secret, revert_review_to_pending_after_failed_drain,
    selector_document_budget, tag_ids_for_names, upsert_inventory_item,
    upsert_paperless_custom_field, upsert_paperless_named_entity, upsert_paperless_tag,
};
use archivist_ocr::{normalize_ocr_pages, render_document_pages, validate_ocr_text};
use archivist_paperless::{
    PaperlessClient, PaperlessDocumentDetail, PaperlessDocumentSummary, PaperlessError,
    PaperlessTag,
};
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Client as HttpClient;
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::signal;
use tokio::time::{sleep, timeout};
use tracing::{Instrument, error, info, info_span, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AppConfig::from_env();
    config.validate()?;
    init_tracing(&config.log_level);

    let pool = connect(config.database_url.expose_secret()).await?;
    wait_for_schema(&pool).await?;
    run_worker(pool, Arc::new(config)).await
}

fn init_tracing(filter: &str) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .json()
        .init();
}

async fn run_worker(pool: DbPool, config: Arc<AppConfig>) -> Result<()> {
    let worker_id = format!("worker-{}", uuid::Uuid::now_v7());
    info!(%worker_id, "paperless archivist worker started");
    let mut tick: u64 = 0;
    let trigger_poll_running = Arc::new(AtomicBool::new(false));

    // Write a fresh dashboard snapshot near startup so the read path has something current
    // before the periodic tick fires (snapshots used to be written on every /dashboard read).
    if let Err(error) = record_dashboard_snapshot_tick(&pool).await {
        warn!(error = %error, "initial dashboard snapshot failed");
    }

    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!(%worker_id, "worker shutdown requested");
                return Ok(());
            }
            _ = sleep(Duration::from_secs(5)) => {
                tick += 1;
                if let Err(error) = process_available_jobs(&pool, &config, &worker_id).await {
                    error!(error = %error, "job processing tick failed");
                }
                if tick % 12 == 3
                    && let Err(error) = timeout(
                        Duration::from_secs(20),
                        send_operational_notifications(&pool, &config),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("notification tick timed out")))
                {
                    warn!(error = %error, "notification tick failed");
                }
                // Dashboard snapshot writes used to fire on every /dashboard read; now they
                // happen here once per minute (every 12 five-second ticks).
                if tick % 12 == 5
                    && let Err(error) = record_dashboard_snapshot_tick(&pool).await
                {
                    warn!(error = %error, "dashboard snapshot tick failed");
                }
                // Autopilot review drain: when the runtime is in full_auto, any review_items
                // still sitting in `pending` are auto-applied here, respecting the same safety
                // budget the auto-selector honors. This handles the residual backlog from
                // historical batches that routed to manual_review before commit 0d7a915 made
                // routing follow live runtime mode, and any future flip-from-review case.
                if tick % 12 == 7
                    && let Err(error) =
                        drain_pending_reviews_if_autopilot_tick(&pool, &config).await
                {
                    warn!(error = %error, "autopilot review drain tick failed");
                }
                if tick % 12 == 1
                    && trigger_poll_running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    let pool = pool.clone();
                    let config = Arc::clone(&config);
                    let trigger_poll_running = Arc::clone(&trigger_poll_running);
                    tokio::spawn(async move {
                        let trace_id = Uuid::now_v7();
                        let started = std::time::Instant::now();
                        info!(%trace_id, "trigger polling started");
                        let result = timeout(
                            Duration::from_secs(300),
                            poll_paperless_triggers(&pool, &config),
                        )
                        .await;
                        match result {
                            Ok(Ok(())) => {
                                info!(%trace_id, duration_ms = started.elapsed().as_millis() as u64, "trigger polling completed");
                            }
                            Ok(Err(error)) => {
                                warn!(%trace_id, error = %error, duration_ms = started.elapsed().as_millis() as u64, "trigger polling failed");
                            }
                            Err(_) => {
                                warn!(%trace_id, duration_ms = started.elapsed().as_millis() as u64, "trigger polling timed out");
                            }
                        }
                        trigger_poll_running.store(false, Ordering::Release);
                    });
                }
            }
        }
    }
}

async fn record_dashboard_snapshot_tick(pool: &DbPool) -> Result<()> {
    let counts = get_backlog_counts(pool).await?;
    record_dashboard_snapshot(pool, &counts).await
}

/// Tick wrapper for the autopilot review drain.
///
/// Loads the latest runtime settings each invocation (the dashboard mode
/// badge reflects the live runtime mode, and so should this drain).
///
/// The outer timeout is intentionally generous (8 minutes) because each
/// drained item already has its own short Paperless-side timeout — see
/// `apply_one_autopilot_drain_review`. The outer cap is just a last-ditch
/// liveness guard so a fully wedged Paperless host can't permanently
/// occupy this tick slot.
async fn drain_pending_reviews_if_autopilot_tick(pool: &DbPool, config: &AppConfig) -> Result<()> {
    let settings = get_runtime_settings(pool).await?;
    let applied = timeout(
        Duration::from_secs(8 * 60),
        drain_pending_reviews_if_autopilot(pool, config, &settings),
    )
    .await
    .map_err(|_| anyhow!("autopilot drain tick timed out"))??;
    if applied > 0 {
        info!(
            applied,
            mode = %settings.workflow.mode,
            "autopilot review drain applied pending items"
        );
    }
    Ok(())
}

async fn wait_for_schema(pool: &DbPool) -> Result<()> {
    for attempt in 1..=60 {
        match get_runtime_settings(pool).await {
            Ok(_) => return Ok(()),
            Err(error) if attempt < 60 => {
                warn!(attempt, error = %error, "waiting for API database migrations");
                sleep(Duration::from_secs(2)).await;
            }
            Err(error) => return Err(error).context("wait for API database migrations"),
        }
    }
    Ok(())
}

async fn process_available_jobs(
    pool: &DbPool,
    config: &Arc<AppConfig>,
    worker_id: &str,
) -> Result<()> {
    let jobs = claim_jobs(pool, config.worker_concurrency as i64, worker_id, 300).await?;
    if jobs.is_empty() {
        return Ok(());
    }
    info!(claimed_jobs = jobs.len(), %worker_id, "claimed jobs for processing");

    // Cache RuntimeSettings + PaperlessClient at the batch boundary so each claimed job no
    // longer re-fetches settings and re-decrypts the Paperless token. Effective TTL is the
    // batch interval (~5s by default); a fresh batch always re-reads the latest settings.
    let settings = match get_runtime_settings(pool).await {
        Ok(settings) => Arc::new(settings),
        Err(error) => {
            warn!(error = %error, "failed to load runtime settings for batch; failing claimed jobs");
            for job in &jobs {
                let _ = fail_job(pool, job, &error.to_string(), true).await;
            }
            return Ok(());
        }
    };
    let paperless = match paperless_client(pool, config, &settings).await {
        Ok(client) => Arc::new(client),
        Err(error) => {
            warn!(error = %error, "failed to construct Paperless client for batch; failing claimed jobs");
            for job in &jobs {
                let _ = fail_job(pool, job, &error.to_string(), true).await;
            }
            return Ok(());
        }
    };

    let mut pending = FuturesUnordered::new();
    for job in jobs {
        let pool = pool.clone();
        let config = Arc::clone(config);
        let settings = Arc::clone(&settings);
        let paperless = Arc::clone(&paperless);
        let trace_id = job.run_id;
        let span = info_span!(
            "archivist_job",
            trace_id = %trace_id,
            run_id = %job.run_id,
            job_id = %job.id,
            document_id = job.paperless_document_id,
            stage = %job.stage,
            attempt = job.attempts
        );
        pending.push(tokio::spawn(
            async move {
                let started = std::time::Instant::now();
                let result =
                    process_job(&pool, &config, settings.as_ref(), paperless.as_ref(), &job).await;
                if let Err(error) = &result {
                    let failure_class = classify_processing_failure(error);
                    warn!(
                        error = %error,
                        failure_class = failure_class.as_str(),
                        duration_ms = started.elapsed().as_millis() as u64,
                        "job processing failed"
                    );
                    let _ = fail_job(
                        &pool,
                        &job,
                        &error.to_string(),
                        failure_class.is_retryable(),
                    )
                    .await;
                } else {
                    info!(
                        duration_ms = started.elapsed().as_millis() as u64,
                        "job processing completed"
                    );
                }
                result
            }
            .instrument(span),
        ));
    }

    while let Some(result) = pending.next().await {
        if let Err(error) = result {
            warn!(error = %error, "worker task join failed");
        }
    }
    Ok(())
}

async fn send_operational_notifications(pool: &DbPool, config: &AppConfig) -> Result<()> {
    let settings = get_runtime_settings(pool).await?;
    if !settings.notifications.enabled {
        return Ok(());
    }
    let Some(webhook_secret_id) = settings.notifications.webhook_url_secret_id else {
        return Ok(());
    };
    let Some(webhook_url) = resolve_secret(pool, &config.secret_key, webhook_secret_id).await?
    else {
        return Ok(());
    };
    let cooldown = settings.notifications.cooldown_minutes as i32;
    let counts = get_backlog_counts(pool).await?;
    if counts.waiting_review >= settings.notifications.review_queue_threshold
        && claim_notification_delivery(pool, "review_queue_backlog", cooldown).await?
    {
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "review_queue_backlog",
                "severity": "warning",
                "title": "Review queue needs attention",
                "description": "Paperless Archivist has documents waiting for human review.",
                "metadata": {
                    "waiting_review": counts.waiting_review,
                    "threshold": settings.notifications.review_queue_threshold
                }
            }),
        )
        .await?;
    }

    let live = get_dashboard_live_status(pool, &settings).await?;
    let hard_failures = live
        .recent_failures
        .iter()
        .filter(|failure| failure.status == "failed" || failure.failure_kind == "failed")
        .count() as i64;
    if hard_failures >= settings.notifications.repeated_failure_threshold
        && claim_notification_delivery(pool, "repeated_processing_failures", cooldown).await?
    {
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "repeated_processing_failures",
                "severity": "error",
                "title": "Repeated processing failures",
                "description": "Recent Paperless Archivist jobs are failing. Check the dashboard live status and worker logs.",
                "metadata": {
                    "recent_failure_count": hard_failures,
                    "threshold": settings.notifications.repeated_failure_threshold
                }
            }),
        )
        .await?;
    }

    if settings.workflow.mode == ProcessingMode::FullAuto
        && settings.workflow.paused
        && claim_notification_delivery(pool, "paused_full_auto", cooldown).await?
    {
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "paused_full_auto",
                "severity": "warning",
                "title": "Full autopilot is paused",
                "description": "Full autopilot is configured but processing is paused.",
                "metadata": {
                    "workflow_mode": "full_auto",
                    "paused": true
                }
            }),
        )
        .await?;
    }
    Ok(())
}

async fn send_notification_webhook(
    webhook_url: &SecretString,
    payload: serde_json::Value,
) -> Result<()> {
    let response = HttpClient::builder()
        .timeout(Duration::from_secs(10))
        .build()?
        .post(webhook_url.expose_secret())
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "notification webhook request failed: {}",
                error.without_url()
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("notification webhook returned {status}"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessingFailureClass {
    Transient,
    Permanent,
}

impl ProcessingFailureClass {
    fn is_retryable(self) -> bool {
        matches!(self, Self::Transient)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
        }
    }
}

/// Decide whether `error` should be retried with backoff (Transient) or marked
/// permanent. The function first walks the error chain looking for typed
/// errors from `archivist-paperless` and `archivist-ai`; those carry an
/// authoritative `is_transient()` classification and bypass substring guesses.
/// Anything else — DB driver errors, `reqwest::Error` raised outside the typed
/// wrappers, third-party HTTP clients — falls through to substring matching as
/// a documented last resort.
fn classify_processing_failure(error: &anyhow::Error) -> ProcessingFailureClass {
    for cause in error.chain() {
        if let Some(paperless_error) = cause.downcast_ref::<PaperlessError>() {
            return if paperless_error.is_transient() {
                ProcessingFailureClass::Transient
            } else {
                ProcessingFailureClass::Permanent
            };
        }
        if let Some(ai_error) = cause.downcast_ref::<AiProviderError>() {
            return if ai_error.is_transient() {
                ProcessingFailureClass::Transient
            } else {
                ProcessingFailureClass::Permanent
            };
        }
    }

    // Last-resort substring matcher: covers errors that arise *outside* the
    // typed surfaces — sqlx pool errors, reqwest errors from helpers that
    // still use `anyhow!`, raw HTTP responses, etc. Any new error path
    // should prefer adding a typed variant in the originating crate so this
    // table can keep shrinking.
    let message = error
        .chain()
        .map(|cause| cause.to_string().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    let transient_markers = [
        "timeout",
        "timed out",
        "connection refused",
        "connection reset",
        "connection closed",
        "temporarily unavailable",
        "service unavailable",
        "internal server error",
        "ollama",
        "runner process no longer running",
        "database",
        "pool timed out",
        "broken pipe",
        "dns",
        "network",
        "502",
        "503",
        "504",
    ];

    if transient_markers
        .iter()
        .any(|marker| message.contains(marker))
    {
        ProcessingFailureClass::Transient
    } else {
        ProcessingFailureClass::Permanent
    }
}

async fn process_job(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
    paperless: &PaperlessClient,
    job: &JobRecord,
) -> Result<()> {
    info!(job_id = %job.id, run_id = %job.run_id, document_id = job.paperless_document_id, stage = %job.stage, "processing job");

    match job.stage {
        Stage::Ocr => process_ocr(pool, config, paperless, settings, job).await,
        Stage::Tags => process_tags(pool, config, paperless, settings, job).await,
        Stage::Title => process_title(pool, config, paperless, settings, job).await,
        Stage::Correspondent => {
            process_choice(
                pool,
                config,
                paperless,
                settings,
                job,
                "correspondent",
                "paperless_correspondents",
            )
            .await
        }
        Stage::DocumentType => {
            process_choice(
                pool,
                config,
                paperless,
                settings,
                job,
                "document type",
                "paperless_document_types",
            )
            .await
        }
        Stage::DocumentDate => process_document_date(pool, paperless, settings, job).await,
        Stage::Fields => process_fields(pool, config, paperless, settings, job).await,
        Stage::Metadata => process_metadata(pool, config, paperless, settings, job).await,
        Stage::OcrFix | Stage::Apply => Err(anyhow!(
            "stage {} is not directly executable by the worker",
            job.stage
        )),
    }
}

async fn process_ocr(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
) -> Result<()> {
    let original = paperless
        .download_original(job.paperless_document_id)
        .await?;
    let document = paperless.get_document(job.paperless_document_id).await?;
    let pages = render_document_pages(
        &original,
        document.original_file_name.as_deref(),
        settings.ocr.page_limit,
    )
    .await?;
    if pages.is_empty() {
        return Err(anyhow!("document rendered zero OCR pages"));
    }
    let page_bytes: usize = pages.iter().map(|page| page.bytes.len()).sum();
    info!(
        job_id = %job.id,
        document_id = job.paperless_document_id,
        pages = pages.len(),
        page_bytes,
        "rendered OCR input pages"
    );

    let provider = provider_for_stage(settings, Stage::Ocr, true)?;
    let prompt = get_active_prompt(pool, Stage::Ocr).await?;
    let mut texts = Vec::new();
    let mut raw_responses = Vec::new();
    let started = std::time::Instant::now();
    for (index, page) in pages.iter().enumerate() {
        let page_prompt = prompt
            .as_ref()
            .map(|prompt| {
                format!(
                    "{}\n\nPage {}: transcribe exactly and return only OCR text.",
                    prompt.content,
                    index + 1
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "{}\n\nPage {}: transcribe exactly and return only OCR text.",
                    DEFAULT_OCR_SYSTEM_PROMPT,
                    index + 1
                )
            });
        let response = vision_with_provider(
            pool,
            config,
            &provider,
            VisionRequest {
                model: provider.model.clone(),
                temperature: 0.0,
                prompt: page_prompt,
                images: vec![ImageInput {
                    mime_type: page.mime_type.clone(),
                    bytes: page.bytes.clone(),
                }],
            },
        )
        .await?;
        texts.push(response.text);
        raw_responses.push(response.raw_response);
    }
    let text = normalize_ocr_pages(&texts);
    validate_ocr_text(&text, settings.ocr.min_chars)?;
    let language_detection = detect_document_language(&text);
    record_document_language(
        pool,
        job.paperless_document_id,
        &language_detection,
        Some(job.run_id),
        Some(job.id),
        "worker",
    )
    .await?;

    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Ocr,
            provider: &provider.name,
            model: &provider.model,
            prompt_id: prompt.as_ref().map(|prompt| prompt.id),
            input_hash: &hash_bytes(&original),
            request: None,
            response: Some(json!({ "pages": raw_responses })),
            normalized_output: Some(json!({
                "content_chars": text.chars().count(),
                "language": language_detection.language,
                "language_confidence": language_detection.confidence,
                "language_source": language_detection.source
            })),
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    let patch = DocumentPatch {
        content: Some(text),
        title: None,
        tags: None,
        correspondent: None,
        document_type: None,
        created: None,
        custom_fields: None,
    };
    handle_patch_result(pool, paperless, settings, job, patch, Vec::new(), None).await
}

async fn language_context_for_content(
    pool: &DbPool,
    settings: &RuntimeSettings,
    job: &JobRecord,
    content: &str,
) -> Result<PromptLanguageContext> {
    let detection = if content.trim().is_empty() {
        LanguageDetection::unknown("heuristic")
    } else {
        detect_document_language(content)
    };
    record_document_language(
        pool,
        job.paperless_document_id,
        &detection,
        Some(job.run_id),
        Some(job.id),
        "worker",
    )
    .await?;
    Ok(PromptLanguageContext::new(
        &detection,
        &settings.tagging.tag_output_language,
    ))
}

async fn process_tags(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
) -> Result<()> {
    let document = paperless.get_document(job.paperless_document_id).await?;
    let content = document.content.unwrap_or_default();
    let allowed = list_allowed_tag_names(pool).await?;
    let language = language_context_for_content(pool, settings, job, &content).await?;
    let mut request = prompt_for_tags(&content, &allowed, settings.tagging.max_tags, &language);
    let prompt_id = apply_active_prompt(pool, Stage::Tags, &mut request).await?;
    let response = chat_for_stage(pool, config, settings, Stage::Tags, request.clone()).await?;
    let suggestion = parse_tag_suggestion(&response.text).unwrap_or(TagSuggestion {
        tags: Vec::new(),
        new_tags: Vec::new(),
        confidence: Some(0.0),
    });
    let normalized = serde_json::to_value(&suggestion)?;
    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Tags,
            provider: &response.provider,
            model: &response.model,
            prompt_id,
            input_hash: &hash_text(&content),
            request: Some(serde_json::to_value(request)?),
            response: Some(response.raw_response),
            normalized_output: Some(normalized.clone()),
            duration_ms: response.duration_ms,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    match validate_tag_suggestion(
        suggestion,
        &allowed,
        &settings.workflow.tags,
        &settings.tagging,
    ) {
        Ok(valid) => {
            let selected_ids = tag_ids_for_names(pool, &valid.tags).await?;
            let mut tag_ids = match settings.tagging.old_tag_strategy {
                OldTagStrategy::KeepExisting | OldTagStrategy::ReplaceAiManaged => {
                    document.tags.clone()
                }
                OldTagStrategy::RemoveAllBusiness => Vec::new(),
            };
            for tag_id in selected_ids {
                if !tag_ids.contains(&tag_id) {
                    tag_ids.push(tag_id);
                }
            }
            tag_ids.sort_unstable();
            tag_ids.dedup();
            let patch = DocumentPatch {
                content: None,
                title: None,
                tags: Some(tag_ids),
                correspondent: None,
                document_type: None,
                created: None,
                custom_fields: None,
            };
            handle_patch_result(pool, paperless, settings, job, patch, valid.warnings, None).await
        }
        Err(errors) => {
            let patch = json!({
                "tags": normalized.get("tags").cloned().unwrap_or_else(|| json!([]))
            });
            create_review_item(pool, job, patch, json!(errors)).await?;
            Ok(())
        }
    }
}

async fn process_title(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
) -> Result<()> {
    let document = paperless.get_document(job.paperless_document_id).await?;
    let content = document.content.unwrap_or_default();
    let language = language_context_for_content(pool, settings, job, &content).await?;
    let mut request = prompt_for_title(&content, &language);
    let prompt_id = apply_active_prompt(pool, Stage::Title, &mut request).await?;
    let response = chat_for_stage(pool, config, settings, Stage::Title, request.clone()).await?;
    let suggestion = parse_title_suggestion(&response.text).unwrap_or(TitleSuggestion {
        title: String::new(),
        confidence: Some(0.0),
    });
    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Title,
            provider: &response.provider,
            model: &response.model,
            prompt_id,
            input_hash: &hash_text(&content),
            request: Some(serde_json::to_value(request)?),
            response: Some(response.raw_response),
            normalized_output: Some(serde_json::to_value(&suggestion)?),
            duration_ms: response.duration_ms,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;
    match validate_title_suggestion(suggestion, 160, settings.tagging.confidence_threshold) {
        Ok(valid) => {
            let patch = DocumentPatch {
                content: None,
                title: Some(valid.title),
                tags: None,
                correspondent: None,
                document_type: None,
                created: None,
                custom_fields: None,
            };
            handle_patch_result(pool, paperless, settings, job, patch, Vec::new(), None).await
        }
        Err(errors) => {
            create_review_item(pool, job, json!({ "title": "" }), json!(errors)).await?;
            Ok(())
        }
    }
}

async fn process_choice(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    choice_kind: &str,
    table: &str,
) -> Result<()> {
    let allowed = list_allowed_named_entities(pool, table).await?;
    if allowed.is_empty() {
        complete_job(pool, job, json!({ "skipped": "no allowed choices" })).await?;
        return Ok(());
    }
    let document = paperless.get_document(job.paperless_document_id).await?;
    if job.stage == Stage::Correspondent
        && document.correspondent.is_some()
        && !settings.metadata.overwrite_existing_correspondent
    {
        complete_job(
            pool,
            job,
            json!({ "skipped": "Paperless correspondent already set" }),
        )
        .await?;
        return Ok(());
    }
    if job.stage == Stage::DocumentType
        && document.document_type.is_some()
        && !settings.metadata.overwrite_existing_document_type
    {
        complete_job(
            pool,
            job,
            json!({ "skipped": "Paperless document type already set" }),
        )
        .await?;
        return Ok(());
    }
    let content = document.content.unwrap_or_default();
    let language = language_context_for_content(pool, settings, job, &content).await?;
    let mut request = prompt_for_choice(&content, choice_kind, &allowed, &language);
    let prompt_id = apply_active_prompt(pool, job.stage, &mut request).await?;
    let response = chat_for_stage(pool, config, settings, job.stage, request.clone()).await?;
    let suggestion = parse_choice_suggestion(&response.text).unwrap_or(ChoiceSuggestion {
        name: String::new(),
        confidence: Some(0.0),
        evidence: None,
    });
    let suggestion_for_review = suggestion.clone();
    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: job.stage,
            provider: &response.provider,
            model: &response.model,
            prompt_id,
            input_hash: &hash_text(&content),
            request: Some(serde_json::to_value(request)?),
            response: Some(response.raw_response),
            normalized_output: Some(serde_json::to_value(&suggestion)?),
            duration_ms: response.duration_ms,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;
    let patch_key = if job.stage == Stage::DocumentType {
        "document_type"
    } else {
        choice_kind
    };
    match validate_choice_suggestion(suggestion, &allowed, settings.metadata.confidence_threshold) {
        Ok(valid) => {
            let id = named_entity_id_for_name(pool, table, &valid.name)
                .await?
                .ok_or_else(|| anyhow!("validated choice disappeared from cache"))?;
            let patch = if job.stage == Stage::Correspondent {
                DocumentPatch {
                    content: None,
                    title: None,
                    tags: None,
                    correspondent: Some(Some(id)),
                    document_type: None,
                    created: None,
                    custom_fields: None,
                }
            } else {
                DocumentPatch {
                    content: None,
                    title: None,
                    tags: None,
                    correspondent: None,
                    document_type: Some(Some(id)),
                    created: None,
                    custom_fields: None,
                }
            };
            handle_patch_result(
                pool,
                paperless,
                settings,
                job,
                patch,
                Vec::new(),
                Some(json!({
                    "field": patch_key,
                    "suggested_name": valid.name,
                    "confidence": valid.confidence,
                    "evidence": valid.evidence,
                    "current_correspondent": document.correspondent,
                    "current_document_type": document.document_type
                })),
            )
            .await
        }
        Err(errors) => {
            create_review_item(
                pool,
                job,
                json!({
                    patch_key: "",
                    "standard_metadata": {
                        "field": patch_key,
                        "suggested_name": suggestion_for_review.name,
                        "confidence": suggestion_for_review.confidence,
                        "evidence": suggestion_for_review.evidence
                    }
                }),
                json!(errors),
            )
            .await?;
            Ok(())
        }
    }
}

async fn process_document_date(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
) -> Result<()> {
    let document = paperless.get_document(job.paperless_document_id).await?;
    if document
        .created
        .as_deref()
        .is_some_and(|value| !value.is_empty())
        && !settings.metadata.overwrite_existing_document_date
    {
        complete_job(
            pool,
            job,
            json!({ "skipped": "Paperless document date already set" }),
        )
        .await?;
        return Ok(());
    }

    let content = document.content.unwrap_or_default();
    let language = if content.trim().is_empty() {
        LanguageDetection::unknown("heuristic")
    } else {
        detect_document_language(&content)
    };
    record_document_language(
        pool,
        job.paperless_document_id,
        &language,
        Some(job.run_id),
        Some(job.id),
        "worker",
    )
    .await?;

    let suggestion = extract_issue_date_suggestion(&content, &language).unwrap_or_else(|| {
        archivist_core::DocumentDateSuggestion {
            date: String::new(),
            confidence: Some(0.0),
            evidence: None,
            warnings: vec!["no document date candidate found".to_owned()],
        }
    });
    let normalized = serde_json::to_value(&suggestion)?;
    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::DocumentDate,
            provider: "heuristic",
            model: "date-regex-v1",
            prompt_id: None,
            input_hash: &hash_text(&content),
            request: Some(json!({
                "language": language.language,
                "language_confidence": language.confidence
            })),
            response: None,
            normalized_output: Some(normalized.clone()),
            duration_ms: 0,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    match validate_document_date_suggestion(
        suggestion,
        settings.metadata.document_date_confidence_threshold,
    ) {
        Ok(valid) => {
            let patch = DocumentPatch {
                content: None,
                title: None,
                tags: None,
                correspondent: None,
                document_type: None,
                created: Some(valid.date.clone()),
                custom_fields: None,
            };
            handle_patch_result(
                pool,
                paperless,
                settings,
                job,
                patch,
                valid.warnings.clone(),
                Some(json!({
                    "field": "document_date",
                    "suggested_date": valid.date,
                    "confidence": valid.confidence,
                    "evidence": valid.evidence,
                    "current_date": document.created
                })),
            )
            .await
        }
        Err(errors) => {
            create_review_item(
                pool,
                job,
                json!({
                    "created": normalized.get("date").cloned().unwrap_or_else(|| json!("")),
                    "standard_metadata": {
                        "field": "document_date",
                        "suggested_date": normalized.get("date").cloned().unwrap_or_else(|| json!("")),
                        "confidence": normalized.get("confidence").cloned().unwrap_or(json!(null)),
                        "evidence": normalized.get("evidence").cloned().unwrap_or(json!(null)),
                        "warnings": normalized.get("warnings").cloned().unwrap_or_else(|| json!([]))
                    }
                }),
                json!(errors),
            )
            .await?;
            Ok(())
        }
    }
}

async fn process_fields(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
) -> Result<()> {
    let fields = list_custom_fields(pool).await?;
    if fields.is_empty() {
        complete_job(
            pool,
            job,
            json!({ "skipped": "no Paperless custom fields configured" }),
        )
        .await?;
        return Ok(());
    }
    let document = paperless.get_document(job.paperless_document_id).await?;
    let content = document.content.unwrap_or_default();
    let allowed = fields
        .iter()
        .filter(|field| settings.fields.field_enabled(&field.name))
        .map(|field| field.name.clone())
        .collect::<Vec<_>>();
    if allowed.is_empty() {
        complete_job(
            pool,
            job,
            json!({ "skipped": "all Paperless custom fields are disabled by field mappings" }),
        )
        .await?;
        return Ok(());
    }
    let language = language_context_for_content(pool, settings, job, &content).await?;
    let mut request = prompt_for_fields(&content, &allowed, settings.fields.max_fields, &language);
    let prompt_id = apply_active_prompt(pool, Stage::Fields, &mut request).await?;
    let response = chat_for_stage(pool, config, settings, Stage::Fields, request.clone()).await?;
    let suggestion =
        parse_field_suggestion(&response.text).unwrap_or(archivist_core::FieldSuggestion {
            fields: Vec::new(),
            confidence: Some(0.0),
        });
    let normalized = serde_json::to_value(&suggestion)?;
    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Fields,
            provider: &response.provider,
            model: &response.model,
            prompt_id,
            input_hash: &hash_text(&content),
            request: Some(serde_json::to_value(request)?),
            response: Some(response.raw_response),
            normalized_output: Some(normalized.clone()),
            duration_ms: response.duration_ms,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    match validate_field_suggestion(
        suggestion,
        &allowed,
        settings.fields.max_fields,
        settings.fields.confidence_threshold,
    ) {
        Ok(valid) => {
            let names = valid
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect::<Vec<_>>();
            let ids = custom_field_ids_for_names(pool, &names).await?;
            let values = valid
                .fields
                .iter()
                .filter_map(|field| {
                    ids.iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(&field.name))
                        .map(|(_, id)| json!({ "field": id, "value": field.value }))
                })
                .collect::<Vec<_>>();
            let patch = DocumentPatch {
                content: None,
                title: None,
                tags: None,
                correspondent: None,
                document_type: None,
                created: None,
                custom_fields: Some(json!(values)),
            };
            handle_patch_result(pool, paperless, settings, job, patch, valid.warnings, None).await
        }
        Err(errors) => {
            create_review_item(
                pool,
                job,
                json!({ "custom_fields": normalized.get("fields").cloned().unwrap_or_else(|| json!([])) }),
                json!(errors),
            )
            .await?;
            Ok(())
        }
    }
}

/// Consolidated metadata stage (v1.4.0). One LLM call replaces six per-field
/// round-trips. The response is fanned out into up to six review items (or one
/// composite Paperless patch in full_auto mode) so existing reviewer UX, audit
/// trails, and per-field opt-outs keep working.
async fn process_metadata(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
) -> Result<()> {
    let enabled = MetadataFieldFlags::from_enabled_stages(&settings.workflow.enabled_stages);
    if !enabled.any() {
        complete_job(
            pool,
            job,
            json!({ "skipped": "no metadata fields are enabled in workflow settings" }),
        )
        .await?;
        return Ok(());
    }

    let document = paperless.get_document(job.paperless_document_id).await?;
    let content = document.content.clone().unwrap_or_default();
    let language = language_context_for_content(pool, settings, job, &content).await?;

    // Cheap pre-flight: short-circuit fields that Paperless already populated and the operator
    // has not opted into overwriting. We still ask the LLM for the field if any other field is
    // requested, but we drop the suggestion before creating a review item / applying. Doing the
    // gating after the LLM call keeps the prompt deterministic across runs.
    let allowed_correspondents = if enabled.correspondent {
        list_allowed_named_entities(pool, "paperless_correspondents").await?
    } else {
        Vec::new()
    };
    let allowed_document_types = if enabled.document_type {
        list_allowed_named_entities(pool, "paperless_document_types").await?
    } else {
        Vec::new()
    };
    let allowed_tags = if enabled.tags {
        list_allowed_tag_names(pool).await?
    } else {
        Vec::new()
    };
    let allowed_field_names = if enabled.fields {
        list_custom_fields(pool)
            .await?
            .into_iter()
            .filter(|field| settings.fields.field_enabled(&field.name))
            .map(|field| field.name)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let mut request = prompt_for_metadata(
        &content,
        &allowed_correspondents,
        &allowed_document_types,
        &allowed_tags,
        &allowed_field_names,
        &enabled,
        &language,
        settings.tagging.max_tags,
        settings.fields.max_fields,
    );
    let prompt_id = apply_active_prompt(pool, Stage::Metadata, &mut request).await?;
    let response = chat_for_stage(pool, config, settings, Stage::Metadata, request.clone()).await?;
    let suggestion =
        parse_metadata_suggestion(&response.text).unwrap_or_else(|_| MetadataSuggestion::default());
    let normalized = serde_json::to_value(&suggestion)?;

    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Metadata,
            provider: &response.provider,
            model: &response.model,
            prompt_id,
            input_hash: &hash_text(&content),
            request: Some(serde_json::to_value(request)?),
            response: Some(response.raw_response),
            normalized_output: Some(normalized.clone()),
            duration_ms: response.duration_ms,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    // Fan the suggestion out into per-field outcomes.
    //
    // Each field is one of:
    //   * `Apply(field_patch)`              — valid, ready to auto-apply or to attach to a
    //                                         composite review item.
    //   * `Review(review_patch, warnings)`  — needs operator review (low confidence, validation
    //                                         failure, or operator policy says "don't overwrite").
    //   * `Skip(reason)`                     — model omitted the field or the document already had
    //                                         a value we are not allowed to overwrite.
    let auto_apply = settings.workflow.mode.auto_apply_validated_suggestions()
        && !settings.workflow.dry_run;
    let mut composite_patch = DocumentPatch {
        content: None,
        title: None,
        tags: None,
        correspondent: None,
        document_type: None,
        created: None,
        custom_fields: None,
    };
    let mut composite_warnings: Vec<String> = Vec::new();
    let mut review_items: Vec<(serde_json::Value, serde_json::Value)> = Vec::new();
    let mut applied_fields: Vec<&'static str> = Vec::new();
    let mut skipped_fields: Vec<&'static str> = Vec::new();

    // --- title ---
    if enabled.title
        && let Some(title) = suggestion.title.clone()
    {
        match validate_title_suggestion(title.clone(), 160, settings.tagging.confidence_threshold) {
            Ok(valid) => {
                composite_patch.title = Some(valid.title.clone());
                applied_fields.push("title");
            }
            Err(errors) => {
                review_items.push((
                    json!({
                        "title": title.title,
                        "standard_metadata": { "field": "title", "confidence": title.confidence }
                    }),
                    json!(errors),
                ));
            }
        }
    }

    // --- document_type ---
    if enabled.document_type
        && let Some(choice) = suggestion.document_type.clone()
    {
        if document.document_type.is_some()
            && !settings.metadata.overwrite_existing_document_type
        {
            skipped_fields.push("document_type");
        } else {
            match validate_choice_suggestion(
                choice.clone(),
                &allowed_document_types,
                settings.metadata.confidence_threshold,
            ) {
                Ok(valid) => {
                    let id = named_entity_id_for_name(pool, "paperless_document_types", &valid.name)
                        .await?;
                    if let Some(id) = id {
                        composite_patch.document_type = Some(Some(id));
                        applied_fields.push("document_type");
                    } else {
                        skipped_fields.push("document_type");
                    }
                }
                Err(errors) => {
                    review_items.push((
                        json!({
                            "document_type": "",
                            "standard_metadata": {
                                "field": "document_type",
                                "suggested_name": choice.name,
                                "confidence": choice.confidence,
                                "evidence": choice.evidence,
                                "current_document_type": document.document_type,
                            }
                        }),
                        json!(errors),
                    ));
                }
            }
        }
    }

    // --- correspondent ---
    if enabled.correspondent
        && let Some(choice) = suggestion.correspondent.clone()
    {
        if document.correspondent.is_some()
            && !settings.metadata.overwrite_existing_correspondent
        {
            skipped_fields.push("correspondent");
        } else {
            match validate_choice_suggestion(
                choice.clone(),
                &allowed_correspondents,
                settings.metadata.confidence_threshold,
            ) {
                Ok(valid) => {
                    let id =
                        named_entity_id_for_name(pool, "paperless_correspondents", &valid.name)
                            .await?;
                    if let Some(id) = id {
                        composite_patch.correspondent = Some(Some(id));
                        applied_fields.push("correspondent");
                    } else {
                        skipped_fields.push("correspondent");
                    }
                }
                Err(errors) => {
                    review_items.push((
                        json!({
                            "correspondent": "",
                            "standard_metadata": {
                                "field": "correspondent",
                                "suggested_name": choice.name,
                                "confidence": choice.confidence,
                                "evidence": choice.evidence,
                                "current_correspondent": document.correspondent,
                            }
                        }),
                        json!(errors),
                    ));
                }
            }
        }
    }

    // --- document_date ---
    if enabled.document_date
        && let Some(date) = suggestion.document_date.clone()
    {
        let already_set = document
            .created
            .as_deref()
            .is_some_and(|value| !value.is_empty());
        if already_set && !settings.metadata.overwrite_existing_document_date {
            skipped_fields.push("document_date");
        } else {
            match validate_document_date_suggestion(
                date.clone(),
                settings.metadata.document_date_confidence_threshold,
            ) {
                Ok(valid) => {
                    composite_patch.created = Some(valid.date.clone());
                    composite_warnings.extend(valid.warnings);
                    applied_fields.push("document_date");
                }
                Err(errors) => {
                    review_items.push((
                        json!({
                            "created": date.date.clone(),
                            "standard_metadata": {
                                "field": "document_date",
                                "suggested_date": date.date,
                                "confidence": date.confidence,
                                "evidence": date.evidence,
                                "warnings": date.warnings,
                                "current_date": document.created,
                            }
                        }),
                        json!(errors),
                    ));
                }
            }
        }
    }

    // --- tags ---
    if enabled.tags
        && let Some(tags) = suggestion.tags.clone()
    {
        match validate_tag_suggestion(
            tags.clone(),
            &allowed_tags,
            &settings.workflow.tags,
            &settings.tagging,
        ) {
            Ok(valid) => {
                let selected_ids = tag_ids_for_names(pool, &valid.tags).await?;
                let mut tag_ids = match settings.tagging.old_tag_strategy {
                    OldTagStrategy::KeepExisting | OldTagStrategy::ReplaceAiManaged => {
                        document.tags.clone()
                    }
                    OldTagStrategy::RemoveAllBusiness => Vec::new(),
                };
                for tag_id in selected_ids {
                    if !tag_ids.contains(&tag_id) {
                        tag_ids.push(tag_id);
                    }
                }
                tag_ids.sort_unstable();
                tag_ids.dedup();
                composite_patch.tags = Some(tag_ids);
                composite_warnings.extend(valid.warnings);
                applied_fields.push("tags");
            }
            Err(errors) => {
                review_items.push((
                    json!({
                        "tags": tags.tags.clone(),
                        "standard_metadata": { "field": "tags", "confidence": tags.confidence }
                    }),
                    json!(errors),
                ));
            }
        }
    }

    // --- fields ---
    if enabled.fields
        && let Some(fields) = suggestion.fields.clone()
    {
        match validate_field_suggestion(
            fields.clone(),
            &allowed_field_names,
            settings.fields.max_fields,
            settings.fields.confidence_threshold,
        ) {
            Ok(valid) => {
                let names = valid
                    .fields
                    .iter()
                    .map(|field| field.name.clone())
                    .collect::<Vec<_>>();
                let ids = custom_field_ids_for_names(pool, &names).await?;
                let values = valid
                    .fields
                    .iter()
                    .filter_map(|field| {
                        ids.iter()
                            .find(|(name, _)| name.eq_ignore_ascii_case(&field.name))
                            .map(|(_, id)| json!({ "field": id, "value": field.value }))
                    })
                    .collect::<Vec<_>>();
                composite_patch.custom_fields = Some(json!(values));
                composite_warnings.extend(valid.warnings);
                applied_fields.push("fields");
            }
            Err(errors) => {
                review_items.push((
                    json!({ "custom_fields": fields.fields }),
                    json!(errors),
                ));
            }
        }
    }

    info!(
        job_id = %job.id,
        document_id = job.paperless_document_id,
        applied_fields = ?applied_fields,
        review_items = review_items.len(),
        skipped_fields = ?skipped_fields,
        "consolidated metadata stage planned outcome"
    );

    // Routing:
    //   * If anything needs review, every field becomes a review item — the operator inspects
    //     all suggestions atomically rather than seeing a half-applied document.
    //   * Otherwise, in full_auto mode we apply one composite Paperless patch.
    //   * If everything was skipped (already-set fields with overwrite disabled), we still
    //     mark the job complete so the run drains.
    if !review_items.is_empty() {
        // Demote applied fields to review items too, so the operator can sign off on the full
        // set rather than seeing partial application.
        if composite_patch.title.is_some() {
            review_items.push((
                json!({
                    "title": composite_patch.title.clone().unwrap_or_default(),
                    "standard_metadata": { "field": "title", "auto_validated": true }
                }),
                json!([]),
            ));
        }
        if let Some(Some(correspondent)) = composite_patch.correspondent {
            review_items.push((
                json!({
                    "correspondent": correspondent,
                    "standard_metadata": { "field": "correspondent", "auto_validated": true }
                }),
                json!([]),
            ));
        }
        if let Some(Some(document_type)) = composite_patch.document_type {
            review_items.push((
                json!({
                    "document_type": document_type,
                    "standard_metadata": { "field": "document_type", "auto_validated": true }
                }),
                json!([]),
            ));
        }
        if let Some(date) = composite_patch.created.clone() {
            review_items.push((
                json!({
                    "created": date,
                    "standard_metadata": { "field": "document_date", "auto_validated": true }
                }),
                json!([]),
            ));
        }
        if let Some(tags) = composite_patch.tags.clone() {
            review_items.push((
                json!({
                    "tags": tags,
                    "standard_metadata": { "field": "tags", "auto_validated": true }
                }),
                json!([]),
            ));
        }
        if let Some(custom_fields) = composite_patch.custom_fields.clone() {
            review_items.push((
                json!({
                    "custom_fields": custom_fields,
                    "standard_metadata": { "field": "fields", "auto_validated": true }
                }),
                json!([]),
            ));
        }

        for (patch, warnings) in review_items {
            create_review_item(pool, job, patch, warnings).await?;
        }
        Ok(())
    } else if !applied_fields.is_empty() {
        if auto_apply {
            let final_run_stage = is_last_active_job(pool, job.run_id, job.id).await?;
            apply_patch_with_workflow_tags(
                pool,
                paperless,
                settings,
                job,
                composite_patch,
                final_run_stage,
            )
            .await?;
            complete_job(
                pool,
                job,
                json!({
                    "applied": true,
                    "fields": applied_fields,
                    "warnings": composite_warnings,
                }),
            )
            .await
        } else {
            // manual_review (or dry_run): a single composite review item with all validated
            // suggestions so the operator approves the whole set atomically.
            let composite_review_patch = serde_json::to_value(&composite_patch)?;
            create_review_item(pool, job, composite_review_patch, json!(composite_warnings))
                .await?;
            Ok(())
        }
    } else {
        complete_job(
            pool,
            job,
            json!({
                "skipped": "all metadata fields skipped (already-set or model omitted)",
                "skipped_fields": skipped_fields,
            }),
        )
        .await
    }
}

async fn handle_patch_result(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    patch: DocumentPatch,
    warnings: Vec<String>,
    review_metadata: Option<serde_json::Value>,
) -> Result<()> {
    // Effective routing policy is the live runtime workflow mode, not the per-run mode that was
    // stamped onto pipeline_runs at queue time. Per-run mode is captured at queue time from the
    // runtime default, so once a batch is queued it cannot follow later operator policy changes
    // (e.g. operator flips runtime from manual_review to full_auto). Honoring runtime mode here
    // matches the operator's live intent and the dashboard mode badge. Per-run mode is still
    // recorded for audit/UX context. dry_run always forces review regardless of mode.
    let auto_apply = settings.workflow.mode.auto_apply_validated_suggestions();
    if !auto_apply || settings.workflow.dry_run {
        let mut review_patch = serde_json::to_value(patch)?;
        if let Some(metadata) = review_metadata
            && let Some(object) = review_patch.as_object_mut()
        {
            object.insert("standard_metadata".to_owned(), metadata);
        }
        let mut review_warnings = warnings;
        if settings.workflow.dry_run && auto_apply {
            review_warnings.push(
                "Dry-run is enabled: validated patch was evaluated but not auto-applied."
                    .to_owned(),
            );
        }
        let review_id = create_review_item(pool, job, review_patch, json!(review_warnings)).await?;
        if settings.workflow.dry_run && auto_apply {
            append_audit(
                pool,
                AuditEventInput {
                    event_type: "workflow.dry_run_review_created".to_owned(),
                    actor_type: "worker".to_owned(),
                    actor_id: None,
                    run_id: Some(job.run_id),
                    job_id: Some(job.id),
                    paperless_document_id: Some(job.paperless_document_id),
                    before: None,
                    after: Some(json!({ "review_id": review_id, "stage": job.stage })),
                    metadata: Some(json!({ "mode": job.mode })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
        }
        return Ok(());
    }
    let final_run_stage = is_last_active_job(pool, job.run_id, job.id).await?;
    apply_patch_with_workflow_tags(pool, paperless, settings, job, patch, final_run_stage).await?;
    complete_job(pool, job, json!({ "applied": true, "warnings": warnings })).await
}

async fn apply_patch_with_workflow_tags(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    mut patch: DocumentPatch,
    final_run_stage: bool,
) -> Result<()> {
    let document = paperless.get_document(job.paperless_document_id).await?;
    let tags = paperless.list_tags().await?;
    let mut tag_ids = patch.tags.clone().unwrap_or_else(|| document.tags.clone());

    if let Some(completion_name) = settings.workflow.tags.completion_tag_for_stage(job.stage) {
        let completion = paperless.ensure_tag(completion_name).await?;
        if !tag_ids.contains(&completion.id) {
            tag_ids.push(completion.id);
        }
    }
    if final_run_stage {
        let full = paperless
            .ensure_tag(&settings.workflow.tags.completion_processed)
            .await?;
        if !tag_ids.contains(&full.id) {
            tag_ids.push(full.id);
        }
    }
    for trigger_name in [
        settings.workflow.tags.trigger_tag_for_stage(job.stage),
        final_run_stage.then_some(settings.workflow.tags.trigger_process.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(trigger) = tags
            .iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(trigger_name))
        {
            tag_ids.retain(|id| *id != trigger.id);
        }
    }

    tag_ids.sort_unstable();
    tag_ids.dedup();
    patch.tags = Some(tag_ids);
    prune_unchanged_patch_fields(&mut patch, &document);
    let before_value = audit_before_for_patch(&document, &patch);
    let patch_value = audit_patch_payload(&patch);
    let apply_started = std::time::Instant::now();
    if let Err(error) = paperless
        .patch_document(job.paperless_document_id, &patch)
        .await
    {
        let duration_ms = apply_started.elapsed().as_millis() as u64;
        append_audit(
            pool,
            AuditEventInput {
                event_type: "document.patch_apply_failed".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: Some(job.run_id),
                job_id: Some(job.id),
                paperless_document_id: Some(job.paperless_document_id),
                before: Some(before_value),
                after: Some(patch_value),
                metadata: Some(json!({ "stage": job.stage, "duration_ms": duration_ms })),
                outcome: "failed".to_owned(),
                error_message: Some(error.to_string()),
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
        return Err(error);
    }
    let duration_ms = apply_started.elapsed().as_millis() as u64;
    append_audit(
        pool,
        AuditEventInput {
            event_type: "document.patch_applied".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: Some(before_value),
            after: Some(patch_value),
            metadata: Some(json!({ "stage": job.stage, "duration_ms": duration_ms })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    Ok(())
}

/// Decide whether the autopilot review drain should run on this tick.
///
/// Returns `Some(budget)` when the drain is allowed:
/// - `budget = None`   means "no per-tick cap" (unlimited)
/// - `budget = Some(n)` means "at most n items this tick" (n > 0)
///
/// Returns `None` when the drain must skip — mode is not `FullAuto`, dry-run
/// is on, the workflow is paused, or the safety budget is exhausted.
///
/// Kept as a pure function (no DB / IO) so it is unit-testable.
fn autopilot_drain_budget(
    settings: &RuntimeSettings,
    safety: &archivist_core::WorkflowSafetyStatus,
) -> Option<Option<i64>> {
    if !settings.workflow.mode.auto_apply_validated_suggestions() {
        return None;
    }
    if settings.workflow.dry_run {
        return None;
    }
    if safety.paused {
        return None;
    }
    let budget = selector_document_budget(safety);
    match budget {
        None => Some(None),
        Some(remaining) if remaining > 0 => Some(Some(remaining)),
        Some(_) => None,
    }
}

/// Drain pending review_items by auto-applying them when the runtime is in
/// full_auto. Complements the per-run `handle_patch_result` routing fix: if
/// items were queued under manual_review and the operator later flipped to
/// full_auto, those rows would otherwise sit in `pending` forever. The drain
/// is gated by the same safety dials the auto-selector honors (paused, dry
/// run, hourly + daily document limits).
async fn drain_pending_reviews_if_autopilot(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
) -> Result<usize> {
    let safety = get_workflow_safety_status(pool, settings).await?;
    let Some(budget) = autopilot_drain_budget(settings, &safety) else {
        return Ok(0);
    };
    // Hard ceiling per tick. Bumped from 50 to 100 in v1.3.2 after the
    // first production deployment showed the original cap drained too
    // slowly on backlogs in the multi-thousand range. Still safety-budget
    // bounded, so an operator hourly cap of e.g. 200/h continues to land
    // ~200 items/h regardless of this ceiling.
    const PER_TICK_CEILING: i64 = 100;
    let limit = match budget {
        None => PER_TICK_CEILING,
        Some(remaining) => remaining.min(PER_TICK_CEILING),
    };
    if limit <= 0 {
        return Ok(0);
    }
    let pending = list_pending_review_items_for_autopilot_drain(pool, limit).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let paperless = paperless_client(pool, config, settings).await?;

    // Hoist the tag list out of the per-item loop. The v1.3.1 drain called
    // `paperless.list_tags()` AND `paperless.ensure_tag()` (which itself
    // calls `list_tags` internally) on every iteration. With paginated
    // tag responses that's a multi-second cost per item; on a 4000-item
    // backlog the per-tick deadline ran out before more than 1-2 items
    // were applied. We snapshot tags once per drain batch, ensure all
    // workflow tags we might need, and reuse them per item. New tags
    // created during the batch are appended to the local snapshot.
    let mut tag_cache = paperless.list_tags().await?;
    let completion_full = ensure_tag_cached(
        &paperless,
        &mut tag_cache,
        &settings.workflow.tags.completion_processed,
    )
    .await?;

    let mut applied = 0usize;
    for review in pending {
        let review_id = review.id;
        let paperless_document_id = review.paperless_document_id;
        // Per-item timeout: keep one slow Paperless call from holding up
        // the whole batch. The PATCH itself rarely blocks for more than a
        // second or two; 45s gives even a sluggish or rate-limited
        // Paperless time to respond before we move on and let the row
        // retry on the next tick.
        let result = timeout(
            Duration::from_secs(45),
            apply_one_autopilot_drain_review(
                pool,
                &paperless,
                settings,
                review,
                &mut tag_cache,
                completion_full.clone(),
            ),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow!("per-item drain timeout after 45s")));
        match result {
            Ok(true) => {
                applied += 1;
                info!(
                    %review_id,
                    paperless_document_id,
                    trigger = "autopilot_drain",
                    "autopilot drain applied pending review item"
                );
            }
            Ok(false) => {
                // Raced — another worker tick (or a human reviewer) claimed
                // the row first. Not an error.
            }
            Err(error) => {
                warn!(
                    %review_id,
                    paperless_document_id,
                    error = %error,
                    "autopilot drain failed to apply review item; row returned to pending"
                );
            }
        }
    }
    Ok(applied)
}

/// Local cache helper for the drain: look up a workflow tag by name in the
/// pre-fetched tag list, creating it on Paperless (and inserting into the
/// cache) only if it really isn't there yet. Replaces the per-item
/// `paperless.ensure_tag()` call that re-fetched the whole tag page.
async fn ensure_tag_cached(
    paperless: &PaperlessClient,
    cache: &mut Vec<archivist_paperless::PaperlessTag>,
    name: &str,
) -> Result<archivist_paperless::PaperlessTag> {
    if let Some(tag) = cache.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
        return Ok(tag.clone());
    }
    let created = paperless.ensure_tag(name).await?;
    if !cache.iter().any(|t| t.id == created.id) {
        cache.push(created.clone());
    }
    Ok(created)
}

async fn apply_one_autopilot_drain_review(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    review: ReviewItemRecord,
    tag_cache: &mut Vec<archivist_paperless::PaperlessTag>,
    completion_full: archivist_paperless::PaperlessTag,
) -> Result<bool> {
    let Some(claimed) = claim_pending_review_for_autopilot_drain(pool, review.id).await? else {
        // Raced — the row is no longer pending.
        return Ok(false);
    };
    if let Err(error) = apply_autopilot_drain_patch(
        pool,
        paperless,
        settings,
        &claimed,
        tag_cache,
        &completion_full,
    )
    .await
    {
        // Roll the row back to pending so the next tick can retry. We
        // deliberately don't audit a failure event here — Paperless errors
        // already get an `document.patch_apply_failed` audit inside
        // `apply_autopilot_drain_patch`; non-Paperless errors are rare and
        // logged via the caller's `warn!`.
        if let Err(revert_error) =
            revert_review_to_pending_after_failed_drain(pool, claimed.id).await
        {
            warn!(
                review_id = %claimed.id,
                error = %revert_error,
                "failed to revert review item to pending after drain failure"
            );
        }
        return Err(error);
    }
    mark_review_auto_applied(pool, claimed.id).await?;
    Ok(true)
}

async fn apply_autopilot_drain_patch(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    review: &ReviewItemRecord,
    tag_cache: &mut Vec<archivist_paperless::PaperlessTag>,
    completion_full: &archivist_paperless::PaperlessTag,
) -> Result<()> {
    let patch_value = review
        .edited_patch
        .clone()
        .unwrap_or_else(|| review.suggested_patch.clone());
    let mut patch: DocumentPatch = serde_json::from_value(patch_value)?;
    let final_run_stage = if let Some(job_id) = review.job_id {
        is_last_active_job(pool, review.run_id, job_id).await?
    } else {
        false
    };
    let document = paperless.get_document(review.paperless_document_id).await?;
    let mut tag_ids = patch.tags.clone().unwrap_or_else(|| document.tags.clone());
    if let Some(completion_name) = settings
        .workflow
        .tags
        .completion_tag_for_stage(review.stage)
    {
        let tag = ensure_tag_cached(paperless, tag_cache, completion_name).await?;
        if !tag_ids.contains(&tag.id) {
            tag_ids.push(tag.id);
        }
    }
    if final_run_stage && !tag_ids.contains(&completion_full.id) {
        tag_ids.push(completion_full.id);
    }
    if let Some(trigger_name) = settings.workflow.tags.trigger_tag_for_stage(review.stage)
        && let Some(tag) = tag_cache
            .iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(trigger_name))
    {
        tag_ids.retain(|id| *id != tag.id);
    }
    if final_run_stage
        && let Some(tag) = tag_cache.iter().find(|tag| {
            tag.name
                .eq_ignore_ascii_case(&settings.workflow.tags.trigger_process)
        })
    {
        tag_ids.retain(|id| *id != tag.id);
    }
    tag_ids.sort_unstable();
    tag_ids.dedup();
    patch.tags = Some(tag_ids);
    prune_unchanged_patch_fields(&mut patch, &document);
    let before = audit_before_for_patch(&document, &patch);
    let after = audit_patch_payload(&patch);
    let apply_started = std::time::Instant::now();
    if let Err(error) = paperless
        .patch_document(review.paperless_document_id, &patch)
        .await
    {
        let duration_ms = apply_started.elapsed().as_millis() as u64;
        append_audit(
            pool,
            AuditEventInput {
                event_type: "document.patch_apply_failed".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: Some(review.run_id),
                job_id: review.job_id,
                paperless_document_id: Some(review.paperless_document_id),
                before: Some(before),
                after: Some(after),
                metadata: Some(json!({
                    "stage": review.stage,
                    "review_id": review.id,
                    "duration_ms": duration_ms,
                    "trigger": "autopilot_drain"
                })),
                outcome: "failed".to_owned(),
                error_message: Some(error.to_string()),
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
        return Err(error);
    }
    let duration_ms = apply_started.elapsed().as_millis() as u64;
    append_audit(
        pool,
        AuditEventInput {
            event_type: "document.patch_applied".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(review.run_id),
            job_id: review.job_id,
            paperless_document_id: Some(review.paperless_document_id),
            before: Some(before),
            after: Some(after),
            metadata: Some(json!({
                "stage": review.stage,
                "review_id": review.id,
                "duration_ms": duration_ms,
                "trigger": "autopilot_drain"
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    Ok(())
}

fn prune_unchanged_patch_fields(patch: &mut DocumentPatch, document: &PaperlessDocumentDetail) {
    if patch.content.as_deref() == document.content.as_deref() {
        patch.content = None;
    }
    if patch.title == document.title {
        patch.title = None;
    }
    if patch
        .tags
        .as_ref()
        .is_some_and(|tags| same_i32_set(tags, &document.tags))
    {
        patch.tags = None;
    }
    if patch
        .correspondent
        .as_ref()
        .is_some_and(|value| *value == document.correspondent)
    {
        patch.correspondent = None;
    }
    if patch
        .document_type
        .as_ref()
        .is_some_and(|value| *value == document.document_type)
    {
        patch.document_type = None;
    }
    if patch
        .created
        .as_deref()
        .is_some_and(|value| document_date_equals(document.created.as_deref(), value))
    {
        patch.created = None;
    }
}

fn same_i32_set(left: &[i32], right: &[i32]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort_unstable();
    left.dedup();
    right.sort_unstable();
    right.dedup();
    left == right
}

fn document_date_equals(current: Option<&str>, requested: &str) -> bool {
    current
        .map(|value| value.get(..10).unwrap_or(value) == requested)
        .unwrap_or(false)
}

fn audit_before_for_patch(
    document: &PaperlessDocumentDetail,
    patch: &DocumentPatch,
) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    if patch.content.is_some() {
        object.insert(
            "content".to_owned(),
            audit_text_metadata(document.content.as_deref().unwrap_or_default()),
        );
    }
    if patch.title.is_some() {
        object.insert("title".to_owned(), json!(document.title));
    }
    if patch.tags.is_some() {
        object.insert("tags".to_owned(), json!(document.tags));
    }
    if patch.correspondent.is_some() {
        object.insert("correspondent".to_owned(), json!(document.correspondent));
    }
    if patch.document_type.is_some() {
        object.insert("document_type".to_owned(), json!(document.document_type));
    }
    if patch.created.is_some() {
        object.insert("created".to_owned(), json!(document.created));
    }
    if patch.custom_fields.is_some() {
        object.insert("custom_fields".to_owned(), json!({ "present": "redacted" }));
    }
    serde_json::Value::Object(object)
}

fn audit_patch_payload(patch: &DocumentPatch) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    if let Some(content) = &patch.content {
        object.insert("content".to_owned(), audit_text_metadata(content));
    }
    if let Some(title) = &patch.title {
        object.insert("title".to_owned(), json!(title));
    }
    if let Some(tags) = &patch.tags {
        object.insert("tags".to_owned(), json!(tags));
    }
    if let Some(correspondent) = &patch.correspondent {
        object.insert("correspondent".to_owned(), json!(correspondent));
    }
    if let Some(document_type) = &patch.document_type {
        object.insert("document_type".to_owned(), json!(document_type));
    }
    if let Some(created) = &patch.created {
        object.insert("created".to_owned(), json!(created));
    }
    if let Some(custom_fields) = &patch.custom_fields {
        object.insert(
            "custom_fields".to_owned(),
            json!({
                "sha256": hash_text(&custom_fields.to_string()),
                "redacted": true
            }),
        );
    }
    serde_json::Value::Object(object)
}

fn audit_text_metadata(value: &str) -> serde_json::Value {
    json!({
        "sha256": hash_text(value),
        "chars": value.chars().count(),
        "redacted": true
    })
}

async fn poll_paperless_triggers(pool: &DbPool, config: &AppConfig) -> Result<()> {
    let settings = get_runtime_settings(pool).await?;
    if settings.workflow.paused {
        append_audit(
            pool,
            AuditEventInput {
                event_type: "workflow.selector_skipped".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: None,
                after: None,
                metadata: Some(json!({ "reason": "paused", "mode": settings.workflow.mode })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
        info!("trigger polling skipped because workflow is paused");
        return Ok(());
    }
    let paperless = paperless_client(pool, config, &settings).await?;
    let snapshot = sync_metadata(pool, &paperless, &settings).await?;

    let mut trigger_matches = 0_u64;
    // O(1) tag lookups per document — avoids a quadratic scan when both
    // the document set and the tag catalog are large.
    let tags_by_id: HashMap<i32, &PaperlessTag> =
        snapshot.tags.iter().map(|tag| (tag.id, tag)).collect();
    for document in snapshot.documents {
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tags_by_id.get(id).copied())
            .map(|tag| tag.name.clone())
            .collect::<Vec<_>>();
        let stages = settings.workflow.tags.stages_requested_by_tags(&tag_names);
        if !stages.is_empty() {
            trigger_matches += 1;
            let trigger = if tag_names
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case(&settings.workflow.tags.trigger_process))
            {
                settings.workflow.tags.trigger_process.as_str()
            } else {
                "paperless-trigger"
            };
            create_run_with_jobs(
                pool,
                document.id,
                &stages,
                settings.workflow.mode,
                trigger,
                "worker",
            )
            .await?;
        }
    }
    info!(
        trigger_matches,
        "trigger polling inspected Paperless documents"
    );
    if settings.workflow.mode.auto_select_documents() {
        let safety = get_workflow_safety_status(pool, &settings).await?;
        let document_budget = selector_document_budget(&safety);
        if document_budget.is_some_and(|remaining| remaining <= 0) {
            append_audit(
                pool,
                AuditEventInput {
                    event_type: "workflow.selector_limit_reached".to_owned(),
                    actor_type: "worker".to_owned(),
                    actor_id: None,
                    run_id: None,
                    job_id: None,
                    paperless_document_id: None,
                    before: None,
                    after: None,
                    metadata: Some(json!({
                        "hourly_document_limit": safety.hourly_document_limit,
                        "daily_document_limit": safety.daily_document_limit,
                        "hourly_remaining": safety.hourly_remaining,
                        "daily_remaining": safety.daily_remaining
                    })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
            info!("auto-selector skipped because document limit is exhausted");
            return Ok(());
        }
        let auto_selected = queue_missing_pipeline(
            pool,
            &settings.workflow.enabled_stages,
            settings.workflow.mode,
            "auto-selector",
            "worker",
            &settings.workflow.rules,
            document_budget,
        )
        .await?;
        append_audit(
            pool,
            AuditEventInput {
                event_type: "workflow.selector_ran".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: None,
                after: Some(json!({ "queued": auto_selected })),
                metadata: Some(json!({
                    "mode": settings.workflow.mode,
                    "dry_run": settings.workflow.dry_run,
                    "hourly_remaining": safety.hourly_remaining,
                    "daily_remaining": safety.daily_remaining
                })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
        info!(
            auto_selected,
            mode = %settings.workflow.mode,
            "auto-selector queued missing document stages"
        );
    }
    Ok(())
}

struct PaperlessSyncSnapshot {
    tags: Vec<PaperlessTag>,
    documents: Vec<PaperlessDocumentSummary>,
}

async fn sync_metadata(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
) -> Result<PaperlessSyncSnapshot> {
    let mut tags = paperless.list_tags().await?;
    for workflow_tag in settings.workflow.tags.all() {
        let tag = paperless.ensure_tag(workflow_tag).await?;
        if !tags.iter().any(|existing| existing.id == tag.id) {
            tags.push(tag);
        }
    }
    let correspondents = paperless.list_correspondents().await?;
    let document_types = paperless.list_document_types().await?;
    let custom_fields = paperless.list_custom_fields().await.unwrap_or_default();
    let documents = paperless.list_documents().await?;

    let mut tx = pool.begin().await?;
    for tag in &tags {
        upsert_paperless_tag(
            &mut tx,
            tag.id,
            &tag.name,
            tag.slug.as_deref(),
            tag.color.as_deref(),
            settings.workflow.tags.is_workflow_tag(&tag.name),
        )
        .await?;
    }
    for entity in &correspondents {
        upsert_paperless_named_entity(&mut tx, "paperless_correspondents", entity.id, &entity.name)
            .await?;
    }
    for entity in &document_types {
        upsert_paperless_named_entity(&mut tx, "paperless_document_types", entity.id, &entity.name)
            .await?;
    }
    for field in &custom_fields {
        upsert_paperless_custom_field(&mut tx, field.id, &field.name, field.data_type.as_deref())
            .await?;
    }
    for document in &documents {
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tags.iter().find(|tag| tag.id == *id))
            .map(|tag| tag.name.clone())
            .collect::<Vec<_>>();
        upsert_inventory_item(
            &mut tx,
            &archivist_db::InventoryUpsert {
                paperless_document_id: document.id,
                title: document.title.clone(),
                original_file_name: document.original_file_name.clone(),
                current_tags: tag_names.clone(),
                current_tag_ids: document.tags.clone(),
                correspondent_id: document.correspondent,
                document_type_id: document.document_type,
                document_date: document.created.clone(),
                paperless_modified_at: None,
                has_ocr_completion_tag: tag_names
                    .iter()
                    .any(|tag| tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_ocr)),
                has_tagging_completion_tag: tag_names.iter().any(|tag| {
                    tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_tagging)
                }),
                has_full_completion_tag: tag_names.iter().any(|tag| {
                    tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_processed)
                }),
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(PaperlessSyncSnapshot { tags, documents })
}

async fn paperless_client(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
) -> Result<PaperlessClient> {
    let active_profile = settings.paperless.archive_profiles.iter().find(|profile| {
        profile.enabled
            && profile
                .name
                .eq_ignore_ascii_case(&settings.paperless.active_archive)
    });
    let base_url = active_profile
        .map(|profile| profile.base_url.as_str())
        .unwrap_or(&settings.paperless.base_url);
    let secret_id = active_profile
        .and_then(|profile| profile.token_secret_id)
        .or(settings.paperless.token_secret_id)
        .ok_or_else(|| anyhow!("Paperless token is not configured"))?;
    let token = resolve_secret(pool, &config.secret_key, secret_id)
        .await?
        .ok_or_else(|| anyhow!("Paperless token secret reference does not exist"))?;
    PaperlessClient::new(base_url, token, settings.paperless.timeout_seconds)
}

#[derive(Debug, Clone)]
struct StageProvider {
    name: String,
    kind: AiProviderKind,
    base_url: String,
    model: String,
    secret_id: Option<Uuid>,
}

fn provider_for_stage(
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
        .find(|provider| provider.enabled && provider.name == provider_name)
        .cloned()
        .or_else(|| {
            if provider_name == "ollama" {
                Some(archivist_core::AiProviderSettings::ollama_default())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("AI provider '{provider_name}' is not configured or disabled"))?;
    if provider.name == "ollama" {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings
        .ai
        .model_for_stage_provider(&provider, stage, vision);
    let base_url = provider_base_url(&provider.kind, &provider.base_url);
    Ok(StageProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
    })
}

fn provider_base_url(kind: &AiProviderKind, configured: &str) -> String {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return trimmed.trim_end_matches('/').to_owned();
    }
    match kind {
        AiProviderKind::Ollama => "http://ollama:11434".to_owned(),
        AiProviderKind::Openai => "https://api.openai.com/v1".to_owned(),
        AiProviderKind::Anthropic => "https://api.anthropic.com/v1".to_owned(),
        AiProviderKind::OpenaiCompatible => "http://localhost:8000/v1".to_owned(),
    }
}

async fn apply_active_prompt(
    pool: &DbPool,
    stage: Stage,
    request: &mut ChatRequest,
) -> Result<Option<Uuid>> {
    let Some(prompt) = get_active_prompt(pool, stage).await? else {
        return Ok(None);
    };
    request.system_prompt = prompt.content;
    Ok(Some(prompt.id))
}

async fn chat_for_stage(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
    stage: Stage,
    mut request: ChatRequest,
) -> Result<AiResponse> {
    let provider = provider_for_stage(settings, stage, false)?;
    request.model = provider.model.clone();
    chat_with_provider(pool, config, &provider, request).await
}

async fn chat_with_provider(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
    request: ChatRequest,
) -> Result<AiResponse> {
    match provider.kind {
        AiProviderKind::Ollama => {
            let client = OllamaClient::new(
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new(
                &provider.name,
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Anthropic => {
            let secret = provider_secret(pool, config, provider)
                .await?
                .ok_or_else(|| {
                    anyhow!("AI provider '{}' requires an API key secret", provider.name)
                })?;
            let client = AnthropicClient::new(&provider.name, &provider.base_url, secret)?;
            client.chat(request).await
        }
    }
}

async fn vision_with_provider(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
    request: VisionRequest,
) -> Result<AiResponse> {
    match provider.kind {
        AiProviderKind::Ollama => {
            let client = OllamaClient::new(
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
            )?;
            client.vision(request).await
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new(
                &provider.name,
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
            )?;
            client.vision(request).await
        }
        AiProviderKind::Anthropic => {
            let secret = provider_secret(pool, config, provider)
                .await?
                .ok_or_else(|| {
                    anyhow!("AI provider '{}' requires an API key secret", provider.name)
                })?;
            let client = AnthropicClient::new(&provider.name, &provider.base_url, secret)?;
            client.vision(request).await
        }
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

fn hash_text(value: &str) -> String {
    hash_bytes(value.as_bytes())
}

fn hash_bytes(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn typed_paperless_errors_drive_classification() {
        let transient: anyhow::Error =
            anyhow::Error::new(PaperlessError::Timeout("waiting for paperless".to_owned()))
                .context("higher-level wrap that does not mention transient keywords");
        assert!(matches!(
            classify_processing_failure(&transient),
            ProcessingFailureClass::Transient
        ));

        let permanent: anyhow::Error = anyhow::Error::new(PaperlessError::Client {
            status: 422,
            body: "no transient keyword here".to_owned(),
        });
        assert!(matches!(
            classify_processing_failure(&permanent),
            ProcessingFailureClass::Permanent
        ));
    }

    #[test]
    fn typed_ai_errors_drive_classification() {
        let transient: anyhow::Error =
            anyhow::Error::new(AiProviderError::RunnerUnavailable("ollama".to_owned()));
        assert!(matches!(
            classify_processing_failure(&transient),
            ProcessingFailureClass::Transient
        ));

        let permanent: anyhow::Error = anyhow::Error::new(AiProviderError::InvalidResponse(
            "unexpected shape".to_owned(),
        ));
        assert!(matches!(
            classify_processing_failure(&permanent),
            ProcessingFailureClass::Permanent
        ));
    }

    #[test]
    fn fallback_substring_matching_still_classifies_untyped_errors() {
        let transient: anyhow::Error = anyhow!("pool timed out waiting for connection");
        assert!(matches!(
            classify_processing_failure(&transient),
            ProcessingFailureClass::Transient
        ));
        let permanent: anyhow::Error = anyhow!("invalid configuration: missing field");
        assert!(matches!(
            classify_processing_failure(&permanent),
            ProcessingFailureClass::Permanent
        ));
    }

    fn document_detail() -> PaperlessDocumentDetail {
        PaperlessDocumentDetail {
            id: 42,
            title: Some("Existing title".to_owned()),
            created: Some("2026-03-14".to_owned()),
            modified: Some("2026-03-15T10:00:00Z".to_owned()),
            content: Some("private OCR text".to_owned()),
            tags: vec![1, 2],
            correspondent: Some(7),
            document_type: Some(9),
            original_file_name: Some("document.pdf".to_owned()),
        }
    }

    #[test]
    fn unchanged_standard_metadata_is_pruned_before_patch() {
        let document = document_detail();
        let mut patch = DocumentPatch {
            content: None,
            title: Some("Existing title".to_owned()),
            tags: Some(vec![2, 1]),
            correspondent: Some(Some(7)),
            document_type: Some(Some(9)),
            created: Some("2026-03-14".to_owned()),
            custom_fields: None,
        };

        prune_unchanged_patch_fields(&mut patch, &document);

        assert!(patch.is_empty());
    }

    #[test]
    fn audit_payload_redacts_content_and_custom_fields() {
        let patch = DocumentPatch {
            content: Some("private OCR text".to_owned()),
            title: Some("New title".to_owned()),
            tags: Some(vec![1, 2, 3]),
            correspondent: Some(Some(7)),
            document_type: None,
            created: Some("2026-03-14".to_owned()),
            custom_fields: Some(json!([{ "field": 1, "value": "private value" }])),
        };

        let audit = audit_patch_payload(&patch);
        assert_eq!(audit["content"]["redacted"], Value::Bool(true));
        assert_eq!(audit["content"]["chars"], Value::from(16));
        assert!(audit["content"].get("sha256").is_some());
        assert_eq!(audit["custom_fields"]["redacted"], Value::Bool(true));
        assert!(!audit.to_string().contains("private OCR text"));
        assert!(!audit.to_string().contains("private value"));
    }

    #[test]
    fn classifies_integration_interruptions_as_transient() {
        let cases = [
            anyhow!("Paperless request timed out while downloading original"),
            anyhow!(
                "Ollama vision returned 500 Internal Server Error: runner process no longer running"
            ),
            anyhow!("PostgreSQL database pool timed out while claiming jobs"),
        ];

        for error in cases {
            assert_eq!(
                classify_processing_failure(&error),
                ProcessingFailureClass::Transient
            );
        }
    }

    #[test]
    fn classifies_validation_and_configuration_errors_as_permanent() {
        let cases = [
            anyhow!("Paperless returned 406 Not Acceptable"),
            anyhow!("model response did not contain valid JSON"),
            anyhow!("unknown allowed tag returned by model"),
        ];

        for error in cases {
            assert_eq!(
                classify_processing_failure(&error),
                ProcessingFailureClass::Permanent
            );
        }
    }

    fn unrestricted_safety() -> archivist_core::WorkflowSafetyStatus {
        archivist_core::WorkflowSafetyStatus {
            paused: false,
            dry_run: false,
            hourly_document_limit: None,
            daily_document_limit: None,
            hourly_remaining: None,
            daily_remaining: None,
        }
    }

    #[test]
    fn autopilot_drain_runs_under_full_auto_with_unlimited_budget() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        settings.workflow.dry_run = false;
        let safety = unrestricted_safety();
        // Unlimited budget → drain is allowed and the per-tick cap is the
        // only ceiling.
        assert_eq!(autopilot_drain_budget(&settings, &safety), Some(None));
    }

    #[test]
    fn autopilot_drain_skips_when_mode_is_manual_review() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::ManualReview;
        let safety = unrestricted_safety();
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_skips_when_mode_is_auto_select_review() {
        let mut settings = RuntimeSettings::default();
        // AutoSelectReview enables auto-selection but still requires human
        // review — drain must not auto-apply under this mode.
        settings.workflow.mode = ProcessingMode::AutoSelectReview;
        let safety = unrestricted_safety();
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_skips_when_dry_run_is_enabled() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        settings.workflow.dry_run = true;
        let safety = unrestricted_safety();
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_skips_when_workflow_is_paused() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        let mut safety = unrestricted_safety();
        safety.paused = true;
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_skips_when_safety_budget_is_exhausted() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        let mut safety = unrestricted_safety();
        safety.hourly_document_limit = Some(100);
        safety.daily_document_limit = Some(1000);
        safety.hourly_remaining = Some(0);
        safety.daily_remaining = Some(500);
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_caps_at_smaller_of_hourly_or_daily() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        let mut safety = unrestricted_safety();
        safety.hourly_document_limit = Some(50);
        safety.daily_document_limit = Some(200);
        safety.hourly_remaining = Some(7);
        safety.daily_remaining = Some(120);
        // Drain budget is the smaller of the two remaining quotas.
        assert_eq!(autopilot_drain_budget(&settings, &safety), Some(Some(7)));
    }
}
