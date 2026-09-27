//! Workflow controls, batch queueing/reruns and recovery operations.

use crate::*;

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateWorkflowModeRequest {
    pub(crate) mode: ProcessingMode,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, mode = tracing::field::Empty)
)]
pub(crate) async fn update_workflow_mode(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<UpdateWorkflowModeRequest>,
) -> ApiResult<Json<RuntimeSettings>> {
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    Span::current().record("mode", tracing::field::debug(request.mode));
    let mut settings = get_runtime_settings(&state.pool).await?;
    settings.workflow.mode = request.mode;
    update_runtime_settings(&state.pool, &settings, actor_id).await?;
    info!(%actor_id, mode = ?request.mode, "workflow mode updated");
    Ok(Json(settings))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateWorkflowControlsRequest {
    pub(crate) paused: Option<bool>,
    pub(crate) dry_run: Option<bool>,
    pub(crate) hourly_document_limit: Option<Option<i64>>,
    pub(crate) daily_document_limit: Option<Option<i64>>,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty)
)]
pub(crate) async fn update_workflow_controls(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<UpdateWorkflowControlsRequest>,
) -> ApiResult<Json<RuntimeSettings>> {
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let before = get_runtime_settings(&state.pool).await?;
    let mut settings = before.clone();
    if let Some(paused) = request.paused {
        settings.workflow.paused = paused;
    }
    if let Some(dry_run) = request.dry_run {
        settings.workflow.dry_run = dry_run;
    }
    if let Some(limit) = request.hourly_document_limit {
        settings.workflow.hourly_document_limit = limit;
    }
    if let Some(limit) = request.daily_document_limit {
        settings.workflow.daily_document_limit = limit;
    }
    settings = settings.normalized();
    update_runtime_settings(&state.pool, &settings, actor_id).await?;

    let event_type = match (before.workflow.paused, settings.workflow.paused) {
        (false, true) => "workflow.paused",
        (true, false) => "workflow.resumed",
        _ => "workflow.controls_updated",
    };
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: event_type.to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: Some(json!({
                "paused": before.workflow.paused,
                "dry_run": before.workflow.dry_run,
                "hourly_document_limit": before.workflow.hourly_document_limit,
                "daily_document_limit": before.workflow.daily_document_limit
            })),
            after: Some(json!({
                "paused": settings.workflow.paused,
                "dry_run": settings.workflow.dry_run,
                "hourly_document_limit": settings.workflow.hourly_document_limit,
                "daily_document_limit": settings.workflow.daily_document_limit
            })),
            metadata: Some(json!({ "source": "workflow_controls" })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    info!(
        %actor_id,
        event = event_type,
        paused = settings.workflow.paused,
        dry_run = settings.workflow.dry_run,
        "workflow controls updated"
    );
    Ok(Json(settings))
}

#[tracing::instrument(
    skip(state, auth),
    fields(user_id = tracing::field::Empty, queued = tracing::field::Empty)
)]
pub(crate) async fn queue_ocr_batch(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    let created = queue_missing_stage(
        &state.pool,
        Stage::Ocr,
        settings.workflow.mode,
        &auth.0.actor_type,
        &settings.workflow.rules,
        None,
    )
    .await?;
    Span::current().record("queued", created);
    info!(queued = created, "queued missing OCR documents");
    Ok(Json(json!({ "queued": created })))
}

#[tracing::instrument(
    skip(state, auth),
    fields(user_id = tracing::field::Empty, queued = tracing::field::Empty)
)]
pub(crate) async fn queue_full_batch(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    // Emit ONE pipeline_run per eligible document with the full enabled-stages array
    // (e.g. `["ocr","metadata"]`), so the document drains the entire pipeline within a
    // single run. The previous per-stage loop created separate single-stage runs which
    // forced operators to press "Queue Full" twice to advance both stages.
    let created = queue_missing_pipeline(
        &state.pool,
        &settings.workflow.enabled_stages,
        settings.workflow.mode,
        "manual-batch",
        &auth.0.actor_type,
        &settings.workflow.rules,
        None,
    )
    .await?;
    Span::current().record("queued", created);
    info!(queued = created, "queued full pipeline batch");
    Ok(Json(json!({ "queued": created })))
}

/// Same per-request ceiling as the other bulk endpoints; each ID becomes one
/// document advisory lock, so an unbounded list could exhaust the shared
/// lock table. #390
pub(crate) const MAX_RERUN_BATCH_DOCUMENTS: usize = 500;

pub(crate) fn validate_rerun_document_ids(document_ids: &[i32]) -> ApiResult<()> {
    if document_ids.is_empty() {
        return Err(ApiError::bad_request("document_ids must not be empty"));
    }
    if document_ids.len() > MAX_RERUN_BATCH_DOCUMENTS {
        return Err(ApiError::bad_request(format!(
            "rerun batch is limited to {MAX_RERUN_BATCH_DOCUMENTS} documents per request"
        )));
    }
    if let Some(invalid) = document_ids.iter().find(|id| **id <= 0) {
        return Err(ApiError::bad_request(format!(
            "document_ids must be positive Paperless document IDs (got {invalid})"
        )));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub(crate) struct RerunBatchRequest {
    pub(crate) document_ids: Vec<i32>,
    /// Stage names (e.g. `["ocr","metadata"]`). Validated against the known [`Stage`] set.
    pub(crate) stages: Vec<String>,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, queued = tracing::field::Empty)
)]
pub(crate) async fn rerun_batch(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<RerunBatchRequest>,
) -> ApiResult<Json<Value>> {
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }

    validate_rerun_document_ids(&request.document_ids)?;
    if request.stages.is_empty() {
        return Err(ApiError::bad_request("stages must not be empty"));
    }

    // Validate every requested stage against the known Stage set before queueing anything.
    let stages = request
        .stages
        .iter()
        .map(|raw| {
            raw.parse::<Stage>()
                .map_err(|_| ApiError::bad_request(format!("unknown stage: {raw}")))
        })
        .collect::<Result<Vec<Stage>, _>>()?;

    // De-duplicate ids so a doubled id can't enqueue (or attempt) two runs.
    let mut document_ids: Vec<i32> = request.document_ids;
    document_ids.sort_unstable();
    document_ids.dedup();

    let settings = get_runtime_settings(&state.pool).await?;
    // Operator-initiated re-run jumps ahead of age-derived auto-selected runs (priority 0),
    // mirroring the manual single-document trigger.
    let queued = create_runs_for_documents(
        &state.pool,
        &document_ids,
        &stages,
        settings.workflow.mode,
        "bulk-rerun",
        &auth.0.actor_type,
        Some(0),
    )
    .await?;
    Span::current().record("queued", queued);
    info!(queued, "queued bulk re-run batch");
    Ok(Json(json!({ "queued": queued })))
}

/// Re-run every document the dashboard counts as failed (a failed `ocr` or
/// `metadata` stage, no active run) in one click, so an operator does not have
/// to filter the inventory and hand-select after an upstream incident.
/// Idempotent: documents already being reprocessed are skipped by the
/// per-document active-run guard, so a double click cannot duplicate runs.
pub(crate) async fn rerun_failed_batch(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }

    let document_ids = failed_document_ids(&state.pool).await?;
    if document_ids.is_empty() {
        return Ok(Json(json!({ "queued": 0, "candidates": 0 })));
    }

    let settings = get_runtime_settings(&state.pool).await?;
    // Re-run both stages: OCR is page-cached so a still-good result is cheap to
    // revalidate and a failed OCR is redone. Priority 0 jumps ahead of
    // age-derived auto-selected runs, mirroring the hand-picked rerun.
    let queued = create_runs_for_documents(
        &state.pool,
        &document_ids,
        &[Stage::Ocr, Stage::Metadata],
        settings.workflow.mode,
        "rerun-failed",
        &auth.0.actor_type,
        Some(0),
    )
    .await?;
    Span::current().record("queued", queued);
    info!(
        queued,
        candidates = document_ids.len(),
        "queued re-run of all failed documents"
    );
    Ok(Json(
        json!({ "queued": queued, "candidates": document_ids.len() }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RecoveryQuery {
    pub(crate) older_than_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RecoveryRequest {
    pub(crate) older_than_seconds: Option<i64>,
}

pub(crate) fn recovery_window_seconds(value: Option<i64>) -> i64 {
    value.unwrap_or(600).clamp(60, 86_400)
}

pub(crate) async fn recovery_status(
    State(state): State<AppState>,
    _auth: Authenticated,
    Query(query): Query<RecoveryQuery>,
) -> ApiResult<Json<Value>> {
    let older_than_seconds = recovery_window_seconds(query.older_than_seconds);
    Ok(Json(json!({
        "older_than_seconds": older_than_seconds,
        "items": recovery_candidates(&state.pool, older_than_seconds).await?
    })))
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, older_than_seconds = tracing::field::Empty)
)]
pub(crate) async fn recover_stale_leases_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<RecoveryRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let older_than_seconds = recovery_window_seconds(request.older_than_seconds);
    Span::current().record("older_than_seconds", older_than_seconds);
    let summary = recover_stale_leases(&state.pool, older_than_seconds, actor_id).await?;
    info!(
        %actor_id,
        older_than_seconds,
        ?summary,
        "stale leases recovered"
    );
    Ok(Json(json!({
        "older_than_seconds": older_than_seconds,
        "summary": summary
    })))
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, older_than_seconds = tracing::field::Empty)
)]
pub(crate) async fn recover_stuck_runs_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<RecoveryRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let older_than_seconds = recovery_window_seconds(request.older_than_seconds);
    Span::current().record("older_than_seconds", older_than_seconds);
    let summary = recover_stuck_runs(&state.pool, older_than_seconds, actor_id).await?;
    info!(
        %actor_id,
        older_than_seconds,
        ?summary,
        "stuck runs recovered"
    );
    Ok(Json(json!({
        "older_than_seconds": older_than_seconds,
        "summary": summary
    })))
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct UnblockJobsRequest {
    /// Optional ILIKE pattern; when set, only failed predecessor jobs
    /// whose `error_message` contains the substring are re-queued.
    /// Useful for unblocking only the post-quota cohort while leaving
    /// genuine code-bug failures pinned.
    #[serde(default)]
    pub(crate) error_substring: Option<String>,
    /// When true (default), also drop every active provider cooldown
    /// so the next claim cycle retries the providers immediately.
    /// Set to false to unblock the queue but keep cooldowns in place
    /// (e.g. operator knows the provider is still rate-limited).
    #[serde(default = "default_true")]
    pub(crate) clear_provider_cooldowns: bool,
}

pub(crate) fn default_true() -> bool {
    true
}

#[tracing::instrument(skip(state, auth, request), fields(user_id = tracing::field::Empty))]
pub(crate) async fn unblock_jobs_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<UnblockJobsRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let summary = archivist_db::unblock_jobs_from_failed_predecessors(
        &state.pool,
        request.error_substring.as_deref(),
    )
    .await?;
    let (cooldowns_cleared, retries_released) = if request.clear_provider_cooldowns {
        let cleared = archivist_db::clear_all_provider_cooldowns(&state.pool).await?;
        // Lifting the cooldowns must also wake the jobs they parked: their
        // `run_after` sits at the (now-irrelevant) cooldown end, so without
        // this the queue would keep waiting it out despite the cooldown being
        // gone (mirrors clear_provider_cooldowns_endpoint). #306
        let released = archivist_db::release_scheduled_retries(&state.pool).await?;
        (cleared, released)
    } else {
        (0, 0)
    };
    info!(
        %actor_id,
        predecessors_requeued = summary.predecessors_requeued,
        runs_unblocked = summary.runs_unblocked,
        cooldowns_cleared,
        retries_released,
        error_substring = ?request.error_substring,
        "operator unblocked queued jobs"
    );
    let _ = archivist_db::append_audit(
        &state.pool,
        archivist_core::AuditEventInput {
            event_type: "operations.jobs_unblocked".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "predecessors_requeued": summary.predecessors_requeued,
                "runs_unblocked": summary.runs_unblocked,
                "cooldowns_cleared": cooldowns_cleared,
                "retries_released": retries_released,
            })),
            metadata: Some(json!({
                "error_substring": request.error_substring,
                "clear_provider_cooldowns": request.clear_provider_cooldowns,
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await;
    Ok(Json(json!({
        "predecessors_requeued": summary.predecessors_requeued,
        "runs_unblocked": summary.runs_unblocked,
        "cooldowns_cleared": cooldowns_cleared,
        "retries_released": retries_released,
    })))
}

pub(crate) async fn provider_cooldowns_endpoint(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let cooldowns = archivist_db::list_active_provider_cooldowns(&state.pool).await?;
    let payload = cooldowns
        .into_iter()
        .map(|c| {
            json!({
                "provider_name": c.provider_name,
                "cooldown_until": c.cooldown_until,
                "reason": c.reason,
                "set_at": c.set_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "cooldowns": payload })))
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ClearProviderCooldownRequest {
    /// Optional — clear only this provider's cooldown. When None, all
    /// active cooldowns are cleared.
    #[serde(default)]
    pub(crate) provider_name: Option<String>,
}

#[tracing::instrument(skip(state, auth, request), fields(user_id = tracing::field::Empty))]
pub(crate) async fn clear_provider_cooldowns_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<ClearProviderCooldownRequest>,
) -> ApiResult<Json<Value>> {
    // WriteRuns, not WriteSettings: cooldown manipulation is queue/run
    // recovery, and unblock_jobs_endpoint already wipes cooldowns under
    // WriteRuns — requiring more here only forced operators through the
    // unblock detour for the exact same effect (#313).
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let cleared = match request.provider_name.as_deref() {
        Some(name) => archivist_db::clear_provider_cooldown(&state.pool, name).await?,
        None => archivist_db::clear_all_provider_cooldowns(&state.pool).await?,
    };
    // Lifting a cooldown must also wake the jobs it parked: their `run_after`
    // sits at the (now-irrelevant) cooldown end, so without this the queue
    // would keep waiting it out despite the cooldown being gone. (prod-blocked)
    let released = archivist_db::release_scheduled_retries(&state.pool).await?;
    let _ = archivist_db::append_audit(
        &state.pool,
        archivist_core::AuditEventInput {
            event_type: "ai.provider_cooldown_cleared".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "provider_name": request.provider_name,
                "cleared": cleared,
                "released": released,
            })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await;
    Ok(Json(json!({ "cleared": cleared, "released": released })))
}

/// Wake jobs that a provider cooldown (or other backoff) deferred into the
/// future, so the worker claims them immediately instead of waiting out the
/// cooldown window. Operator-triggered counterpart to the automatic release on
/// a model change; also reachable from the dashboard.
#[tracing::instrument(skip(state, auth), fields(user_id = tracing::field::Empty))]
pub(crate) async fn release_scheduled_retries_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let released = archivist_db::release_scheduled_retries(&state.pool).await?;
    info!(%actor_id, released, "operator released scheduled job retries");
    let _ = archivist_db::append_audit(
        &state.pool,
        archivist_core::AuditEventInput {
            event_type: "operations.scheduled_retries_released".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "released": released })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await;
    Ok(Json(json!({ "released": released })))
}
