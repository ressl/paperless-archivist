//! Metadata stage: prompts the text model, validates the suggestion and routes it
//! to review or auto-apply (including consensus checks).

use anyhow::{Result, anyhow};
use archivist_ai::{
    AiResponse, ChatRequest, MetadataParseDiagnostics, MetadataParseStatus, PromptLanguageContext,
    parse_metadata_suggestion, prompt_for_metadata,
};
use archivist_apply::review_apply_baseline;
use archivist_config::AppConfig;
use archivist_core::{
    AuditEventInput, DocumentPatch, LanguageDetection, MetadataFieldFlags, MetadataSuggestion,
    RuntimeSettings, Stage, detect_document_language, validate_choice_suggestion,
    validate_document_date_suggestion, validate_field_suggestion, validate_tag_suggestion,
    validate_title_suggestion,
};
use archivist_db::{
    AiArtifactInput, DbPool, JobRecord, append_audit, complete_job, create_review_item,
    custom_field_ids_for_names, insert_ai_artifact, is_last_active_job,
    list_allowed_named_entities, list_allowed_tag_names, list_custom_fields,
    named_entity_id_for_name, record_document_language, tag_id_pairs_for_names, tag_ids_for_names,
};
use archivist_paperless::PaperlessClient;
use serde_json::json;
use tracing::{info, warn};
use uuid::Uuid;

use crate::apply::{
    apply_patch_with_workflow_tags, retire_trigger_tags_after_terminal_outcome,
    tags_for_old_tag_strategy,
};
use crate::failure::{ProcessingFailureClass, classify_processing_failure};
use crate::lease::job_lease_seconds;
use crate::providers::{
    apply_active_prompt_with_experiment, apply_retry_prompt, chat_for_stage, chat_with_provider,
    classify_document_type, ollama_text_num_ctx_for_provider, provider_for_stage,
};
use crate::{hash_bytes, hash_text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataWorkerParseRoute {
    ProcessSuggestion,
    CompleteOmission,
    RetryContractViolation,
}

fn metadata_worker_parse_route(diagnostics: &MetadataParseDiagnostics) -> MetadataWorkerParseRoute {
    match diagnostics.status {
        MetadataParseStatus::Valid => MetadataWorkerParseRoute::ProcessSuggestion,
        MetadataParseStatus::Omitted => MetadataWorkerParseRoute::CompleteOmission,
        MetadataParseStatus::ContractViolation => MetadataWorkerParseRoute::RetryContractViolation,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_terminal_metadata_parse_route(
    pool: &DbPool,
    job: &JobRecord,
    lease_owner: &str,
    settings: &RuntimeSettings,
    response: &AiResponse,
    request: &ChatRequest,
    prompt_id: Option<Uuid>,
    content: &str,
    suggestion: &MetadataSuggestion,
    diagnostics: &MetadataParseDiagnostics,
) -> Result<bool> {
    let route = metadata_worker_parse_route(diagnostics);
    if route == MetadataWorkerParseRoute::ProcessSuggestion {
        return Ok(false);
    }

    let mut normalized = serde_json::to_value(suggestion)?;
    if let Some(object) = normalized.as_object_mut() {
        object.insert(
            "parse_diagnostics".to_owned(),
            serde_json::to_value(diagnostics)?,
        );
    }
    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Metadata,
            provider: &response.provider,
            model: &response.model,
            prompt_id,
            input_hash: &hash_text(content),
            request: Some(serde_json::to_value(request)?),
            response: Some(response.raw_response.clone()),
            normalized_output: Some(normalized),
            duration_ms: response.duration_ms,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    match route {
        MetadataWorkerParseRoute::ProcessSuggestion => Ok(false),
        MetadataWorkerParseRoute::CompleteOmission => {
            if !complete_job(
                pool,
                job,
                lease_owner,
                json!({
                    "skipped": "metadata model omitted all enabled fields",
                    "skipped_fields": [],
                    "parse_diagnostics": diagnostics,
                }),
            )
            .await?
            {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    "lease lost before metadata-omission completion; another worker owns this job"
                );
            }
            Ok(true)
        }
        MetadataWorkerParseRoute::RetryContractViolation => {
            let contract_error = diagnostics.contract_error().ok_or_else(|| {
                anyhow!("metadata parser routing invariant violated: contract error missing")
            })?;
            Err(contract_error.into())
        }
    }
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

/// Pure split of `resolve_tag_names_to_ids`: given the requested names and the
/// (name, id) pairs that the local Paperless mirror already knows about, return
/// the set of known ids plus the list of names that were NOT found (and therefore
/// need creation-or-drop downstream depending on `allow_new_tags`).
///
/// Extracted so the diff/dedup/case-fold logic is unit-testable without a real
/// `DbPool` or `PaperlessClient`.
fn diff_known_tag_names(
    requested: &[String],
    known_pairs: &[(String, i32)],
) -> (Vec<i32>, Vec<String>) {
    let known_lower: std::collections::HashSet<String> = known_pairs
        .iter()
        .map(|(name, _)| archivist_core::fold_catalog_name(name))
        .collect();
    let mut ids: Vec<i32> = known_pairs.iter().map(|(_, id)| *id).collect();
    ids.sort_unstable();
    ids.dedup();
    let unknown: Vec<String> = requested
        .iter()
        .filter(|name| !known_lower.contains(&archivist_core::fold_catalog_name(name)))
        .cloned()
        .collect();
    (ids, unknown)
}

/// Resolve LLM-supplied tag NAMES to Paperless tag IDs so review_items always carry the
/// `Vec<i32>` shape that the approve → patch path expects (the apply path deserializes the
/// review_item's `suggested_patch.tags` as `Vec<i32>`; raw names cause a 500 there and the
/// autopilot drain then reverts the review forever).
///
/// Only names already present in the local `paperless_tags` mirror resolve to ids; nothing
/// is created here. Unknown names are returned so the caller can carry them as pending
/// names that are created at apply time (#404). Workflow tags are dropped from both lists
/// so model output can never set or re-add a trigger/completion tag (#403).
async fn resolve_known_tag_names(
    pool: &DbPool,
    names: &[String],
    workflow_tags: &archivist_core::WorkflowTags,
) -> Result<(Vec<i32>, Vec<String>)> {
    let names: Vec<String> = names
        .iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && !workflow_tags.is_workflow_tag(name))
        .collect();
    if names.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let known_pairs: Vec<(String, i32)> = tag_id_pairs_for_names(pool, &names)
        .await?
        .into_iter()
        .filter(|(name, _)| !workflow_tags.is_workflow_tag(name))
        .collect();
    Ok(diff_known_tag_names(&names, &known_pairs))
}

/// Pure split of `resolve_custom_field_values_to_ids`: build the
/// `[{ "field": id, "value": ... }]` JSON array Paperless expects from the input
/// FieldValueSuggestion list and the locally-known (name, id) pairs. Names not in
/// the pairs list are dropped. Extracted for unit testability.
fn build_custom_field_value_patch(
    fields: &[archivist_core::FieldValueSuggestion],
    id_pairs: &[(String, i32, Option<String>)],
) -> Vec<serde_json::Value> {
    fields
        .iter()
        .filter_map(|field| {
            let (_, id, data_type) = id_pairs
                .iter()
                .find(|(name, _, _)| archivist_core::catalog_names_equal(name, &field.name))?;
            match archivist_core::coerce_custom_field_value(data_type.as_deref(), &field.value) {
                Some(value) => Some(json!({ "field": id, "value": value })),
                None => {
                    warn!(
                        field = %field.name,
                        value = %field.value,
                        "dropped uncoercible custom field value"
                    );
                    None
                }
            }
        })
        .collect()
}

/// Resolve LLM-supplied custom-field NAMES to Paperless custom-field IDs. Same contract as
/// `resolve_tag_names_to_ids` but for custom fields. Unknown names are dropped with a warn
/// log — custom fields cannot be safely auto-created here because they require a `data_type`
/// the LLM doesn't reliably supply.
async fn resolve_custom_field_values_to_ids(
    pool: &DbPool,
    fields: &[archivist_core::FieldValueSuggestion],
) -> Result<Vec<serde_json::Value>> {
    if fields.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<String> = fields.iter().map(|field| field.name.clone()).collect();
    let id_pairs = custom_field_ids_for_names(pool, &names).await?;
    for field in fields {
        if !id_pairs
            .iter()
            .any(|(name, _, _)| archivist_core::catalog_names_equal(name, &field.name))
        {
            warn!(
                unknown_custom_field = %field.name,
                "dropping unknown custom field from review_item suggested_patch"
            );
        }
    }
    Ok(build_custom_field_value_patch(fields, &id_pairs))
}

/// Consolidated metadata stage (v1.4.0). One LLM call replaces six per-field
/// round-trips. The response is fanned out into up to six review items (or one
/// composite Paperless patch in full_auto mode) so existing reviewer UX, audit
/// trails, and per-field opt-outs keep working.
pub(crate) async fn process_metadata(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    lease_owner: &str,
) -> Result<()> {
    // #445: a review "retry with ..." job carries a provider/model/prompt
    // choice in its payload. It applies to this job only (and forces manual
    // review so the result returns to the reviewer); runtime settings stay
    // untouched.
    let retry_overrides = archivist_core::MetadataRetryOverrides::from_job_payload(&job.payload);
    let retry_settings = retry_overrides
        .as_ref()
        .map(|overrides| overrides.apply_to_settings(settings));
    let settings = retry_settings.as_ref().unwrap_or(settings);
    if let Some(overrides) = &retry_overrides {
        info!(
            job_id = %job.id,
            document_id = job.paperless_document_id,
            provider = overrides.provider_name.as_deref().unwrap_or("-"),
            model = overrides.model.as_deref().unwrap_or("-"),
            prompt_id = ?overrides.prompt_id,
            "metadata job runs with review retry overrides"
        );
    }
    let enabled = MetadataFieldFlags::from_enabled_stages(&settings.workflow.enabled_stages);
    if !enabled.any() {
        if !complete_job(
            pool,
            job,
            lease_owner,
            json!({ "skipped": "no metadata fields are enabled in workflow settings" }),
        )
        .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "lease lost before completion; another worker owns this job — skipping"
            );
        } else {
            // #400: terminal without a patch — retire the trigger tags.
            retire_trigger_tags_after_terminal_outcome(pool, paperless, settings, job, false).await;
        }
        return Ok(());
    }

    let document = paperless.get_document(job.paperless_document_id).await?;
    let content = document.content.clone().unwrap_or_default();

    // v1.5.14 (#117): content-hash dedup. If another document with the
    // same OCR-text sha256 has already had its metadata stage succeed,
    // we log the match and emit an audit event but keep running the
    // LLM call. This makes prod safe to enable: the dedup currently
    // serves as a signal-only feature (operators see the hit, but the
    // patch is still freshly LLM-derived). A future release can flip
    // this to a hard skip + clone of the source patch once we have
    // confidence the hash match is a reliable substitution.
    if !content.trim().is_empty() {
        let dedup_hash = hash_bytes(content.as_bytes());
        if let Some((source_id, _payload)) =
            archivist_db::find_metadata_dedup_source(pool, job.paperless_document_id, &dedup_hash)
                .await?
        {
            info!(
                document_id = job.paperless_document_id,
                dedup_source = source_id,
                "content-hash dedup match found (signal-only in v1.5.14)"
            );
            append_audit(
                pool,
                AuditEventInput {
                    event_type: "workflow.metadata_dedup_match".to_owned(),
                    actor_type: "worker".to_owned(),
                    actor_id: None,
                    run_id: Some(job.run_id),
                    job_id: Some(job.id),
                    paperless_document_id: Some(job.paperless_document_id),
                    before: None,
                    after: Some(json!({ "dedup_source": source_id })),
                    metadata: Some(json!({ "trigger": "content_hash" })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
        }
    }

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
    // Carry each field's data_type alongside its name so the prompt can
    // render a per-type formatting hint (#148). The schema still only needs
    // the names, so we derive a names-only view for `schema_for_metadata`
    // just before that call.
    let allowed_fields: Vec<(String, Option<String>)> = if enabled.fields {
        list_custom_fields(pool)
            .await?
            .into_iter()
            .filter(|field| settings.fields.field_enabled(&field.name))
            .map(|field| (field.name, field.data_type))
            .collect()
    } else {
        Vec::new()
    };

    // v1.5.12: pre-filter the closed-vocabulary lists by OCR-substring
    // frequency so the LLM gets the most relevant candidates and the prompt
    // stays under the token budget. Field names are typically a short curated
    // list so they bypass filtering.
    let allowed_list_max = settings.effective_tuning().allowed_list_max as usize;
    // Lowercase the (potentially large) content ONCE and share it across the
    // three prefilter passes instead of re-lowercasing it each time. #295
    let content_lower = content.to_lowercase();
    let allowed_correspondents = archivist_core::prefilter_allowed_list_lower(
        &content_lower,
        &allowed_correspondents,
        allowed_list_max,
    );
    let allowed_document_types = archivist_core::prefilter_allowed_list_lower(
        &content_lower,
        &allowed_document_types,
        allowed_list_max,
    );
    let allowed_tags = archivist_core::prefilter_allowed_list_lower(
        &content_lower,
        &allowed_tags,
        allowed_list_max,
    );

    // v1.5.13: cheap pre-pass classifier to give the main metadata prompt a
    // document-type-specific hint. Skips the call gracefully when content is
    // empty or the classifier fails — main prompt then runs without the hint.
    let doc_type_category = match classify_document_type(pool, config, settings, &content).await {
        Ok(category) => category,
        // A quota signal must propagate so the provider cooldown is persisted
        // and the stage isn't followed by a second doomed call against the
        // exhausted provider. Everything else degrades to the generic prompt. #280
        Err(error)
            if classify_processing_failure(&error) == ProcessingFailureClass::ProviderQuota =>
        {
            return Err(error);
        }
        Err(error) => {
            warn!(
                document_id = job.paperless_document_id,
                error = %error,
                "doc-type pre-pass failed; falling back to generic metadata prompt"
            );
            archivist_ai::DocTypeCategory::Other
        }
    };
    let doc_type_hint = archivist_ai::metadata_hint_for_doc_type(doc_type_category);
    info!(
        document_id = job.paperless_document_id,
        category = doc_type_category.as_str(),
        hint_chars = doc_type_hint.len(),
        "classified document type for metadata prompt"
    );

    let mut request = prompt_for_metadata(
        &content,
        &allowed_correspondents,
        &allowed_document_types,
        &allowed_tags,
        &allowed_fields,
        &enabled,
        &language,
        settings.effective_tuning().max_tags as usize,
        settings.fields.max_fields,
        doc_type_hint,
    );
    // v1.5.30: attach a JSON Schema that mirrors the prompt's
    // <output_schema> block. The Ollama client forwards it via the
    // `format` field of /api/chat, which lowers the schema to a GBNF
    // grammar and applies it during sampling — out-of-vocabulary tokens
    // become impossible to emit, so the closed-vocabulary
    // (document_type, correspondent, tags, custom-field names) gets
    // hard guarantees on top of the soft prompt constraints.
    // The OpenAI-compatible client forwards it as `response_format`
    // (json_schema/json_object per the provider's `structured_output`
    // tuning, with a one-shot schema-400 fallback), and the Anthropic
    // client as forced tool-use.
    let allowed_field_names: Vec<String> = allowed_fields
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    request.response_schema = archivist_ai::schema_for_metadata(
        &allowed_correspondents,
        &allowed_document_types,
        &allowed_tags,
        &allowed_field_names,
        &enabled,
        settings.effective_tuning().max_tags as usize,
        settings.fields.max_fields,
    );
    let retry_prompt_id = retry_overrides
        .as_ref()
        .and_then(|overrides| overrides.prompt_id);
    let (prompt_id, prompt_experiment_group) =
        match apply_retry_prompt(pool, retry_prompt_id, &mut request).await? {
            Some(prompt_id) => (Some(prompt_id), None),
            None => {
                apply_active_prompt_with_experiment(pool, Stage::Metadata, job.run_id, &mut request)
                    .await?
            }
        };
    // Heartbeat the lease before each long LLM call. The metadata stage can
    // chain classifier + main call + consensus (each up to the configured
    // request timeout) under one lease window; without renewing, a second
    // replica reclaims the "stale" job mid-stage and processes it concurrently.
    if !archivist_db::bump_job_lease(pool, job.id, lease_owner, job_lease_seconds(settings)).await?
    {
        warn!(
            job_id = %job.id,
            document_id = job.paperless_document_id,
            "metadata lease lost before main LLM call; stopping so a replica isn't duplicated"
        );
        return Ok(());
    }
    let response = chat_for_stage(pool, config, settings, Stage::Metadata, request.clone()).await?;
    let parsed = parse_metadata_suggestion(&response.text);
    let parse_diagnostics = parsed.diagnostics;
    let mut suggestion = parsed.suggestion;

    // Omission and contract errors are terminal before consensus/validation:
    // both persist the safe parse artifact, omission completes immediately,
    // and a violation returns the typed retryable error without applying any
    // retained valid subfields.
    if handle_terminal_metadata_parse_route(
        pool,
        job,
        lease_owner,
        settings,
        &response,
        &request,
        prompt_id,
        &content,
        &suggestion,
        &parse_diagnostics,
    )
    .await?
    {
        // #400: an omission completes the job without a patch.
        retire_trigger_tags_after_terminal_outcome(pool, paperless, settings, job, false).await;
        return Ok(());
    }

    // v1.5.15 (#118): two-model consensus check. When
    // `ai.consensus_secondary_text_model` is set AND we're heading for an
    // auto-apply (full_auto, not dry_run), fire a focused secondary call
    // against the configured cross-check model asking ONLY for
    // correspondent + document_date. Drop disagreeing fields from the
    // primary suggestion so they fall into review instead of being
    // silently auto-applied with an unverified value.
    let consensus_enabled = settings
        .effective_tuning()
        .consensus_secondary_text_model
        .as_deref()
        .map(str::trim)
        .is_some_and(|m| !m.is_empty())
        && settings.workflow.mode.auto_apply_validated_suggestions()
        && !settings.workflow.dry_run;
    let consensus_outcome = if consensus_enabled {
        if !archivist_db::bump_job_lease(pool, job.id, lease_owner, job_lease_seconds(settings))
            .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "metadata lease lost before consensus check; stopping so a replica isn't duplicated"
            );
            return Ok(());
        }
        Some(
            run_consensus_check(
                pool,
                config,
                settings,
                job,
                &content,
                &allowed_correspondents,
                &language,
                &mut suggestion,
            )
            .await?,
        )
    } else {
        None
    };

    let mut normalized = serde_json::to_value(&suggestion)?;
    if let Some(object) = normalized.as_object_mut() {
        object.insert(
            "parse_diagnostics".to_owned(),
            serde_json::to_value(&parse_diagnostics)?,
        );
    }
    if let Some(outcome) = consensus_outcome.as_ref()
        && let Some(object) = normalized.as_object_mut()
    {
        object.insert("consensus".to_owned(), serde_json::to_value(outcome)?);
    }
    if let Some(label) = prompt_experiment_group.as_ref()
        && let Some(object) = normalized.as_object_mut()
    {
        object.insert(
            "prompt_experiment_group".to_owned(),
            serde_json::Value::String(label.clone()),
        );
    }

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
    let auto_apply =
        settings.workflow.mode.auto_apply_validated_suggestions() && !settings.workflow.dry_run;
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
        match validate_title_suggestion(
            title.clone(),
            // Paperless-ngx Document.title is CharField(max_length=128);
            // anything longer passes validation here but 400s on PATCH.
            128,
            settings.effective_tuning().title_confidence_threshold,
        ) {
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
        if document.document_type.is_some() && !settings.metadata.overwrite_existing_document_type {
            skipped_fields.push("document_type");
        } else {
            match validate_choice_suggestion(
                choice.clone(),
                &allowed_document_types,
                settings
                    .effective_tuning()
                    .document_type_confidence_threshold,
            ) {
                Ok(valid) => {
                    let id =
                        named_entity_id_for_name(pool, "paperless_document_types", &valid.name)
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
        if document.correspondent.is_some() && !settings.metadata.overwrite_existing_correspondent {
            skipped_fields.push("correspondent");
        } else {
            match validate_choice_suggestion(
                choice.clone(),
                &allowed_correspondents,
                settings
                    .effective_tuning()
                    .correspondent_confidence_threshold,
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

    // --- new correspondent (gated, created at apply time) ---
    // When the model found no closed-vocabulary match (it set `correspondent`
    // null) but proposed a sender/issuer the document clearly names, carry
    // the NAME forward. It is created in Paperless only when the patch is
    // actually applied (full_auto right below, or on review approval) — never
    // while the suggestion waits for review or in dry-run (#404). Gated by
    // `metadata.allow_new_correspondents` and the same overwrite-existing
    // guard as the closed-vocab path.
    let mut pending_new_correspondent: Option<String> = None;
    if enabled.correspondent
        && settings.metadata.allow_new_correspondents
        && suggestion.correspondent.is_none()
        && composite_patch.correspondent.is_none()
        && (document.correspondent.is_none() || settings.metadata.overwrite_existing_correspondent)
        && let Some(new_name) = suggestion
            .new_correspondent
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
    {
        pending_new_correspondent = Some(new_name.to_owned());
        applied_fields.push("correspondent");
    }
    let mut pending_new_tags: Vec<String> = Vec::new();

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
            // v1.5.12: anchor-check the suggested date against the OCR text.
            // If no anchor phrase (Rechnungsdatum, Issued on, …) is within
            // ±80 chars of an occurrence of the date in the OCR text, drop
            // the confidence by document_date_anchor_penalty before
            // validating — this catches the common case where the model
            // picks up a body-text date (delivery date, due date, reference
            // to another invoice) instead of the actual document date.
            let mut adjusted_date = date.clone();
            let mut date_anchor_warning: Option<String> = None;
            if settings.metadata.document_date_anchor_required
                && !archivist_core::document_date_has_anchor(&date.date, &content)
            {
                let original = adjusted_date.confidence.unwrap_or(0.0);
                let penalty = settings.metadata.document_date_anchor_penalty;
                adjusted_date.confidence = Some((original - penalty).max(0.0));
                date_anchor_warning = Some(format!(
                    "document_date suggestion '{}' has no anchor phrase (Rechnungsdatum, Issued on, …) within {} chars in the OCR text; confidence reduced from {:.2} to {:.2}",
                    date.date,
                    80,
                    original,
                    adjusted_date.confidence.unwrap_or(0.0),
                ));
            }
            match validate_document_date_suggestion(
                adjusted_date,
                settings
                    .effective_tuning()
                    .document_date_confidence_threshold,
            ) {
                Ok(valid) => {
                    composite_patch.created = Some(valid.date.clone());
                    composite_warnings.extend(valid.warnings);
                    if let Some(warning) = date_anchor_warning.clone() {
                        composite_warnings.push(warning);
                    }
                    applied_fields.push("document_date");
                }
                Err(mut errors) => {
                    if let Some(warning) = date_anchor_warning.clone() {
                        errors.push(archivist_core::ValidationError::DataQuality(warning));
                    }
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
                                "anchor_warning": date_anchor_warning,
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
        // v1.5.12: tags-confidence override for the consolidated stage. Clone
        // TaggingSettings and bump the confidence_threshold to the per-field
        // metadata override so process_metadata stays decoupled from how the
        // legacy per-field tag stage thresholds work.
        let mut tagging_for_meta = settings.tagging.clone();
        tagging_for_meta.confidence_threshold =
            settings.effective_tuning().tags_confidence_threshold;
        match validate_tag_suggestion(
            tags.clone(),
            &allowed_tags,
            &settings.workflow.tags,
            &tagging_for_meta,
        ) {
            Ok(valid) => {
                let selected_ids = tag_ids_for_names(pool, &valid.tags).await?;
                // #411: honour old_tag_strategy for real; workflow and rule
                // tags always survive.
                let tag_ids =
                    tags_for_old_tag_strategy(pool, settings, &document, &selected_ids).await?;
                composite_patch.tags = Some(tag_ids);
                // #411: validated new_tags (only non-empty with allow_new_tags)
                // are applied too — created at apply time (#404).
                pending_new_tags = valid.new_tags.clone();
                composite_warnings.extend(valid.warnings);
                applied_fields.push("tags");
            }
            Err(errors) => {
                // Validation failed (e.g. low confidence, count over max_tags). Resolve the raw
                // LLM names to integer IDs BEFORE creating the review_item so the apply path can
                // deserialize `suggested_patch.tags` as `Vec<i32>` without 500-ing.
                //
                // #403: the review patch is `current ∪ suggested` so approving it can only ADD
                // tags (the apply path treats `baseline − desired` as removals); workflow tags
                // are never taken from model output.
                // #404: unknown names are NOT created here; with allow_new_tags they travel as
                // pending names and are created only if the review is approved.
                let (known_ids, unknown) =
                    resolve_known_tag_names(pool, &tags.tags, &settings.workflow.tags).await?;
                let mut tag_ids = document.tags.clone();
                tag_ids.extend(known_ids);
                tag_ids.sort_unstable();
                tag_ids.dedup();
                let pending = if settings.tagging.allow_new_tags {
                    archivist_apply::PendingNewObjects::new(
                        unknown.into_iter().chain(tags.new_tags.iter().cloned()),
                        None,
                        &settings.workflow.tags,
                    )
                } else {
                    for name in &unknown {
                        warn!(
                            unknown_tag = %name,
                            "dropping unknown tag from review_item suggested_patch (allow_new_tags is false)"
                        );
                    }
                    archivist_apply::PendingNewObjects::default()
                };
                let mut review_patch = json!({
                    "tags": tag_ids,
                    "standard_metadata": {
                        "field": "tags",
                        "confidence": tags.confidence,
                        "suggested_names": tags.tags,
                        "new_tag_names": pending.tags,
                    }
                });
                pending.attach_to(&mut review_patch);
                review_items.push((review_patch, json!(errors)));
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
            settings.effective_tuning().fields_confidence_threshold,
        ) {
            Ok(valid) => {
                let names = valid
                    .fields
                    .iter()
                    .map(|field| field.name.clone())
                    .collect::<Vec<_>>();
                let ids = custom_field_ids_for_names(pool, &names).await?;
                let mut values = Vec::new();
                for field in &valid.fields {
                    let Some((_, id, data_type)) = ids.iter().find(|(name, _, _)| {
                        archivist_core::catalog_names_equal(name, &field.name)
                    }) else {
                        continue;
                    };
                    match archivist_core::coerce_custom_field_value(
                        data_type.as_deref(),
                        &field.value,
                    ) {
                        Some(value) => values.push(json!({ "field": id, "value": value })),
                        None => {
                            warn!(
                                field = %field.name,
                                value = %field.value,
                                "dropped uncoercible custom field value"
                            );
                            composite_warnings.push(format!(
                                "dropped uncoercible custom field value: {} = {}",
                                field.name, field.value
                            ));
                        }
                    }
                }
                composite_patch.custom_fields = Some(json!(values));
                composite_warnings.extend(valid.warnings);
                applied_fields.push("fields");
            }
            Err(errors) => {
                // Same shape-correctness fix as tags: resolve field NAMES to numeric IDs and
                // wrap as `[{ "field": id, "value": ... }]` so the approve → patch path can
                // deserialize `suggested_patch.custom_fields` against Paperless without 500.
                let values = resolve_custom_field_values_to_ids(pool, &fields.fields).await?;
                review_items.push((
                    json!({
                        "custom_fields": values,
                        "standard_metadata": {
                            "field": "fields",
                            "suggested_names": fields
                                .fields
                                .iter()
                                .map(|f| f.name.clone())
                                .collect::<Vec<_>>(),
                        }
                    }),
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

    // Model-proposed objects that do not exist in Paperless yet. Capped and
    // stripped of workflow tag names; created only at apply time (#404).
    let pending_new_objects = archivist_apply::PendingNewObjects::new(
        pending_new_tags,
        pending_new_correspondent,
        &settings.workflow.tags,
    );

    // Routing:
    //   * full_auto: apply the validated composite_patch directly even if some
    //     fields had validation warnings (UnknownTag, UnknownChoice, EmptyOutput
    //     etc.). The warnings tell the operator WHICH per-field suggestion was
    //     dropped, but the patch only carries fields that resolved. Creating
    //     six review items per document in full_auto turns "hands-off mode"
    //     into a manual-review queue and explodes Paperless API calls 6x.
    //   * Otherwise (manual_review, auto_select_review, or full_auto + dry_run):
    //     every field becomes a review item — operator inspects all
    //     suggestions atomically rather than seeing a half-applied document.
    //   * If everything was skipped (already-set fields with overwrite disabled),
    //     we still mark the job complete so the run drains.
    // Final heartbeat before side effects (review inserts / Paperless PATCH):
    // from here on a lost lease must stop this worker, not double-apply.
    if !archivist_db::bump_job_lease(pool, job.id, lease_owner, job_lease_seconds(settings)).await?
    {
        warn!(
            job_id = %job.id,
            document_id = job.paperless_document_id,
            "metadata lease lost before apply/review; stopping so a replica isn't duplicated"
        );
        return Ok(());
    }
    let review_warning_count = review_items.len();
    if !review_items.is_empty() && !auto_apply {
        // Manual / dry-run path: demote applied fields to review items too,
        // so the operator can sign off on the full set rather than seeing
        // partial application.
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
        if let Some(name) = pending_new_objects.correspondent.clone() {
            // #404: name only; the correspondent is created on approval.
            let mut patch = json!({
                "standard_metadata": {
                    "field": "correspondent",
                    "auto_validated": true,
                    "suggested_name": name,
                    "new_object": true,
                }
            });
            archivist_apply::PendingNewObjects {
                tags: Vec::new(),
                correspondent: Some(name),
            }
            .attach_to(&mut patch);
            review_items.push((patch, json!([])));
        }
        if let Some(tags) = composite_patch.tags.clone() {
            let mut patch = json!({
                "tags": tags,
                "standard_metadata": {
                    "field": "tags",
                    "auto_validated": true,
                    "new_tag_names": pending_new_objects.tags,
                }
            });
            archivist_apply::PendingNewObjects {
                tags: pending_new_objects.tags.clone(),
                correspondent: None,
            }
            .attach_to(&mut patch);
            review_items.push((patch, json!([])));
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

        let baseline = review_apply_baseline(&document);
        for (patch, warnings) in review_items {
            if create_review_item(pool, job, patch, warnings, baseline.clone(), lease_owner)
                .await?
                .is_none()
            {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    "metadata lease lost during review creation; stopping so a replica isn't duplicated"
                );
                return Ok(());
            }
        }
        Ok(())
    } else if !applied_fields.is_empty() {
        if auto_apply {
            // #404: full_auto + validated is the apply moment, so create the
            // proposed objects now. A creation failure degrades to applying
            // the rest of the patch, as before.
            if !pending_new_objects.is_empty() {
                match archivist_apply::materialize_pending_new_objects(
                    paperless,
                    &pending_new_objects,
                    archivist_apply::NewObjectPolicy::from_settings(settings),
                    &mut composite_patch,
                )
                .await
                {
                    Ok(new_tag_ids) if !new_tag_ids.is_empty() => {
                        let tags = composite_patch
                            .tags
                            .get_or_insert_with(|| document.tags.clone());
                        tags.extend(new_tag_ids);
                        tags.sort_unstable();
                        tags.dedup();
                    }
                    Ok(_) => {}
                    Err(error) => warn!(
                        document_id = job.paperless_document_id,
                        error = %error,
                        "failed to create proposed Paperless objects; applying without them"
                    ),
                }
            }
            let final_run_stage = is_last_active_job(pool, job.run_id, job.id).await?;
            let execution = apply_patch_with_workflow_tags(
                pool,
                paperless,
                settings,
                job,
                composite_patch,
                final_run_stage,
                lease_owner,
            )
            .await?;
            if complete_job(
                pool,
                job,
                lease_owner,
                json!({
                    "applied": true,
                    "fields": applied_fields,
                    "warnings": composite_warnings,
                    "dropped_field_count": review_warning_count,
                    "parse_diagnostics": parse_diagnostics,
                }),
            )
            .await?
            {
                archivist_db::finalize_apply_intent(pool, execution.attempt_id()).await?;
            } else {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    "lease lost before completion; another worker owns this job — skipping"
                );
            }
            Ok(())
        } else {
            // manual_review (or dry_run): a single composite review item with all validated
            // suggestions so the operator approves the whole set atomically.
            let baseline = review_apply_baseline(&document);
            let mut composite_review_patch = serde_json::to_value(&composite_patch)?;
            pending_new_objects.attach_to(&mut composite_review_patch);
            if create_review_item(
                pool,
                job,
                composite_review_patch,
                json!(composite_warnings),
                baseline,
                lease_owner,
            )
            .await?
            .is_none()
            {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    "metadata lease lost during review creation; skipping"
                );
            }
            Ok(())
        }
    } else if auto_apply && review_warning_count > 0 {
        // full_auto + every field had a validation warning, nothing applied. We
        // record the warnings in the job result and mark the job complete so
        // the run drains — Paperless gets nothing for this stage but neither
        // does the operator get a useless review pile.
        if !complete_job(
            pool,
            job,
            lease_owner,
            json!({
                "skipped": "all metadata fields had validation warnings — no resolvable patch in full_auto",
                "warnings": composite_warnings,
                "dropped_field_count": review_warning_count,
                "parse_diagnostics": parse_diagnostics,
            }),
        )
        .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "lease lost before completion; another worker owns this job — skipping"
            );
        } else {
            // #400: terminal without a patch — retire the trigger tags.
            retire_trigger_tags_after_terminal_outcome(pool, paperless, settings, job, false)
                .await;
        }
        Ok(())
    } else {
        if !complete_job(
            pool,
            job,
            lease_owner,
            json!({
                "skipped": "all metadata fields skipped by model omission or overwrite policy",
                "skipped_fields": skipped_fields,
                "parse_diagnostics": parse_diagnostics,
            }),
        )
        .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "lease lost before completion; another worker owns this job — skipping"
            );
        } else {
            // #400: terminal without a patch — retire the trigger tags.
            retire_trigger_tags_after_terminal_outcome(pool, paperless, settings, job, false).await;
        }
        Ok(())
    }
}

/// Outcome of the two-model consensus cross-check. Captured for the
/// metadata `ai_artifacts.normalized` payload so dashboards can chart
/// the disagreement rate without re-parsing audit events.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct ConsensusOutcome {
    secondary_model: String,
    correspondent_primary: Option<String>,
    correspondent_secondary: Option<String>,
    correspondent_disagreed: bool,
    date_primary: Option<String>,
    date_secondary: Option<String>,
    date_disagreed: bool,
}

/// Two-model consensus cross-check for high-stakes fields.
///
/// Runs a focused secondary LLM call asking ONLY for `correspondent`
/// and `document_date`. When the secondary answer disagrees with the
/// primary suggestion's value, that specific field is wiped from the
/// primary `MetadataSuggestion` so it falls into review instead of
/// being auto-applied. Disagreements are audited via
/// `workflow.consensus_disagreement`.
///
/// Comparison rules:
/// * correspondent — case-insensitive exact match on the resolved name.
///   Empty secondary answer means "no opinion", which is NOT a
///   disagreement.
/// * document_date — both sides parsed as ISO; absolute day difference
///   must be ≤ `settings.ai.consensus_date_tolerance_days`. Empty or
///   un-parsable secondary answer means "no opinion".
#[allow(clippy::too_many_arguments)]
async fn run_consensus_check(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
    job: &JobRecord,
    content: &str,
    allowed_correspondents: &[String],
    language: &archivist_ai::PromptLanguageContext,
    suggestion: &mut MetadataSuggestion,
) -> Result<ConsensusOutcome> {
    let tuning = settings.effective_tuning();
    let secondary_model = tuning
        .consensus_secondary_text_model
        .clone()
        .unwrap_or_default();
    let mut outcome = ConsensusOutcome {
        secondary_model: secondary_model.clone(),
        ..Default::default()
    };
    if secondary_model.trim().is_empty() {
        return Ok(outcome);
    }

    // Build the focused 2-field prompt and override the model for the
    // call. Reuses the metadata stage's provider (and therefore the
    // operator's authentication) so no separate endpoint config is
    // needed.
    let mut request =
        archivist_ai::prompt_for_consensus_check(content, allowed_correspondents, language);
    let mut provider = match provider_for_stage(settings, Stage::Metadata, false) {
        Ok(p) => p,
        Err(error) => {
            warn!(
                document_id = job.paperless_document_id,
                error = %error,
                "consensus skipped: provider_for_stage(metadata) failed"
            );
            return Ok(outcome);
        }
    };
    provider.model = secondary_model.clone();
    request.model = secondary_model.clone();
    request.num_ctx = ollama_text_num_ctx_for_provider(&provider, tuning.text_num_ctx);
    request.reasoning_effort = Some(provider.reasoning_effort);
    request.max_output_tokens = provider.max_output_tokens;
    request.structured_output = Some(provider.structured_output);

    let response = match chat_with_provider(pool, config, &provider, request).await {
        Ok(r) => r,
        // Propagate a quota signal so a cooldown is recorded; a non-quota
        // secondary-call failure stays a graceful no-opinion. #280
        Err(error)
            if classify_processing_failure(&error) == ProcessingFailureClass::ProviderQuota =>
        {
            return Err(error);
        }
        Err(error) => {
            warn!(
                document_id = job.paperless_document_id,
                secondary_model = %secondary_model,
                error = %error,
                "consensus secondary call failed; treating as no-opinion"
            );
            return Ok(outcome);
        }
    };
    let answer = archivist_ai::parse_consensus_answer(&response.text);

    // Correspondent comparison
    if let Some(primary) = suggestion.correspondent.clone() {
        outcome.correspondent_primary = Some(primary.name.clone());
        outcome.correspondent_secondary = Some(answer.correspondent.clone());
        let primary_norm = primary.name.trim().to_lowercase();
        let secondary_norm = answer.correspondent.trim().to_lowercase();
        if !secondary_norm.is_empty() && primary_norm != secondary_norm {
            outcome.correspondent_disagreed = true;
            suggestion.correspondent = None;
        }
    }

    // Date comparison
    if let Some(primary) = suggestion.document_date.clone() {
        outcome.date_primary = Some(primary.date.clone());
        outcome.date_secondary = Some(answer.document_date.clone());
        let primary_parsed = chrono::NaiveDate::parse_from_str(&primary.date, "%Y-%m-%d").ok();
        let secondary_parsed =
            chrono::NaiveDate::parse_from_str(answer.document_date.trim(), "%Y-%m-%d").ok();
        if let (Some(p), Some(s)) = (primary_parsed, secondary_parsed) {
            let tolerance = tuning.consensus_date_tolerance_days.max(0);
            let diff = (p - s).num_days().abs();
            if diff > tolerance {
                outcome.date_disagreed = true;
                suggestion.document_date = None;
            }
        }
    }

    if outcome.correspondent_disagreed || outcome.date_disagreed {
        info!(
            document_id = job.paperless_document_id,
            secondary_model = %secondary_model,
            correspondent_disagreed = outcome.correspondent_disagreed,
            date_disagreed = outcome.date_disagreed,
            "consensus disagreement — dropping disagreeing fields from auto-apply"
        );
        append_audit(
            pool,
            AuditEventInput {
                event_type: "workflow.consensus_disagreement".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: Some(job.run_id),
                job_id: Some(job.id),
                paperless_document_id: Some(job.paperless_document_id),
                before: None,
                after: Some(json!({
                    "secondary_model": secondary_model,
                    "correspondent_disagreed": outcome.correspondent_disagreed,
                    "correspondent_primary": outcome.correspondent_primary,
                    "correspondent_secondary": outcome.correspondent_secondary,
                    "date_disagreed": outcome.date_disagreed,
                    "date_primary": outcome.date_primary,
                    "date_secondary": outcome.date_secondary,
                })),
                metadata: Some(json!({ "trigger": "consensus_check" })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archivist_core::ProcessingMode;
    use archivist_db::{claim_jobs, connect, create_run_with_jobs_with_priority, fail_job};
    use sqlx::Row;

    #[test]
    fn metadata_contract_violations_retry_but_omissions_take_the_explicit_success_route() {
        let malformed = parse_metadata_suggestion(r#"{"title":42}"#);
        assert_eq!(
            metadata_worker_parse_route(&malformed.diagnostics),
            MetadataWorkerParseRoute::RetryContractViolation
        );
        let contract_error = malformed
            .diagnostics
            .contract_error()
            .expect("wrong known field type must violate the contract");
        let error = anyhow::Error::new(contract_error);
        assert_eq!(
            classify_processing_failure(&error),
            ProcessingFailureClass::Transient
        );

        let omitted = parse_metadata_suggestion(r#"{"title":null}"#);
        assert_eq!(omitted.diagnostics.status, MetadataParseStatus::Omitted);
        assert_eq!(
            metadata_worker_parse_route(&omitted.diagnostics),
            MetadataWorkerParseRoute::CompleteOmission
        );
        assert!(omitted.diagnostics.contract_error().is_none());

        let valid = parse_metadata_suggestion(r#"{"title":{"title":"Invoice","confidence":0.9}}"#);
        assert_eq!(
            metadata_worker_parse_route(&valid.diagnostics),
            MetadataWorkerParseRoute::ProcessSuggestion
        );
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
    async fn metadata_terminal_parse_routes_persist_artifacts_before_completion_or_retry() {
        let Ok(database_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = connect(&database_url, 10)
            .await
            .expect("connect test database (DB integration tests share one database: run them serially with `-- --ignored --test-threads=1`, see scripts/verify/migration_smoke.sh)");
        archivist_db::migrate(&pool)
            .await
            .expect("apply migrations");
        sqlx::query(
            r#"
            truncate document_inventory, jobs, pipeline_runs, review_items,
                     ai_artifacts, audit_events restart identity cascade
            "#,
        )
        .execute(&pool)
        .await
        .expect("truncate test tables");

        let request = ChatRequest {
            model: "test-model".to_owned(),
            system_prompt: "system".to_owned(),
            user_prompt: "user".to_owned(),
            temperature: 0.0,
            num_ctx: None,
            response_schema: None,
            reasoning_effort: None,
            max_output_tokens: None,
            structured_output: None,
        };
        let settings = RuntimeSettings::default();

        for document_id in [77, 78] {
            sqlx::query(
                "insert into document_inventory (paperless_document_id, current_tags) values ($1, '{}')",
            )
            .bind(document_id)
            .execute(&pool)
            .await
            .expect("seed inventory");
            create_run_with_jobs_with_priority(
                &pool,
                document_id,
                &[Stage::Metadata],
                ProcessingMode::ManualReview,
                "test",
                "test",
                Some(0),
            )
            .await
            .expect("create metadata run");
        }
        let jobs = claim_jobs(&pool, 2, "worker-a", 300)
            .await
            .expect("claim jobs");
        let omitted_job = jobs
            .iter()
            .find(|job| job.paperless_document_id == 77)
            .expect("omission job");
        let contract_job = jobs
            .iter()
            .find(|job| job.paperless_document_id == 78)
            .expect("contract job");

        let omitted = parse_metadata_suggestion("{}");
        let omitted_response = AiResponse {
            provider: "test-provider".to_owned(),
            model: "test-model".to_owned(),
            text: "{}".to_owned(),
            raw_response: json!({"message":{"content":"{}"}}),
            duration_ms: 1,
        };
        let handled = handle_terminal_metadata_parse_route(
            &pool,
            omitted_job,
            "worker-a",
            &settings,
            &omitted_response,
            &request,
            None,
            "private document content",
            &omitted.suggestion,
            &omitted.diagnostics,
        )
        .await
        .expect("complete omission");
        assert!(handled, "omission is terminal before consensus/apply");
        let row = sqlx::query("select status, result from jobs where id = $1")
            .bind(omitted_job.id)
            .fetch_one(&pool)
            .await
            .expect("omission job result");
        let status: String = row.try_get("status").expect("omission status");
        let result: serde_json::Value = row.try_get("result").expect("omission result");
        assert_eq!(status, "succeeded");
        assert_eq!(result["parse_diagnostics"]["status"], "omitted");

        let malformed = parse_metadata_suggestion(r#"{"title":"raw-private-value"}"#);
        let malformed_response = AiResponse {
            provider: "test-provider".to_owned(),
            model: "test-model".to_owned(),
            text: r#"{"title":"raw-private-value"}"#.to_owned(),
            raw_response: json!({"message":{"content":"raw-private-value"}}),
            duration_ms: 1,
        };
        let error = handle_terminal_metadata_parse_route(
            &pool,
            contract_job,
            "worker-a",
            &settings,
            &malformed_response,
            &request,
            None,
            "private document content",
            &malformed.suggestion,
            &malformed.diagnostics,
        )
        .await
        .expect_err("contract violation must retry");
        assert_eq!(
            classify_processing_failure(&error),
            ProcessingFailureClass::Transient
        );
        let safe_error = format!("{error:#}");
        assert!(safe_error.contains("metadata model contract violation"));
        assert!(safe_error.contains("invalid_fields=title"));
        assert!(!safe_error.contains("raw-private-value"));
        let row = sqlx::query("select status, result from jobs where id = $1")
            .bind(contract_job.id)
            .fetch_one(&pool)
            .await
            .expect("contract job state");
        let status: String = row.try_get("status").expect("contract status");
        let result: Option<serde_json::Value> = row.try_get("result").expect("contract result");
        assert_eq!(status, "running", "terminal helper never completes it");
        assert!(result.is_none());
        let normalized: serde_json::Value = sqlx::query_scalar(
            "select normalized_output from ai_artifacts where job_id = $1 order by created_at desc limit 1",
        )
        .bind(contract_job.id)
        .fetch_one(&pool)
        .await
        .expect("contract artifact");
        assert_eq!(
            normalized["parse_diagnostics"]["status"],
            "contract_violation"
        );
        assert_eq!(
            normalized["parse_diagnostics"]["invalid_fields"],
            json!(["title"])
        );
        assert!(!normalized.to_string().contains("raw-private-value"));
        let review_count: i64 = sqlx::query_scalar("select count(*) from review_items")
            .fetch_one(&pool)
            .await
            .expect("review count");
        assert_eq!(review_count, 0);

        let updated = fail_job(
            &pool,
            contract_job,
            "worker-a",
            &safe_error,
            classify_processing_failure(&error).is_retryable(),
            None,
        )
        .await
        .expect("route contract failure through normal retry budget");
        assert!(updated);
        let row = sqlx::query("select status, error_message from jobs where id = $1")
            .bind(contract_job.id)
            .fetch_one(&pool)
            .await
            .expect("retried contract job");
        let status: String = row.try_get("status").expect("retry status");
        let error_message: String = row.try_get("error_message").expect("safe job error");
        assert_eq!(status, "queued");
        assert_eq!(error_message, safe_error);
        assert!(!error_message.contains("raw-private-value"));
    }

    // ---- v1.5.2 Bug 2 regression: name-to-id resolution for review_items ----

    #[test]
    fn diff_known_tag_names_case_insensitive_and_unique() {
        let requested = vec![
            "Hardware".to_owned(),
            "Rechnung".to_owned(),
            "hardware".to_owned(), // duplicate, different case
            "NoSuchTag".to_owned(),
        ];
        // The local mirror returns lowercased matches like the real SQL helper does.
        let known = vec![("hardware".to_owned(), 7), ("rechnung".to_owned(), 12)];
        let (ids, unknown) = diff_known_tag_names(&requested, &known);
        assert_eq!(ids, vec![7, 12], "known ids returned sorted-deduped");
        assert_eq!(
            unknown,
            vec!["NoSuchTag".to_owned()],
            "only the unmatched name needs creation-or-drop downstream"
        );
    }

    #[test]
    fn diff_known_tag_names_folds_non_ascii_capitals() {
        // The SQL lookup returns the catalog spelling; a lowercase request must
        // not be treated as unknown (and re-created in Paperless). #409
        let requested = vec!["ärzte".to_owned(), "Übersetzung".to_owned()];
        let known = vec![("Ärzte".to_owned(), 21), ("übersetzung".to_owned(), 22)];
        let (ids, unknown) = diff_known_tag_names(&requested, &known);
        assert_eq!(ids, vec![21, 22]);
        assert!(unknown.is_empty(), "unexpected unknown tags: {unknown:?}");
    }

    #[test]
    fn diff_known_tag_names_empty_inputs() {
        let (ids, unknown) = diff_known_tag_names(&[], &[]);
        assert!(ids.is_empty());
        assert!(unknown.is_empty());
    }

    #[test]
    fn diff_known_tag_names_all_unknown() {
        let requested = vec!["A".to_owned(), "B".to_owned()];
        let (ids, unknown) = diff_known_tag_names(&requested, &[]);
        assert!(ids.is_empty());
        assert_eq!(unknown, requested);
    }

    #[test]
    fn build_custom_field_value_patch_drops_unknown_names() {
        use archivist_core::FieldValueSuggestion;
        use serde_json::Value;
        let fields = vec![
            FieldValueSuggestion {
                name: "Invoice Number".to_owned(),
                value: Value::String("INV-001".to_owned()),
                confidence: Some(0.9),
            },
            FieldValueSuggestion {
                name: "ghost_field".to_owned(),
                value: Value::String("nope".to_owned()),
                confidence: Some(0.9),
            },
        ];
        let id_pairs = vec![("invoice number".to_owned(), 42, Some("string".to_owned()))];
        let patch = build_custom_field_value_patch(&fields, &id_pairs);
        assert_eq!(patch.len(), 1, "ghost_field should be dropped");
        // Shape must be { "field": <i32>, "value": ... } — what Paperless / DocumentPatch expects.
        let entry = &patch[0];
        assert_eq!(entry.get("field").and_then(Value::as_i64), Some(42));
        assert_eq!(
            entry.get("value").and_then(Value::as_str),
            Some("INV-001"),
            "value passes through unchanged"
        );
    }

    #[test]
    fn build_custom_field_value_patch_preserves_input_order() {
        use archivist_core::FieldValueSuggestion;
        use serde_json::Value;
        let fields = vec![
            FieldValueSuggestion {
                name: "B".to_owned(),
                value: Value::String("val-b".to_owned()),
                confidence: None,
            },
            FieldValueSuggestion {
                name: "A".to_owned(),
                value: Value::String("val-a".to_owned()),
                confidence: None,
            },
        ];
        let id_pairs = vec![("a".to_owned(), 1, None), ("b".to_owned(), 2, None)];
        let patch = build_custom_field_value_patch(&fields, &id_pairs);
        assert_eq!(
            patch[0].get("field").and_then(Value::as_i64),
            Some(2),
            "input order preserved (B then A), not pair order"
        );
        assert_eq!(patch[1].get("field").and_then(Value::as_i64), Some(1));
    }
}
