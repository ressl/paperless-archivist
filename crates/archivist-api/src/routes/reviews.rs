//! Review queue, preview proxy, retry, auto-fix and review apply.

use crate::*;

#[derive(Debug, Deserialize)]
pub(crate) struct ReviewQuery {
    pub(crate) status: Option<String>,
    pub(crate) limit: Option<i64>,
}

pub(crate) async fn reviews(
    State(state): State<AppState>,
    _auth: Authenticated,
    Query(query): Query<ReviewQuery>,
) -> ApiResult<Json<Value>> {
    let settings = get_runtime_settings(&state.pool).await?;
    let items = list_reviews(
        &state.pool,
        query.status.as_deref(),
        query.limit.unwrap_or(100).clamp(1, 500),
    )
    .await?
    .into_iter()
    .map(|review| review_with_debug(review, &settings))
    .collect::<Result<Vec<_>>>()?;
    let total = count_reviews(&state.pool, query.status.as_deref()).await?;
    let has_more = total > items.len() as i64;
    Ok(Json(json!({
        "items": items,
        "total": total,
        "has_more": has_more
    })))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewPreviewKind {
    Thumbnail,
    Document,
}

/// Content-Security-Policy for proxied preview bytes. Stricter than the SPA
/// policy (no scripts, no connections); `object-src 'self'` keeps the
/// browser's built-in PDF viewer working for the top-level PDF tab. #445
pub(crate) const REVIEW_PREVIEW_CSP: &str = "default-src 'none'; object-src 'self'; img-src 'self'; \
     style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Proxy the Paperless thumbnail of a review's document. The browser never
/// talks to Paperless: Archivist fetches with its server-side token from the
/// configured (SSRF-validated) base URL, keyed by review id so only documents
/// in the review queue are reachable. #445
pub(crate) async fn review_thumbnail(
    State(state): State<AppState>,
    _auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    review_preview_response(&state, id, ReviewPreviewKind::Thumbnail).await
}

/// Proxy the inline Paperless preview (archive PDF or image). #445
pub(crate) async fn review_document_preview(
    State(state): State<AppState>,
    _auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    review_preview_response(&state, id, ReviewPreviewKind::Document).await
}

pub(crate) async fn review_preview_response(
    state: &AppState,
    review_id: Uuid,
    kind: ReviewPreviewKind,
) -> ApiResult<Response> {
    let document_id = archivist_db::review_document_id(&state.pool, review_id)
        .await?
        .ok_or_else(|| ApiError::not_found("review item does not exist"))?;
    let settings = get_runtime_settings(&state.pool).await?;
    // Missing token/profile -> 409 NotConfigured like every Paperless route.
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    let fetched = match kind {
        ReviewPreviewKind::Thumbnail => client.download_thumbnail(document_id).await,
        ReviewPreviewKind::Document => client.download_preview(document_id).await,
    };
    let preview = fetched.map_err(|error| review_preview_upstream_error(document_id, error))?;
    let extension = match preview.content_type {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        _ => "webp",
    };
    let disposition = format!("inline; filename=\"document-{document_id}.{extension}\"");
    let cache_control = match kind {
        // Thumbnails are re-requested while triaging; the document is not.
        ReviewPreviewKind::Thumbnail => "private, max-age=300",
        ReviewPreviewKind::Document => "private, no-store",
    };
    let mut response = (StatusCode::OK, preview.bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(preview.content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(REVIEW_PREVIEW_CSP),
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

/// Map a failed Paperless preview fetch without leaking upstream text. #445
pub(crate) fn review_preview_upstream_error(document_id: i32, error: anyhow::Error) -> ApiError {
    if let Some(archivist_paperless::PaperlessError::Client { status: 404, .. }) =
        error.downcast_ref::<archivist_paperless::PaperlessError>()
    {
        return ApiError::not_found("document not found in Paperless");
    }
    warn!(document_id, error = %error, "review preview: Paperless fetch failed");
    ApiError {
        status: StatusCode::BAD_GATEWAY,
        message: "could not load the preview from Paperless".to_owned(),
    }
}

/// Choices for "retry with ...": enabled text providers (with the model the
/// metadata stage would use) and the metadata prompt versions, without prompt
/// content, so reviewers without settings access can pick one. #445
pub(crate) async fn review_retry_options(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let settings = get_runtime_settings(&state.pool).await?;
    let default_provider = settings
        .ai
        .stage_models
        .iter()
        .find(|entry| entry.stage == Stage::Metadata)
        .map(|entry| entry.provider.clone())
        .unwrap_or_else(|| settings.ai.default_provider.clone());
    let prompts: Vec<Value> = list_prompts(&state.pool)
        .await?
        .into_iter()
        .filter(|prompt| prompt.stage == Stage::Metadata)
        .map(|prompt| {
            json!({
                "id": prompt.id,
                "name": prompt.name,
                "version": prompt.version,
                "active": prompt.active,
                "created_at": prompt.created_at
            })
        })
        .collect();
    Ok(Json(json!({
        "providers": settings.ai.metadata_retry_providers(),
        "default_provider": default_provider,
        "prompts": prompts
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetryReviewRequest {
    #[serde(default)]
    pub(crate) provider_name: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) prompt_id: Option<Uuid>,
}

/// "Retry with provider/model/prompt": reject the pending metadata review
/// (and its pending siblings) and queue a new metadata run whose job uses the
/// chosen configuration once. Requires a reviewer session like every other
/// review decision. #445
#[tracing::instrument(
    skip(state, auth, request),
    fields(review_id = %id, user_id = tracing::field::Empty)
)]
pub(crate) async fn retry_review(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<RetryReviewRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let overrides = archivist_core::MetadataRetryOverrides {
        provider_name: request.provider_name,
        model: request.model,
        prompt_id: request.prompt_id,
    }
    .normalized();
    let settings = get_runtime_settings(&state.pool).await?;
    overrides
        .validate(&settings.ai)
        .map_err(ApiError::bad_request)?;
    if let Some(prompt_id) = overrides.prompt_id {
        match archivist_db::get_prompt_by_id(&state.pool, prompt_id).await? {
            Some(prompt) if prompt.stage == Stage::Metadata => {}
            _ => {
                return Err(ApiError::bad_request(
                    "prompt_id must reference a metadata prompt version",
                ));
            }
        }
    }
    let payload = serde_json::to_value(&overrides)
        .map_err(|_| ApiError::internal("could not encode retry overrides"))?;
    match archivist_db::retry_review_with_overrides(&state.pool, id, actor_id, &payload).await? {
        archivist_db::ReviewRetryOutcome::Queued {
            run_id,
            rejected_review_ids,
        } => {
            info!(review_id = %id, %run_id, "review retried with overrides");
            Ok(Json(json!({
                "run_id": run_id,
                "rejected_review_ids": rejected_review_ids
            })))
        }
        archivist_db::ReviewRetryOutcome::UnsupportedStage => Err(ApiError::bad_request(
            "only metadata reviews can be retried with another model or prompt",
        )),
        archivist_db::ReviewRetryOutcome::SiblingInFlight => Err(ApiError::conflict(
            "another suggestion for this document is being applied; retry once it has finished",
        )),
        archivist_db::ReviewRetryOutcome::ActiveRun => {
            Err(ApiError::conflict("the document already has an active run"))
        }
    }
}

pub(crate) fn review_with_debug(
    review: ReviewItemRecord,
    settings: &RuntimeSettings,
) -> Result<Value> {
    let mut value = serde_json::to_value(review)?;
    if let Some(object) = value.as_object_mut() {
        let mut debug = object
            .get("debug_context")
            .cloned()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        debug.insert("workflow_mode".to_owned(), json!(settings.workflow.mode));
        debug.insert(
            "workflow_paused".to_owned(),
            json!(settings.workflow.paused),
        );
        debug.insert("dry_run".to_owned(), json!(settings.workflow.dry_run));
        debug.insert(
            "tag_output_language".to_owned(),
            json!(settings.tagging.tag_output_language),
        );
        let prompt_language = debug
            .get("detected_language")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("und")
            .to_owned();
        debug.insert("prompt_language".to_owned(), json!(prompt_language));
        object.insert("debug_context".to_owned(), Value::Object(debug));
    }
    Ok(value)
}

#[derive(Debug, Deserialize)]
pub(crate) struct BatchReviewRequest {
    pub(crate) ids: Vec<Uuid>,
    pub(crate) decision: String,
}

pub(crate) async fn batch_review(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<BatchReviewRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    if request.ids.is_empty() {
        return Err(ApiError::bad_request("ids must not be empty"));
    }
    if request.ids.len() > 100 {
        return Err(ApiError::bad_request(
            "batch review is limited to 100 items per request",
        ));
    }
    if !matches!(request.decision.as_str(), "approve" | "reject") {
        return Err(ApiError::bad_request(
            "decision must be either 'approve' or 'reject'",
        ));
    }

    let mut applied = Vec::new();
    let mut failed = Vec::new();
    for id in request.ids {
        let result = if request.decision == "approve" {
            match review_decision(&state.pool, id, "approved", None, actor_id).await {
                Ok(()) => apply_review_patch(&state, id, actor_id).await,
                Err(error) => Err(error),
            }
        } else {
            review_decision(&state.pool, id, "rejected", None, actor_id).await
        };
        match result {
            Ok(()) => applied.push(id),
            Err(error) => failed.push(json!({ "id": id, "error": error.to_string() })),
        }
    }

    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: format!("review.batch_{}", request.decision),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "succeeded": applied.len(), "failed": failed.len() })),
            metadata: None,
            outcome: if failed.is_empty() {
                "success".to_owned()
            } else {
                "partial_failure".to_owned()
            },
            error_message: failed
                .first()
                .and_then(|entry| entry.get("error"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    Ok(Json(json!({
        "ok": failed.is_empty(),
        "succeeded": applied,
        "failed": failed
    })))
}

#[tracing::instrument(
    skip(state, auth),
    fields(review_id = %id, user_id = tracing::field::Empty)
)]
pub(crate) async fn approve_review(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    // Token requests carry the creator's user_id, so accepting them here
    // attributed automation decisions to that admin in the audit trail and
    // apply intents. All review decision endpoints now require an
    // interactive session, matching batch/auto-fix. #393
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    review_decision(&state.pool, id, "approved", None, actor_id).await?;
    apply_review_patch(&state, id, actor_id).await?;
    info!(review_id = %id, %actor_id, "review approved");
    Ok(Json(json!({ "ok": true })))
}

#[tracing::instrument(
    skip(state, auth),
    fields(review_id = %id, user_id = tracing::field::Empty)
)]
pub(crate) async fn reject_review(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    // Token requests carry the creator's user_id, so accepting them here
    // attributed automation decisions to that admin in the audit trail and
    // apply intents. All review decision endpoints now require an
    // interactive session, matching batch/auto-fix. #393
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    review_decision(&state.pool, id, "rejected", None, actor_id).await?;
    info!(review_id = %id, %actor_id, "review rejected");
    Ok(Json(json!({ "ok": true })))
}

/// Decision made by `clean_review_patch_for_auto_fix` for one review_item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoFixAction {
    /// Patch has meaningful content after cleaning — apply it.
    Apply,
    /// Patch is empty after cleaning — reject the review_item.
    Reject,
}

/// Result of cleaning one review_item's suggested_patch for the auto-fix path.
#[derive(Debug, Clone)]
pub(crate) struct AutoFixDecision {
    pub(crate) cleaned_patch: Value,
    pub(crate) fields_dropped: Vec<String>,
    pub(crate) action: AutoFixAction,
}

/// Inspect a review_item's `suggested_patch` + `validation_warnings` and
/// produce a cleaned patch that drops fields the validator flagged
/// (UnknownChoice / UnknownTag / UnknownField / EmptyOutput), plus a
/// decision on whether anything useful remains.
///
/// The heuristic is conservative:
/// * Any `UnknownChoice` or `UnknownField` warning → drop the entire
///   `custom_fields` array. Most production failures are select-typed
///   custom-field values the LLM made up; Paperless rejects them at the
///   patch boundary, so the safest cleanup is to skip them entirely.
/// * Drop `document_type` / `correspondent` keys whose value is `null`
///   (means the LLM proposed a name that didn't resolve to an ID).
/// * Drop empty `tags` arrays.
/// * Whatever non-null, non-empty fields remain in `{title, correspondent,
///   document_type, created, tags, custom_fields, content}` → Apply.
///   Otherwise → Reject (nothing meaningful left to write to Paperless).
pub(crate) fn clean_review_patch_for_auto_fix(
    suggested_patch: &Value,
    validation_warnings: &Value,
) -> AutoFixDecision {
    let mut patch_obj = suggested_patch.as_object().cloned().unwrap_or_default();
    let warnings_arr: Vec<&Value> = validation_warnings
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();

    let warning_has_kind = |kind: &str| -> bool {
        warnings_arr.iter().any(|w| match w {
            Value::Object(obj) => obj.contains_key(kind),
            Value::String(s) => s == kind || s.contains(kind),
            _ => false,
        })
    };

    let has_unknown_choice = warning_has_kind("UnknownChoice");
    let has_unknown_field = warning_has_kind("UnknownField");
    let has_unknown_tag = warning_has_kind("UnknownTag");
    let has_empty_output = warning_has_kind("EmptyOutput");

    let mut fields_dropped: Vec<String> = Vec::new();

    // Drop custom_fields entirely if any UnknownChoice/UnknownField/EmptyOutput
    // — these all mean at least one custom-field entry would fail at Paperless.
    if (has_unknown_choice || has_unknown_field || has_empty_output)
        && patch_obj.remove("custom_fields").is_some()
    {
        fields_dropped.push("custom_fields".to_owned());
    }

    // Drop document_type / correspondent if they're null (failed resolution).
    for field in ["document_type", "correspondent"] {
        if let Some(v) = patch_obj.get(field)
            && v.is_null()
        {
            patch_obj.remove(field);
            fields_dropped.push(format!("{field} (null)"));
        }
    }

    // Drop tags if empty array (nothing to add) OR if there was an UnknownTag
    // warning AND the patch's tags list is empty/missing (defensive).
    if let Some(v) = patch_obj.get("tags")
        && v.as_array().is_some_and(|a| a.is_empty())
    {
        patch_obj.remove("tags");
        fields_dropped.push("tags (empty)".to_owned());
    }
    if has_unknown_tag && !patch_obj.contains_key("tags") {
        // Already dropped or never there; nothing to do.
    }

    // Strip empty-string title.
    if let Some(v) = patch_obj.get("title")
        && v.as_str().is_some_and(|s| s.trim().is_empty())
    {
        patch_obj.remove("title");
        fields_dropped.push("title (empty)".to_owned());
    }

    // Strip null `created`.
    if let Some(v) = patch_obj.get("created")
        && (v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty()))
    {
        patch_obj.remove("created");
        fields_dropped.push("created (null/empty)".to_owned());
    }

    let useful_keys = [
        "title",
        "correspondent",
        "document_type",
        "created",
        "tags",
        "custom_fields",
        "content",
    ];
    let has_meaningful_content = patch_obj.keys().any(|k| {
        useful_keys.contains(&k.as_str()) && patch_obj.get(k).is_some_and(|v| !v.is_null())
    });

    AutoFixDecision {
        cleaned_patch: Value::Object(patch_obj),
        fields_dropped,
        action: if has_meaningful_content {
            AutoFixAction::Apply
        } else {
            AutoFixAction::Reject
        },
    }
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct AutoFixRequest {
    /// Limit how many pending reviews to touch in this call.
    #[serde(default)]
    pub(crate) limit: Option<i64>,
}

pub(crate) async fn auto_fix_preview(
    State(state): State<AppState>,
    _auth: Authenticated,
    Json(request): Json<AutoFixRequest>,
) -> ApiResult<Json<Value>> {
    let limit = request.limit.unwrap_or(500).clamp(1, 2000);
    let items = archivist_db::list_reviews(&state.pool, Some("pending"), limit).await?;
    let mut apply_count = 0_i64;
    let mut reject_count = 0_i64;
    let mut sample: Vec<Value> = Vec::with_capacity(20);
    for item in &items {
        let decision =
            clean_review_patch_for_auto_fix(&item.suggested_patch, &item.validation_warnings);
        match decision.action {
            AutoFixAction::Apply => apply_count += 1,
            AutoFixAction::Reject => reject_count += 1,
        }
        if sample.len() < 20 {
            sample.push(json!({
                "id": item.id,
                "paperless_document_id": item.paperless_document_id,
                "stage": item.stage,
                "action": match decision.action { AutoFixAction::Apply => "apply", AutoFixAction::Reject => "reject" },
                "fields_dropped": decision.fields_dropped,
            }));
        }
    }
    Ok(Json(json!({
        "total_pending": items.len(),
        "would_apply": apply_count,
        "would_reject": reject_count,
        "sample": sample,
    })))
}

pub(crate) async fn auto_fix_bulk(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<AutoFixRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    let limit = request.limit.unwrap_or(500).clamp(1, 2000);
    let items = archivist_db::list_reviews(&state.pool, Some("pending"), limit).await?;
    let mut applied = 0_i64;
    let mut rejected = 0_i64;
    let mut errors: Vec<Value> = Vec::new();

    for item in items {
        let outcome = auto_fix_apply_one(&state, &item, actor_id).await;
        match outcome {
            Ok(AutoFixAction::Apply) => applied += 1,
            Ok(AutoFixAction::Reject) => rejected += 1,
            Err(error) => {
                errors.push(json!({
                    "id": item.id,
                    "error": error.to_string(),
                }));
            }
        }
    }
    info!(applied, rejected, errors = errors.len(), %actor_id, "auto-fix bulk completed");
    Ok(Json(json!({
        "applied": applied,
        "rejected": rejected,
        "errors": errors,
    })))
}

pub(crate) async fn auto_fix_single(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    // Look the review up by ID instead of scanning the newest 2000 pending
    // rows, which missed older items in a large backlog. #395
    let Some(item) = archivist_db::get_pending_review(&state.pool, id).await? else {
        return Err(ApiError::bad_request(
            "review item not pending or not found",
        ));
    };
    let action = auto_fix_apply_one(&state, &item, actor_id).await?;
    Ok(Json(json!({
        "action": match action { AutoFixAction::Apply => "applied", AutoFixAction::Reject => "rejected" },
    })))
}

/// Auto-fix one review_item. Returns the action that was taken.
pub(crate) async fn auto_fix_apply_one(
    state: &AppState,
    item: &archivist_db::ReviewItemRecord,
    actor_id: Uuid,
) -> Result<AutoFixAction> {
    let decision =
        clean_review_patch_for_auto_fix(&item.suggested_patch, &item.validation_warnings);
    match decision.action {
        AutoFixAction::Apply => {
            // Stamp the cleaned patch onto the review_item as edited_patch,
            // then route through the existing approve+apply pipeline so the
            // audit trail and Paperless write semantics stay identical.
            // `edited` is already an applyable decision; a second
            // `approved` decision is rejected because the row is no longer
            // `pending`, which used to strand every auto-fixed review in
            // `edited` without an apply. #388
            review_decision(
                &state.pool,
                item.id,
                "edited",
                Some(decision.cleaned_patch.clone()),
                actor_id,
            )
            .await?;
            apply_review_patch(state, item.id, actor_id).await?;
            append_audit(
                &state.pool,
                AuditEventInput {
                    event_type: "review.auto_fix_applied".to_owned(),
                    actor_type: "user".to_owned(),
                    actor_id: Some(actor_id.to_string()),
                    run_id: item.run_id,
                    job_id: item.job_id,
                    paperless_document_id: Some(item.paperless_document_id),
                    before: None,
                    after: Some(json!({ "fields_dropped": decision.fields_dropped })),
                    metadata: Some(json!({ "review_id": item.id, "stage": item.stage })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
            Ok(AutoFixAction::Apply)
        }
        AutoFixAction::Reject => {
            review_decision(&state.pool, item.id, "rejected", None, actor_id).await?;
            append_audit(
                &state.pool,
                AuditEventInput {
                    event_type: "review.auto_fix_rejected".to_owned(),
                    actor_type: "user".to_owned(),
                    actor_id: Some(actor_id.to_string()),
                    run_id: item.run_id,
                    job_id: item.job_id,
                    paperless_document_id: Some(item.paperless_document_id),
                    before: None,
                    after: Some(json!({
                        "fields_dropped": decision.fields_dropped,
                        "reason": "no meaningful patch after cleanup",
                    })),
                    metadata: Some(json!({ "review_id": item.id, "stage": item.stage })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
            Ok(AutoFixAction::Reject)
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct EditReviewRequest {
    pub(crate) patch: DocumentPatch,
}

pub(crate) async fn edit_review(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<EditReviewRequest>,
) -> ApiResult<Json<Value>> {
    // Token requests carry the creator's user_id, so accepting them here
    // attributed automation decisions to that admin in the audit trail and
    // apply intents. All review decision endpoints now require an
    // interactive session, matching batch/auto-fix. #393
    let actor_id = auth.session_user_id()?;
    review_decision(
        &state.pool,
        id,
        "edited",
        Some(serde_json::to_value(request.patch)?),
        actor_id,
    )
    .await?;
    apply_review_patch(&state, id, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[tracing::instrument(
    skip(state),
    fields(
        review_id = %review_id,
        user_id = %actor_id,
        run_id = tracing::field::Empty,
        paperless_document_id = tracing::field::Empty
    )
)]
pub(crate) async fn apply_review_patch(
    state: &AppState,
    review_id: Uuid,
    actor_id: Uuid,
) -> Result<()> {
    let Some(review) = archivist_db::claim_review_for_apply(&state.pool, review_id).await? else {
        return Ok(());
    };
    // The row is now 'applying', which fences out a concurrent apply /
    // autopilot drain. Only failures that never produced a nonterminal
    // durable intent may be reverted immediately; ambiguous HTTP outcomes
    // stay fenced for the recovery worker instead of becoming blindly
    // retryable.
    let result = apply_claimed_review(state, &review, actor_id).await;
    if let Err(error) = &result
        && let Some(conflict) = error.downcast_ref::<ReviewApplyConflict>()
    {
        archivist_db::mark_review_apply_conflict(
            &state.pool,
            review_id,
            "pending",
            conflict.fields(),
            "user",
            Some(actor_id.to_string()),
        )
        .await?;
        return result;
    }
    if result.is_err() {
        match archivist_db::review_has_nonterminal_apply_intent(&state.pool, review_id).await {
            Ok(false) => {
                // Revert to `pending`, not the pre-claim `approved`/`edited`:
                // nothing (drain, UI, review_decision) ever picks those up
                // again, so a transient Paperless error used to strand the
                // review forever. The edited patch is kept. Settling the
                // failed intent right away lets a re-approval prepare a fresh
                // attempt for the same patch (#389). #388
                if let Err(error) =
                    archivist_db::revert_review_from_applying(&state.pool, review_id, "pending")
                        .await
                {
                    warn!(%review_id, error = %error, "failed to revert review after apply error");
                } else if let Err(error) =
                    archivist_db::finalize_failed_review_apply_intents(&state.pool, review_id).await
                {
                    warn!(%review_id, error = %error, "failed to settle failed apply intent");
                }
            }
            Ok(true) => warn!(
                %review_id,
                "review apply remains fenced while its Paperless intent is recovered"
            ),
            Err(error) => warn!(
                %review_id,
                error = %error,
                "could not prove review apply safe to revert; leaving it fenced"
            ),
        }
    }
    result
}

pub(crate) async fn apply_claimed_review(
    state: &AppState,
    review: &archivist_db::ReviewItemRecord,
    actor_id: Uuid,
) -> Result<()> {
    let review_id = review.id;
    if let Some(run_id) = review.run_id {
        Span::current().record("run_id", tracing::field::display(run_id));
    }
    Span::current().record("paperless_document_id", review.paperless_document_id);
    let patch_value = review
        .edited_patch
        .clone()
        .unwrap_or_else(|| review.suggested_patch.clone());
    let settings = get_runtime_settings(&state.pool).await?;
    let pending_new_objects =
        archivist_apply::PendingNewObjects::from_patch_value(&patch_value, &settings.workflow.tags);
    let mut patch: DocumentPatch = serde_json::from_value(patch_value)?;
    // run_id is None only for review items whose run was pruned by retention
    // (terminal runs only — a pending review keeps its run alive).
    let final_run_stage = if let (Some(run_id), Some(job_id)) = (review.run_id, review.job_id) {
        archivist_db::is_last_active_job(&state.pool, run_id, job_id).await?
    } else {
        false
    };
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    let mut tag_operations =
        review_workflow_tag_operations(&client, &settings, review.stage, final_run_stage).await?;
    // #404: model-proposed tags/correspondent exist only as names until the
    // review is approved; create them now and add them via the tag merge.
    let new_tag_ids = archivist_apply::materialize_pending_new_objects(
        &client,
        &pending_new_objects,
        archivist_apply::NewObjectPolicy::from_settings(&settings),
        &mut patch,
    )
    .await?;
    tag_operations.additions.extend(new_tag_ids);
    tag_operations.additions.sort_unstable();
    tag_operations.additions.dedup();
    let apply_started = std::time::Instant::now();
    let execution = apply_document(
        &state.pool,
        &client,
        ApplyRequest {
            source: "human_review".to_owned(),
            source_key: format!("review:{review_id}"),
            owner_type: "user".to_owned(),
            owner_id: actor_id.to_string(),
            paperless_document_id: review.paperless_document_id,
            run_id: review.run_id,
            job_id: review.job_id,
            review_id: Some(review.id),
            patch,
            before: None,
            metadata: json!({
                "stage": review.stage,
                "review_id": review.id
            }),
            // Recovery settles a failed human intent back to this status;
            // `pending` keeps the review decidable again. #388
            review_revert_status: Some("pending".to_owned()),
            review_precondition: Some(ReviewApplyPrecondition {
                baseline: review.baseline.clone(),
                tag_operations,
            }),
            allow_custom_fields_fallback: false,
        },
    )
    .await?;
    let duration_ms = apply_started.elapsed().as_millis() as u64;
    archivist_db::mark_review_applied(&state.pool, review_id, actor_id).await?;
    archivist_db::finalize_apply_intent(&state.pool, execution.attempt_id()).await?;
    info!(
        %review_id,
        run_id = ?review.run_id,
        paperless_document_id = review.paperless_document_id,
        duration_ms,
        "review patch applied to Paperless"
    );
    Ok(())
}

pub(crate) async fn review_workflow_tag_operations(
    client: &PaperlessClient,
    settings: &RuntimeSettings,
    stage: Stage,
    final_run_stage: bool,
) -> Result<ReviewTagOperations> {
    let all_tags = client.list_tags().await?;
    let completion = settings.workflow.tags.completion_tag_for_stage(stage);
    // #400: all triggers that requested the stage, incl. per-field metadata ones.
    let triggers = settings.workflow.tags.trigger_tags_requesting_stage(stage);
    let mut additions = Vec::new();
    let mut removals = Vec::new();
    if let Some(completion_name) = completion {
        let tag = client.ensure_tag(completion_name).await?;
        additions.push(tag.id);
    }
    if final_run_stage {
        let tag = client
            .ensure_tag(&settings.workflow.tags.completion_processed)
            .await?;
        additions.push(tag.id);
    }
    for trigger_name in triggers {
        if let Some(tag) = all_tags
            .iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(trigger_name))
        {
            removals.push(tag.id);
        }
    }
    if final_run_stage
        && let Some(tag) = all_tags.iter().find(|tag| {
            tag.name
                .eq_ignore_ascii_case(&settings.workflow.tags.trigger_process)
        })
    {
        removals.push(tag.id);
    }
    additions.sort_unstable();
    additions.dedup();
    removals.sort_unstable();
    removals.dedup();
    Ok(ReviewTagOperations {
        additions,
        removals,
    })
}
