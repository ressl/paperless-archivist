//! Durable, resumable Paperless PATCH orchestration.
//!
//! A Paperless PATCH cannot be made exactly once across process crashes: the
//! request may have reached Paperless even when the caller never received a
//! response. This module therefore persists an immutable intent before HTTP,
//! marks it in flight immediately before the request, and reconciles ambiguous
//! outcomes with a GET. An existing in-flight intent is never patched again.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
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
use archivist_paperless::{
    PaperlessClient, PaperlessDocumentDetail, PaperlessError, document_matches_patch,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
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
    /// Present only for the first execution of a review-backed apply. The
    /// resolved patch is persisted in the durable intent; recovery therefore
    /// leaves this `None` and reconciles that exact body.
    pub review_precondition: Option<ReviewApplyPrecondition>,
    /// Retry once without custom fields when Paperless explicitly rejects that
    /// part of the request with HTTP 400. The rejected attempt remains audited.
    pub allow_custom_fields_fallback: bool,
}

/// Field-scoped optimistic-concurrency input for review-backed applies.
/// Direct worker applies deliberately omit it because they are generated and
/// applied in one lease-owned operation rather than waiting for a reviewer.
#[derive(Debug, Clone)]
pub struct ReviewApplyPrecondition {
    pub baseline: Value,
    pub tag_operations: ReviewTagOperations,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewTagOperations {
    pub additions: Vec<i32>,
    pub removals: Vec<i32>,
}

/// A safe, user-actionable refusal to overwrite fields changed after review
/// creation. Only field names are retained; values never enter the error or
/// conflict audit event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewApplyConflict {
    fields: Vec<String>,
}

impl ReviewApplyConflict {
    pub fn fields(&self) -> &[String] {
        &self.fields
    }
}

impl Display for ReviewApplyConflict {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "review conflicts with newer Paperless changes in: {}",
            self.fields.join(", ")
        )
    }
}

impl std::error::Error for ReviewApplyConflict {}

/// Capture every patchable field so a reviewer may safely edit additional
/// fields before approval. Potentially sensitive strings and custom-field
/// values are represented by deterministic SHA-256 fingerprints.
pub fn review_apply_baseline(document: &PaperlessDocumentDetail) -> Value {
    let mut baseline = Map::new();
    baseline.insert("tags".to_owned(), json!(normalized_tags(&document.tags)));
    baseline.insert("content".to_owned(), json!(fingerprint(&document.content)));
    baseline.insert("title".to_owned(), json!(fingerprint(&document.title)));
    baseline.insert("correspondent".to_owned(), json!(document.correspondent));
    baseline.insert("document_type".to_owned(), json!(document.document_type));
    baseline.insert("created".to_owned(), json!(fingerprint(&document.created)));
    baseline.insert(
        "custom_fields".to_owned(),
        json!(fingerprint_custom_fields(&document.custom_fields)),
    );
    Value::Object(baseline)
}

/// Resolve a review patch against the latest Paperless document. Scalar
/// fields use optimistic concurrency; tags use a three-way set merge so
/// unrelated additions/removals made in Paperless are preserved.
pub fn resolve_review_patch(
    baseline: &Value,
    mut desired: DocumentPatch,
    current: &PaperlessDocumentDetail,
    tag_operations: &ReviewTagOperations,
) -> std::result::Result<DocumentPatch, ReviewApplyConflict> {
    let mut conflicts = Vec::new();

    check_fingerprinted_field(
        baseline,
        "content",
        desired.content.as_ref(),
        &current.content,
        &mut conflicts,
    );
    check_fingerprinted_field(
        baseline,
        "title",
        desired.title.as_ref(),
        &current.title,
        &mut conflicts,
    );
    check_plain_field(
        baseline,
        "correspondent",
        desired.correspondent.as_ref(),
        &current.correspondent,
        &mut conflicts,
    );
    check_plain_field(
        baseline,
        "document_type",
        desired.document_type.as_ref(),
        &current.document_type,
        &mut conflicts,
    );
    check_fingerprinted_field(
        baseline,
        "created",
        desired.created.as_ref(),
        &current.created,
        &mut conflicts,
    );
    check_custom_fields(
        baseline,
        desired.custom_fields.as_ref(),
        &current.custom_fields,
        &mut conflicts,
    );

    let baseline_tags = baseline
        .get("tags")
        .and_then(|value| serde_json::from_value::<Vec<i32>>(value.clone()).ok());
    if baseline_tags.is_none() {
        conflicts.push("tags".to_owned());
    }

    conflicts.sort();
    conflicts.dedup();
    if !conflicts.is_empty() {
        return Err(ReviewApplyConflict { fields: conflicts });
    }

    if desired.content.as_ref() == current.content.as_ref() {
        desired.content = None;
    }
    if desired.title.as_ref() == current.title.as_ref() {
        desired.title = None;
    }
    if desired
        .correspondent
        .as_ref()
        .is_some_and(|value| value == &current.correspondent)
    {
        desired.correspondent = None;
    }
    if desired
        .document_type
        .as_ref()
        .is_some_and(|value| value == &current.document_type)
    {
        desired.document_type = None;
    }
    if desired.created.as_ref() == current.created.as_ref() {
        desired.created = None;
    }
    if desired.custom_fields.as_ref().is_some_and(|value| {
        canonical_custom_fields(value) == canonical_custom_fields(&current.custom_fields)
    }) {
        desired.custom_fields = None;
    }

    let baseline_tags = BTreeSet::from_iter(baseline_tags.unwrap_or_default());
    let desired_tags = desired
        .tags
        .as_ref()
        .map(|tags| BTreeSet::from_iter(tags.iter().copied()))
        .unwrap_or_else(|| baseline_tags.clone());
    let review_additions = desired_tags
        .difference(&baseline_tags)
        .copied()
        .collect::<BTreeSet<_>>();
    let review_removals = baseline_tags
        .difference(&desired_tags)
        .copied()
        .collect::<BTreeSet<_>>();
    let mut merged = BTreeSet::from_iter(current.tags.iter().copied());
    merged.extend(review_additions);
    merged.extend(tag_operations.additions.iter().copied());
    for removed in review_removals.iter().chain(tag_operations.removals.iter()) {
        merged.remove(removed);
    }
    let merged = merged.into_iter().collect::<Vec<_>>();
    desired.tags = (merged != normalized_tags(&current.tags)).then_some(merged);
    Ok(desired)
}

fn check_fingerprinted_field(
    baseline: &Value,
    field: &str,
    desired: Option<&String>,
    current: &Option<String>,
    conflicts: &mut Vec<String>,
) {
    let Some(desired) = desired else {
        return;
    };
    if current.as_ref() == Some(desired) {
        return;
    }
    let matches_baseline = baseline
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|expected| expected == fingerprint(current));
    if !matches_baseline {
        conflicts.push(field.to_owned());
    }
}

fn check_plain_field<T: Serialize + PartialEq>(
    baseline: &Value,
    field: &str,
    desired: Option<&T>,
    current: &T,
    conflicts: &mut Vec<String>,
) {
    let Some(desired) = desired else {
        return;
    };
    if desired == current {
        return;
    }
    let matches_baseline = baseline
        .get(field)
        .is_some_and(|expected| *expected == serde_json::to_value(current).unwrap_or(Value::Null));
    if !matches_baseline {
        conflicts.push(field.to_owned());
    }
}

fn check_custom_fields(
    baseline: &Value,
    desired: Option<&Value>,
    current: &Value,
    conflicts: &mut Vec<String>,
) {
    let Some(desired) = desired else {
        return;
    };
    if canonical_custom_fields(desired) == canonical_custom_fields(current) {
        return;
    }
    let matches_baseline = baseline
        .get("custom_fields")
        .and_then(Value::as_str)
        .is_some_and(|expected| expected == fingerprint_custom_fields(current));
    if !matches_baseline {
        conflicts.push("custom_fields".to_owned());
    }
}

fn normalized_tags(tags: &[i32]) -> Vec<i32> {
    BTreeSet::from_iter(tags.iter().copied())
        .into_iter()
        .collect()
}

fn fingerprint<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).expect("serializing a baseline value cannot fail");
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn fingerprint_custom_fields(value: &Value) -> String {
    fingerprint(&canonical_custom_fields(value))
}

fn canonical_custom_fields(value: &Value) -> Value {
    let mut canonical = canonical_json(value);
    if let Value::Array(items) = &mut canonical
        && items
            .iter()
            .all(|item| item.get("field").is_some() || item.get("id").is_some())
    {
        items.sort_by_key(|item| {
            item.get("field")
                .or_else(|| item.get("id"))
                .map(Value::to_string)
                .unwrap_or_default()
        });
    }
    canonical
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(Map::from_iter(sorted))
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
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
    if let Some(precondition) = request.review_precondition.take() {
        let current = client
            .get_document(request.paperless_document_id)
            .await
            .context("read latest Paperless document for review concurrency check")?;
        request.patch = resolve_review_patch(
            &precondition.baseline,
            request.patch,
            &current,
            &precondition.tag_operations,
        )?;
        request.before = Some(audit_before_for_patch(&current, &request.patch));
    }
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
        review_precondition: None,
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

fn audit_before_for_patch(document: &PaperlessDocumentDetail, patch: &DocumentPatch) -> Value {
    let mut object = Map::new();
    if patch.content.is_some() {
        let content = document.content.as_deref().unwrap_or_default();
        object.insert(
            "content".to_owned(),
            json!({
                "sha256": hex::encode(Sha256::digest(content.as_bytes())),
                "chars": content.chars().count(),
                "redacted": true
            }),
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
    Value::Object(object)
}
