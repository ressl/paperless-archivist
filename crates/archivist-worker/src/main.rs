use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use archivist_ai::{
    AiResponse, AnthropicClient, ChatRequest, DEFAULT_OCR_SYSTEM_PROMPT, ImageInput, OllamaClient,
    OpenAiCompatibleClient, PromptLanguageContext, TextProvider, VisionProvider, VisionRequest,
    parse_choice_suggestion, parse_field_suggestion, parse_tag_suggestion, parse_title_suggestion,
    prompt_for_choice, prompt_for_fields, prompt_for_tags, prompt_for_title,
};
use archivist_config::AppConfig;
use archivist_core::{
    AiProviderKind, AuditEventInput, ChoiceSuggestion, DocumentPatch, LanguageDetection,
    OldTagStrategy, ProcessingMode, RuntimeSettings, Stage, TagSuggestion, TitleSuggestion,
    detect_document_language, extract_issue_date_suggestion, validate_choice_suggestion,
    validate_document_date_suggestion, validate_field_suggestion, validate_tag_suggestion,
    validate_title_suggestion,
};
use archivist_db::{
    AiArtifactInput, DbPool, JobRecord, append_audit, claim_jobs, claim_notification_delivery,
    complete_job, connect, create_review_item, create_run_with_jobs, custom_field_ids_for_names,
    fail_job, get_active_prompt, get_backlog_counts, get_dashboard_live_status,
    get_runtime_settings, get_workflow_safety_status, insert_ai_artifact, is_last_active_job,
    list_allowed_named_entities, list_allowed_tag_names, list_custom_fields,
    named_entity_id_for_name, queue_missing_pipeline, record_document_language, resolve_secret,
    selector_document_budget, tag_ids_for_names, upsert_inventory_item,
    upsert_paperless_custom_field, upsert_paperless_named_entity, upsert_paperless_tag,
};
use archivist_ocr::{normalize_ocr_pages, render_document_pages, validate_ocr_text};
use archivist_paperless::{
    PaperlessClient, PaperlessDocumentDetail, PaperlessDocumentSummary, PaperlessTag,
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

    let mut pending = FuturesUnordered::new();
    for job in jobs {
        let pool = pool.clone();
        let config = Arc::clone(config);
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
                let result = process_job(&pool, &config, &job).await;
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

fn classify_processing_failure(error: &anyhow::Error) -> ProcessingFailureClass {
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

async fn process_job(pool: &DbPool, config: &AppConfig, job: &JobRecord) -> Result<()> {
    info!(job_id = %job.id, run_id = %job.run_id, document_id = job.paperless_document_id, stage = %job.stage, "processing job");
    let settings = get_runtime_settings(pool).await?;
    let paperless = paperless_client(pool, config, &settings).await?;

    match job.stage {
        Stage::Ocr => process_ocr(pool, config, &paperless, &settings, job).await,
        Stage::Tags => process_tags(pool, config, &paperless, &settings, job).await,
        Stage::Title => process_title(pool, config, &paperless, &settings, job).await,
        Stage::Correspondent => {
            process_choice(
                pool,
                config,
                &paperless,
                &settings,
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
                &paperless,
                &settings,
                job,
                "document type",
                "paperless_document_types",
            )
            .await
        }
        Stage::DocumentDate => process_document_date(pool, &paperless, &settings, job).await,
        Stage::Fields => process_fields(pool, config, &paperless, &settings, job).await,
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

async fn handle_patch_result(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    patch: DocumentPatch,
    warnings: Vec<String>,
    review_metadata: Option<serde_json::Value>,
) -> Result<()> {
    if job.mode.requires_manual_review() || settings.workflow.dry_run {
        let mut review_patch = serde_json::to_value(patch)?;
        if let Some(metadata) = review_metadata
            && let Some(object) = review_patch.as_object_mut()
        {
            object.insert("standard_metadata".to_owned(), metadata);
        }
        let mut review_warnings = warnings;
        if settings.workflow.dry_run && !job.mode.requires_manual_review() {
            review_warnings.push(
                "Dry-run is enabled: validated patch was evaluated but not auto-applied."
                    .to_owned(),
            );
        }
        let review_id = create_review_item(pool, job, review_patch, json!(review_warnings)).await?;
        if settings.workflow.dry_run && !job.mode.requires_manual_review() {
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
}
