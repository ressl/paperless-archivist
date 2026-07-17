//! Durable, resumable Paperless PATCH orchestration.
//!
//! A Paperless PATCH cannot be made exactly once across process crashes: the
//! request may have reached Paperless even when the caller never received a
//! response. This module therefore persists an immutable intent before HTTP,
//! marks it in flight immediately before the request, and reconciles ambiguous
//! outcomes with a GET. An existing in-flight intent is never patched again.

use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use archivist_core::DocumentPatch;
use archivist_db::{
    ApplyIntentInput, ApplyIntentRecord, DbPool, fail_apply_intent, finalize_apply_intent,
    finalize_failed_apply_intent, get_apply_intent, get_recoverable_apply_intent_by_source_key,
    get_review_status, list_recoverable_review_apply_intents, mark_apply_intent_confirmed,
    mark_apply_intent_in_flight, mark_review_applied, mark_review_auto_applied,
    prepare_apply_intent, reconcile_apply_intent, revert_review_from_applying,
    revert_review_to_pending_after_failed_drain,
};
use archivist_paperless::{PaperlessClient, PaperlessError, document_matches_patch};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ApplyRequest {
    pub source: String,
    pub source_key: String,
    pub owner_type: String,
    pub owner_id: String,
    pub paperless_document_id: i32,
    pub run_id: Option<Uuid>,
    pub job_id: Option<Uuid>,
    pub review_id: Option<Uuid>,
    pub patch: DocumentPatch,
    pub before: Option<Value>,
    pub metadata: Value,
    pub review_revert_status: Option<String>,
    /// Retry once without custom fields when Paperless explicitly rejects that
    /// part of the request with HTTP 400. The rejected attempt remains audited.
    pub allow_custom_fields_fallback: bool,
}

#[derive(Debug, Clone)]
pub enum ApplyExecution {
    Confirmed {
        attempt_id: Uuid,
        applied_patch: DocumentPatch,
        custom_fields_dropped: bool,
    },
    Reconciled {
        attempt_id: Uuid,
        applied_patch: DocumentPatch,
        custom_fields_dropped: bool,
    },
    Finalized {
        attempt_id: Uuid,
        applied_patch: DocumentPatch,
        custom_fields_dropped: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct RecoverySummary {
    pub examined: usize,
    pub applied: usize,
    pub failed_settled: usize,
    pub deferred: usize,
}

impl ApplyExecution {
    pub fn attempt_id(&self) -> Uuid {
        match self {
            Self::Confirmed { attempt_id, .. }
            | Self::Reconciled { attempt_id, .. }
            | Self::Finalized { attempt_id, .. } => *attempt_id,
        }
    }

    pub fn applied_patch(&self) -> &DocumentPatch {
        match self {
            Self::Confirmed { applied_patch, .. }
            | Self::Reconciled { applied_patch, .. }
            | Self::Finalized { applied_patch, .. } => applied_patch,
        }
    }

    pub fn custom_fields_dropped(&self) -> bool {
        match self {
            Self::Confirmed {
                custom_fields_dropped,
                ..
            }
            | Self::Reconciled {
                custom_fields_dropped,
                ..
            }
            | Self::Finalized {
                custom_fields_dropped,
                ..
            } => *custom_fields_dropped,
        }
    }
}

/// Hash the exact serialized PATCH body. `DocumentPatch` is a struct, so its
/// field order is stable and no map canonicalization ambiguity is introduced.
pub fn patch_hash(patch: &DocumentPatch) -> Result<String> {
    let bytes = serde_json::to_vec(patch).context("serialize Paperless patch for hashing")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

pub async fn apply_document(
    pool: &DbPool,
    client: &PaperlessClient,
    request: ApplyRequest,
) -> Result<ApplyExecution> {
    apply_document_inner(pool, client, request, false).await
}

/// Resume an unfinished logical source using its persisted body. Job callers
/// invoke this before rebuilding/pruning a patch after lease loss.
pub async fn resume_apply_source(
    pool: &DbPool,
    client: &PaperlessClient,
    source_key: &str,
) -> Result<Option<ApplyExecution>> {
    let Some(intent) = get_recoverable_apply_intent_by_source_key(pool, source_key).await? else {
        return Ok(None);
    };
    let request = request_from_intent(&intent)?;
    apply_document(pool, client, request).await.map(Some)
}

async fn apply_document_inner(
    pool: &DbPool,
    client: &PaperlessClient,
    mut request: ApplyRequest,
    mut custom_fields_dropped: bool,
) -> Result<ApplyExecution> {
    loop {
        let patch = serde_json::to_value(&request.patch)
            .context("serialize Paperless patch for apply intent")?;
        let intent = prepare_apply_intent(
            pool,
            &ApplyIntentInput {
                source: request.source.clone(),
                source_key: request.source_key.clone(),
                owner_type: request.owner_type.clone(),
                owner_id: request.owner_id.clone(),
                paperless_document_id: request.paperless_document_id,
                run_id: request.run_id,
                job_id: request.job_id,
                review_id: request.review_id,
                patch_hash: patch_hash(&request.patch)?,
                patch,
                before: request.before.clone(),
                metadata: request.metadata.clone(),
                review_revert_status: request.review_revert_status.clone(),
            },
        )
        .await?;

        match intent.state.as_str() {
            "confirmed" => {
                return Ok(ApplyExecution::Confirmed {
                    attempt_id: intent.attempt_id,
                    applied_patch: request.patch,
                    custom_fields_dropped,
                });
            }
            "reconciled" => {
                return Ok(ApplyExecution::Reconciled {
                    attempt_id: intent.attempt_id,
                    applied_patch: request.patch,
                    custom_fields_dropped,
                });
            }
            "finalized" => {
                return Ok(ApplyExecution::Finalized {
                    attempt_id: intent.attempt_id,
                    applied_patch: request.patch,
                    custom_fields_dropped,
                });
            }
            "failed" => {
                return Err(anyhow!(
                    "Paperless apply attempt {} already failed: {}",
                    intent.attempt_id,
                    intent.last_error.as_deref().unwrap_or("unknown error")
                ));
            }
            "in_flight" => {
                return reconcile_existing(pool, client, &request, &intent, custom_fields_dropped)
                    .await;
            }
            "prepared" => {}
            state => return Err(anyhow!("unknown Paperless apply state {state}")),
        }

        if !mark_apply_intent_in_flight(pool, intent.attempt_id, &request.owner_id).await? {
            let current = get_apply_intent(pool, intent.attempt_id)
                .await?
                .ok_or_else(|| anyhow!("Paperless apply intent disappeared after claim race"))?;
            if current.state == "prepared" {
                continue;
            }
            return resume_existing(pool, client, &request, current, custom_fields_dropped).await;
        }

        let started = Instant::now();
        match client
            .patch_document(request.paperless_document_id, &request.patch)
            .await
        {
            Ok(document) => {
                let response = serde_json::to_value(document)
                    .context("serialize confirmed Paperless response")?;
                let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
                if mark_apply_intent_confirmed(
                    pool,
                    intent.attempt_id,
                    &request.owner_id,
                    Some(response),
                    duration_ms,
                )
                .await?
                {
                    return Ok(ApplyExecution::Confirmed {
                        attempt_id: intent.attempt_id,
                        applied_patch: request.patch,
                        custom_fields_dropped,
                    });
                }
                let current = get_apply_intent(pool, intent.attempt_id)
                    .await?
                    .ok_or_else(|| anyhow!("Paperless apply intent disappeared after PATCH"))?;
                return resume_existing(pool, client, &request, current, custom_fields_dropped)
                    .await;
            }
            Err(patch_error) => match client.get_document(request.paperless_document_id).await {
                Ok(document) if document_matches_patch(&document, &request.patch) => {
                    let response = serde_json::to_value(document)
                        .context("serialize reconciled Paperless response")?;
                    if reconcile_apply_intent(
                        pool,
                        intent.attempt_id,
                        &request.owner_id,
                        Some(response),
                    )
                    .await?
                    {
                        return Ok(ApplyExecution::Reconciled {
                            attempt_id: intent.attempt_id,
                            applied_patch: request.patch,
                            custom_fields_dropped,
                        });
                    }
                    let current = get_apply_intent(pool, intent.attempt_id)
                        .await?
                        .ok_or_else(|| {
                            anyhow!("Paperless apply intent disappeared during reconciliation")
                        })?;
                    return resume_existing(pool, client, &request, current, custom_fields_dropped)
                        .await;
                }
                Ok(_) => {
                    let message = format!(
                        "ambiguous Paperless PATCH did not match current document: {patch_error:#}"
                    );
                    fail_apply_intent(pool, intent.attempt_id, &request.owner_id, &message).await?;

                    if request.allow_custom_fields_fallback
                        && request.patch.custom_fields.is_some()
                        && is_custom_fields_bad_request(&patch_error)
                    {
                        request.patch.custom_fields = None;
                        request.allow_custom_fields_fallback = false;
                        custom_fields_dropped = true;
                        continue;
                    }
                    return Err(anyhow!(message));
                }
                Err(read_error) => {
                    return Err(anyhow!(
                        "Paperless PATCH outcome is ambiguous and reconciliation failed; attempt {} remains in flight: patch={patch_error:#}; get={read_error:#}",
                        intent.attempt_id
                    ));
                }
            },
        }
    }
}

/// Resume review-backed intents after a process crash. Each row is handled
/// independently so one unavailable document cannot block the rest of the
/// recovery batch. Direct job intents are intentionally resumed by the normal
/// job lease/retry path and are not returned by the DB query.
pub async fn recover_review_apply_intents(
    pool: &DbPool,
    client: &PaperlessClient,
    limit: i64,
) -> Result<RecoverySummary> {
    let intents = list_recoverable_review_apply_intents(pool, limit).await?;
    let mut summary = RecoverySummary {
        examined: intents.len(),
        ..RecoverySummary::default()
    };

    for intent in intents {
        if intent.state == "failed" {
            match settle_failed_review(pool, &intent).await {
                Ok(()) => summary.failed_settled += 1,
                Err(error) => {
                    summary.deferred += 1;
                    tracing::warn!(
                        attempt_id = %intent.attempt_id,
                        error = %error,
                        "failed to settle terminal Paperless review intent"
                    );
                }
            }
            continue;
        }

        let request = match request_from_intent(&intent) {
            Ok(request) => request,
            Err(error) => {
                summary.deferred += 1;
                tracing::warn!(
                    attempt_id = %intent.attempt_id,
                    error = %error,
                    "failed to decode persisted Paperless review patch"
                );
                continue;
            }
        };

        match apply_document(pool, client, request).await {
            Ok(execution) => {
                match settle_applied_review(pool, &intent, execution.attempt_id()).await {
                    Ok(()) => summary.applied += 1,
                    Err(error) => {
                        summary.deferred += 1;
                        tracing::warn!(
                            attempt_id = %intent.attempt_id,
                            error = %error,
                            "failed to finalize recovered Paperless review intent"
                        );
                    }
                }
            }
            Err(error) => {
                let current = get_apply_intent(pool, intent.attempt_id).await?;
                if let Some(current) = current
                    && current.state == "failed"
                {
                    match settle_failed_review(pool, &current).await {
                        Ok(()) => summary.failed_settled += 1,
                        Err(settle_error) => {
                            summary.deferred += 1;
                            tracing::warn!(
                                attempt_id = %intent.attempt_id,
                                error = %settle_error,
                                "failed to settle reconciled mismatch"
                            );
                        }
                    }
                } else {
                    summary.deferred += 1;
                    tracing::warn!(
                        attempt_id = %intent.attempt_id,
                        error = %error,
                        "Paperless review intent remains deferred"
                    );
                }
            }
        }
    }
    Ok(summary)
}

fn request_from_intent(intent: &ApplyIntentRecord) -> Result<ApplyRequest> {
    let patch: DocumentPatch = serde_json::from_value(intent.patch.clone())
        .context("decode persisted Paperless apply patch")?;
    Ok(ApplyRequest {
        source: intent.source.clone(),
        source_key: intent.source_key.clone(),
        owner_type: intent.owner_type.clone(),
        // Preserve the initiating actor. The durable row already fences the
        // attempt; changing this value would lose a human reviewer ID.
        owner_id: intent.owner_id.clone(),
        paperless_document_id: intent.paperless_document_id,
        run_id: intent.run_id,
        job_id: intent.job_id,
        review_id: intent.review_id,
        patch,
        before: intent.before.clone(),
        metadata: intent.metadata.clone(),
        review_revert_status: intent.review_revert_status.clone(),
        allow_custom_fields_fallback: false,
    })
}

async fn settle_applied_review(
    pool: &DbPool,
    intent: &ApplyIntentRecord,
    attempt_id: Uuid,
) -> Result<()> {
    let review_id = intent
        .review_id
        .ok_or_else(|| anyhow!("review apply intent has no review ID"))?;
    let status = get_review_status(pool, review_id)
        .await?
        .ok_or_else(|| anyhow!("review apply intent references a missing review"))?;
    if status != "applying" && status != "applied" {
        return Err(anyhow!(
            "cannot finalize successful review apply while review is {status}"
        ));
    }
    match intent.source.as_str() {
        "human_review" => {
            let actor_id = Uuid::parse_str(&intent.owner_id)
                .context("persisted human review owner is not a UUID")?;
            mark_review_applied(pool, review_id, actor_id).await?;
        }
        "autopilot_drain" => mark_review_auto_applied(pool, review_id).await?,
        source => return Err(anyhow!("unknown review apply source {source}")),
    }
    if get_review_status(pool, review_id).await?.as_deref() != Some("applied") {
        return Err(anyhow!("review did not reach applied during recovery"));
    }
    finalize_apply_intent(pool, attempt_id).await?;
    Ok(())
}

async fn settle_failed_review(pool: &DbPool, intent: &ApplyIntentRecord) -> Result<()> {
    let review_id = intent
        .review_id
        .ok_or_else(|| anyhow!("review apply intent has no review ID"))?;
    let current_status = get_review_status(pool, review_id)
        .await?
        .ok_or_else(|| anyhow!("review apply intent references a missing review"))?;
    match intent.source.as_str() {
        "human_review" => {
            let status = intent
                .review_revert_status
                .as_deref()
                .ok_or_else(|| anyhow!("failed human review intent has no safe revert status"))?;
            if current_status == "applying" {
                revert_review_from_applying(pool, review_id, status).await?;
            }
        }
        "autopilot_drain" => {
            if current_status == "applying" {
                revert_review_to_pending_after_failed_drain(pool, review_id).await?;
            }
        }
        source => return Err(anyhow!("unknown review apply source {source}")),
    }
    let expected = intent.review_revert_status.as_deref().unwrap_or("pending");
    let status = get_review_status(pool, review_id).await?;
    if status.as_deref() != Some(expected) {
        return Err(anyhow!(
            "failed review intent expected review status {expected}, got {}",
            status.as_deref().unwrap_or("missing")
        ));
    }
    finalize_failed_apply_intent(pool, intent.attempt_id).await?;
    Ok(())
}

async fn reconcile_existing(
    pool: &DbPool,
    client: &PaperlessClient,
    request: &ApplyRequest,
    intent: &ApplyIntentRecord,
    custom_fields_dropped: bool,
) -> Result<ApplyExecution> {
    let document = client
        .get_document(request.paperless_document_id)
        .await
        .with_context(|| {
            format!(
                "reconcile in-flight Paperless apply attempt {}",
                intent.attempt_id
            )
        })?;
    if !document_matches_patch(&document, &request.patch) {
        let message = format!(
            "ambiguous in-flight Paperless apply attempt {} does not match current document",
            intent.attempt_id
        );
        fail_apply_intent(pool, intent.attempt_id, &request.owner_id, &message).await?;
        return Err(anyhow!(message));
    }
    let response = serde_json::to_value(document).context("serialize reconciled document")?;
    reconcile_apply_intent(pool, intent.attempt_id, &request.owner_id, Some(response)).await?;
    Ok(ApplyExecution::Reconciled {
        attempt_id: intent.attempt_id,
        applied_patch: request.patch.clone(),
        custom_fields_dropped,
    })
}

async fn resume_existing(
    pool: &DbPool,
    client: &PaperlessClient,
    request: &ApplyRequest,
    intent: ApplyIntentRecord,
    custom_fields_dropped: bool,
) -> Result<ApplyExecution> {
    match intent.state.as_str() {
        "in_flight" => {
            reconcile_existing(pool, client, request, &intent, custom_fields_dropped).await
        }
        "confirmed" => Ok(ApplyExecution::Confirmed {
            attempt_id: intent.attempt_id,
            applied_patch: request.patch.clone(),
            custom_fields_dropped,
        }),
        "reconciled" => Ok(ApplyExecution::Reconciled {
            attempt_id: intent.attempt_id,
            applied_patch: request.patch.clone(),
            custom_fields_dropped,
        }),
        "finalized" => Ok(ApplyExecution::Finalized {
            attempt_id: intent.attempt_id,
            applied_patch: request.patch.clone(),
            custom_fields_dropped,
        }),
        "failed" => Err(anyhow!(
            "Paperless apply attempt {} already failed: {}",
            intent.attempt_id,
            intent.last_error.as_deref().unwrap_or("unknown error")
        )),
        state => Err(anyhow!(
            "Paperless apply attempt {} is unexpectedly {state}",
            intent.attempt_id
        )),
    }
}

fn is_custom_fields_bad_request(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<PaperlessError>(),
        Some(PaperlessError::Client { status: 400, body })
            if body.to_ascii_lowercase().contains("custom")
    )
}
