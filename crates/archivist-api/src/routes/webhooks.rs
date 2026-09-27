//! Document trigger and Paperless consumption webhook endpoints.

use crate::*;

#[derive(Debug, Deserialize)]
pub(crate) struct TriggerRequest {
    pub(crate) stages: Option<Vec<Stage>>,
    pub(crate) mode: Option<ProcessingMode>,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(
        paperless_document_id = document_id,
        user_id = tracing::field::Empty,
        run_id = tracing::field::Empty
    )
)]
pub(crate) async fn trigger_document(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(document_id): Path<i32>,
    Json(request): Json<TriggerRequest>,
) -> ApiResult<Json<Value>> {
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    let stages = request
        .stages
        .unwrap_or_else(|| settings.workflow.enabled_stages.clone());
    let mode = request.mode.unwrap_or(settings.workflow.mode);
    // v1.4.0 priority scheduling: manual triggers carry priority 0 so an operator-initiated
    // run jumps ahead of every queued auto-selected run regardless of document age.
    let run_id = create_run_with_jobs_with_priority(
        &state.pool,
        document_id,
        &stages,
        mode,
        "manual",
        &auth.0.actor_type,
        Some(0),
    )
    .await?;
    Span::current().record("run_id", tracing::field::display(run_id));
    info!(%run_id, paperless_document_id = document_id, "manual run triggered");
    Ok(Json(json!({ "run_id": run_id })))
}

/// Inbound webhook body. Accepts either a batch (`document_ids`) or a single
/// (`document_id`) shape so a Paperless workflow can post whichever it has.
#[derive(Debug, Deserialize)]
pub(crate) struct WebhookConsumedRequest {
    #[serde(default)]
    pub(crate) document_ids: Option<Vec<i32>>,
    #[serde(default)]
    pub(crate) document_id: Option<i32>,
}

/// Machine-to-machine webhook: a Paperless workflow posts here when it consumes
/// a document so we trigger processing immediately instead of waiting for the
/// next ~60s poll.
///
/// This route lives OUTSIDE the auth-required router layer (no user session); it
/// is gated solely by the shared `ARCHIVIST_WEBHOOK_SECRET`, supplied in the
/// `X-Webhook-Secret` header and compared in constant time. When the env var is
/// unset the endpoint is disabled and returns `503`.
#[tracing::instrument(
    skip(state, headers, request),
    fields(queued = tracing::field::Empty)
)]
pub(crate) async fn webhook_paperless_document_consumed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<WebhookConsumedRequest>,
) -> ApiResult<Response> {
    let Some(expected) = state.config.webhook_secret.as_ref() else {
        return Err(ApiError::service_unavailable("webhook disabled"));
    };
    let provided = headers
        .get("x-webhook-secret")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let expected = expected.expose_secret();
    // Constant-time compare to deny timing oracles on the shared secret.
    if expected.len() != provided.len()
        || !bool::from(expected.as_bytes().ct_eq(provided.as_bytes()))
    {
        return Err(ApiError::unauthorized("invalid webhook secret"));
    }

    // Merge both accepted shapes, drop non-positive ids, and de-duplicate so a
    // single payload never enqueues the same document twice.
    let mut normalized: Vec<i32> = Vec::new();
    for document_id in request
        .document_ids
        .into_iter()
        .flatten()
        .chain(request.document_id)
    {
        if document_id <= 0 {
            return Err(ApiError::bad_request(
                "document ids must be positive Paperless document IDs",
            ));
        }
        if !normalized.contains(&document_id) {
            normalized.push(document_id);
        }
    }
    if normalized.is_empty() {
        return Err(ApiError::bad_request(
            "document_ids or document_id is required",
        ));
    }

    let settings = get_runtime_settings(&state.pool).await?;
    let stages = settings.workflow.enabled_stages.clone();
    let mode = settings.workflow.mode;
    // Webhook-triggered runs carry priority 0 (same as manual triggers) so a
    // freshly consumed document jumps ahead of queued auto-selected work.
    let mut queued: i64 = 0;
    for document_id in normalized {
        create_run_with_jobs_with_priority(
            &state.pool,
            document_id,
            &stages,
            mode,
            "webhook",
            "webhook",
            Some(0),
        )
        .await?;
        queued += 1;
    }
    Span::current().record("queued", queued);
    info!(queued, "webhook enqueued documents");
    Ok((StatusCode::ACCEPTED, Json(json!({ "queued": queued }))).into_response())
}
