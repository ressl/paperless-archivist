//! Review items, decisions, retries and apply intents.

use super::*;

/// Expected outcomes of a review decision that are not server faults
/// (double click, two reviewers racing, stale UI). #391
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum ReviewDecisionError {
    #[error("review item does not exist")]
    NotFound,
    #[error("review item is not pending")]
    NotPending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItemRecord {
    pub id: Uuid,
    /// None once the originating run has been pruned by the runs retention
    /// (review_items.run_id is ON DELETE SET NULL since migration 0041).
    /// Always present while the run is alive — retention only deletes
    /// terminal runs.
    pub run_id: Option<Uuid>,
    pub job_id: Option<Uuid>,
    pub paperless_document_id: i32,
    pub stage: Stage,
    pub status: String,
    pub suggested_patch: Value,
    pub edited_patch: Option<Value>,
    #[serde(skip_serializing)]
    pub baseline: Value,
    pub conflict_fields: Value,
    pub conflicted_at: Option<DateTime<Utc>>,
    pub validation_warnings: Value,
    pub debug_context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paperless_title: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ApplyIntentInput {
    pub source: String,
    pub source_key: String,
    pub owner_type: String,
    pub owner_id: String,
    pub paperless_document_id: i32,
    pub run_id: Option<Uuid>,
    pub job_id: Option<Uuid>,
    pub review_id: Option<Uuid>,
    pub patch_hash: String,
    pub patch: Value,
    pub before: Option<Value>,
    pub metadata: Value,
    pub review_revert_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyIntentRecord {
    pub attempt_id: Uuid,
    pub source: String,
    pub source_key: String,
    pub owner_type: String,
    pub owner_id: String,
    pub paperless_document_id: i32,
    pub run_id: Option<Uuid>,
    pub job_id: Option<Uuid>,
    pub review_id: Option<Uuid>,
    pub patch_hash: String,
    pub patch: Value,
    pub before: Option<Value>,
    pub response: Option<Value>,
    pub metadata: Value,
    pub review_revert_status: Option<String>,
    pub state: String,
    pub last_error: Option<String>,
    pub request_started_at: Option<DateTime<Utc>>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub finalized_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Insert a review item for `job` and flip job/run/inventory to
/// `waiting_review`. Like `complete_job`/`fail_job`, the job update is fenced
/// on `lease_owner`: a worker whose lease was reclaimed gets `Ok(None)` and
/// must stop instead of inserting review items for a job another replica now
/// owns. A job already in `waiting_review` still matches for the same owner,
/// so one stage can create several per-field items back to back.
pub async fn create_review_item(
    pool: &DbPool,
    job: &JobRecord,
    suggested_patch: Value,
    validation_warnings: Value,
    baseline: Value,
    lease_owner: &str,
) -> Result<Option<Uuid>> {
    let mut tx = pool.begin().await?;
    let fenced = sqlx::query(
        r#"
        update jobs
           set status = 'waiting_review', updated_at = now()
         where id = $1
           and lease_owner = $2
           and status in ('running', 'waiting_review')
        "#,
    )
    .bind(job.id)
    .bind(lease_owner)
    .execute(&mut *tx)
    .await?;
    if fenced.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(None);
    }

    let id: Uuid = sqlx::query(
        r#"
        insert into review_items (
          run_id, job_id, paperless_document_id, stage, status,
          suggested_patch, validation_warnings, baseline
        )
        values ($1, $2, $3, $4, 'pending', $5, $6, $7)
        returning id
        "#,
    )
    .bind(job.run_id)
    .bind(job.id)
    .bind(job.paperless_document_id)
    .bind(job.stage.to_string())
    .bind(&suggested_patch)
    .bind(&validation_warnings)
    .bind(&baseline)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;

    transition_run_tx(&mut tx, job.run_id, RunTransition::AwaitingReview, None).await?;
    set_inventory_stage_status_tx(
        &mut tx,
        job.paperless_document_id,
        job.stage,
        "waiting_review",
        None,
        true,
        Some(job.run_id),
    )
    .await?;
    // #439: the badge used to stay on `running` while the run waited for a
    // decision, breaking the #303 mirror invariant.
    mirror_run_status_tx(&mut tx, &[job.run_id], None).await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "review.created".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: None,
            after: Some(json!({ "review_id": id, "stage": job.stage })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Some(id))
}

pub async fn list_reviews(
    pool: &DbPool,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<ReviewItemRecord>> {
    let rows = if let Some(status) = status {
        sqlx::query(
            r#"
            select ri.id, ri.run_id, ri.job_id, ri.paperless_document_id, ri.stage, ri.status,
                   ri.suggested_patch, ri.edited_patch, ri.baseline,
                   ri.conflict_fields, ri.conflicted_at,
                   ri.validation_warnings, ri.created_at,
                   di.title as paperless_title,
                   jsonb_build_object(
                     'detected_language', di.detected_language,
                     'detected_language_confidence', di.detected_language_confidence,
                     'detected_language_source', di.detected_language_source,
                     'current_run_status', di.current_run_status,
                     'last_error', di.last_error,
                     'next_required_stage', di.next_required_stage
                   ) as debug_context
              from review_items ri
              left join document_inventory di
                on di.paperless_document_id = ri.paperless_document_id
             where ri.status = $1
             order by ri.created_at desc
             limit $2
            "#,
        )
        .bind(status)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            r#"
            select ri.id, ri.run_id, ri.job_id, ri.paperless_document_id, ri.stage, ri.status,
                   ri.suggested_patch, ri.edited_patch, ri.baseline,
                   ri.conflict_fields, ri.conflicted_at,
                   ri.validation_warnings, ri.created_at,
                   di.title as paperless_title,
                   jsonb_build_object(
                     'detected_language', di.detected_language,
                     'detected_language_confidence', di.detected_language_confidence,
                     'detected_language_source', di.detected_language_source,
                     'current_run_status', di.current_run_status,
                     'last_error', di.last_error,
                     'next_required_stage', di.next_required_stage
                   ) as debug_context
              from review_items ri
              left join document_inventory di
                on di.paperless_document_id = ri.paperless_document_id
             order by ri.created_at desc
             limit $1
            "#,
        )
        .bind(limit)
        .fetch_all(pool)
        .await?
    };

    rows.into_iter().map(review_list_item_from_row).collect()
}

fn review_list_item_from_row(row: PgRow) -> Result<ReviewItemRecord> {
    let stage: String = row.try_get("stage")?;
    Ok(ReviewItemRecord {
        id: row.try_get("id")?,
        run_id: row.try_get("run_id")?,
        job_id: row.try_get("job_id")?,
        paperless_document_id: row.try_get("paperless_document_id")?,
        stage: stage.parse()?,
        status: row.try_get("status")?,
        suggested_patch: row.try_get("suggested_patch")?,
        edited_patch: row.try_get("edited_patch")?,
        baseline: row.try_get("baseline")?,
        conflict_fields: row.try_get("conflict_fields")?,
        conflicted_at: row.try_get("conflicted_at")?,
        validation_warnings: row.try_get("validation_warnings")?,
        debug_context: row.try_get("debug_context")?,
        paperless_title: row.try_get("paperless_title").ok(),
        created_at: row.try_get("created_at")?,
    })
}

/// Load one review by ID only while it is still `pending`, independent of
/// how deep it sits in the backlog. #395
pub async fn get_pending_review(
    pool: &DbPool,
    review_id: Uuid,
) -> Result<Option<ReviewItemRecord>> {
    let row = sqlx::query(
        r#"
        select ri.id, ri.run_id, ri.job_id, ri.paperless_document_id, ri.stage, ri.status,
               ri.suggested_patch, ri.edited_patch, ri.baseline,
               ri.conflict_fields, ri.conflicted_at,
               ri.validation_warnings, ri.created_at,
               di.title as paperless_title,
               null::jsonb as debug_context
          from review_items ri
          left join document_inventory di
            on di.paperless_document_id = ri.paperless_document_id
         where ri.id = $1 and ri.status = 'pending'
        "#,
    )
    .bind(review_id)
    .fetch_optional(pool)
    .await?;
    row.map(review_list_item_from_row).transpose()
}

/// Count review items matching the same optional `status` filter used by
/// [`list_reviews`]. Lets the API report an honest total alongside a clamped page.
pub async fn count_reviews(pool: &DbPool, status: Option<&str>) -> Result<i64> {
    let count = if let Some(status) = status {
        sqlx::query_scalar::<_, i64>(r#"select count(*) from review_items where status = $1"#)
            .bind(status)
            .fetch_one(pool)
            .await?
    } else {
        sqlx::query_scalar::<_, i64>(r#"select count(*) from review_items"#)
            .fetch_one(pool)
            .await?
    };
    Ok(count)
}

/// Paperless document id a review item belongs to (any status). The preview
/// proxy is keyed by review id so only documents that are in the review queue
/// can be fetched through Archivist's Paperless token. #445
pub async fn review_document_id(pool: &DbPool, review_id: Uuid) -> Result<Option<i32>> {
    Ok(
        sqlx::query_scalar("select paperless_document_id from review_items where id = $1")
            .bind(review_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Result of [`retry_review_with_overrides`]. The non-`Queued` variants are
/// expected client-visible states, reported without side effects. #445
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewRetryOutcome {
    Queued {
        run_id: Uuid,
        rejected_review_ids: Vec<Uuid>,
    },
    /// Only metadata-stage reviews can be regenerated with another model/prompt.
    UnsupportedStage,
    /// A sibling review of the same job is approved/edited/applying; the
    /// original run cannot be closed without racing that apply.
    SiblingInFlight,
    /// Closing the reviews left an active run for the document (e.g. a later
    /// stage still queued), so a fresh run cannot be created.
    ActiveRun,
}

/// "Retry with ...": reject every still-pending review of the same job and
/// queue a new metadata-only run whose job payload carries `overrides`
/// (see `archivist_core::MetadataRetryOverrides`). Everything happens in one
/// transaction, so a conflict leaves the reviews untouched. #445
pub async fn retry_review_with_overrides(
    pool: &DbPool,
    review_id: Uuid,
    actor_id: Uuid,
    overrides: &Value,
) -> Result<ReviewRetryOutcome> {
    let mut tx = pool.begin().await?;
    let Some(review) = sqlx::query(
        "select job_id, paperless_document_id, stage, status from review_items where id = $1",
    )
    .bind(review_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Err(ReviewDecisionError::NotFound.into());
    };
    let status: String = review.try_get("status")?;
    if status != "pending" {
        tx.rollback().await?;
        return Err(ReviewDecisionError::NotPending.into());
    }
    let stage: String = review.try_get("stage")?;
    if stage != Stage::Metadata.to_string() {
        tx.rollback().await?;
        return Ok(ReviewRetryOutcome::UnsupportedStage);
    }
    let document_id: i32 = review.try_get("paperless_document_id")?;
    let job_id: Option<Uuid> = review.try_get("job_id")?;

    // Same lock order as run creation: the document lock first, audit last.
    lock_active_run_document_tx(&mut tx, document_id).await?;

    // Siblings share the job; a job-less (legacy) review is its own aggregate.
    let siblings =
        sqlx::query("select id, status from review_items where job_id = $1 or id = $2 for update")
            .bind(job_id)
            .bind(review_id)
            .fetch_all(&mut *tx)
            .await?;
    let mut pending = Vec::new();
    for sibling in &siblings {
        let id: Uuid = sibling.try_get("id")?;
        let sibling_status: String = sibling.try_get("status")?;
        match sibling_status.as_str() {
            "pending" => pending.push(id),
            "rejected" | "applied" => {}
            _ => {
                tx.rollback().await?;
                return Ok(ReviewRetryOutcome::SiblingInFlight);
            }
        }
    }
    // The row lock above may have waited on a concurrent decision.
    if !pending.contains(&review_id) {
        tx.rollback().await?;
        return Err(ReviewDecisionError::NotPending.into());
    }

    let rejected = sqlx::query(
        r#"
        update review_items
           set status = 'rejected',
               reviewed_by = $2,
               reviewed_at = now(),
               conflict_fields = '[]'::jsonb,
               conflicted_at = null
         where id = any($1) and status = 'pending'
        returning id, run_id, suggested_patch
        "#,
    )
    .bind(&pending)
    .bind(actor_id)
    .fetch_all(&mut *tx)
    .await?;
    if let Some(job_id) = job_id {
        finalize_review_aggregate_tx(
            &mut tx,
            job_id,
            review_id,
            "user",
            Some(actor_id.to_string()),
        )
        .await?;
    }

    let prepared = prepare_run_with_jobs_on_tx(
        &mut tx,
        document_id,
        &[Stage::Metadata],
        ProcessingMode::ManualReview,
        "review-retry",
        "user",
        Some(0),
    )
    .await?;
    let Some(run_created) = prepared.audit_event else {
        // Reused an existing active run: never attach overrides to it.
        tx.rollback().await?;
        return Ok(ReviewRetryOutcome::ActiveRun);
    };
    sqlx::query(
        r#"
        update jobs
           set payload = payload || jsonb_build_object($2::text, $3::jsonb)
         where run_id = $1 and stage = 'metadata'
        "#,
    )
    .bind(prepared.run_id)
    .bind(archivist_core::METADATA_RETRY_OVERRIDES_KEY)
    .bind(overrides)
    .execute(&mut *tx)
    .await?;

    let mut rejected_review_ids = Vec::with_capacity(rejected.len());
    for row in rejected {
        let id: Uuid = row.try_get("id")?;
        rejected_review_ids.push(id);
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: "review.rejected".to_owned(),
                actor_type: "user".to_owned(),
                actor_id: Some(actor_id.to_string()),
                run_id: row.try_get("run_id")?,
                job_id,
                paperless_document_id: Some(document_id),
                before: Some(row.try_get("suggested_patch")?),
                after: None,
                metadata: Some(json!({
                    "review_id": id,
                    "stage": stage,
                    "reason": "retry",
                    "retry_run_id": prepared.run_id
                })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }
    append_audit_tx(&mut tx, run_created).await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "review.retried".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: Some(prepared.run_id),
            job_id: None,
            paperless_document_id: Some(document_id),
            before: None,
            after: None,
            metadata: Some(json!({
                "review_id": review_id,
                "rejected_reviews": rejected_review_ids.len(),
                "overrides": overrides
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(ReviewRetryOutcome::Queued {
        run_id: prepared.run_id,
        rejected_review_ids,
    })
}

pub async fn review_decision(
    pool: &DbPool,
    review_id: Uuid,
    status: &str,
    edited_patch: Option<Value>,
    actor_id: Uuid,
) -> Result<()> {
    // #439: legal decision targets come from the review transition table.
    if ReviewStatus::parse(status)
        .and_then(|to| ReviewTransition::Decided(to).to())
        .is_none()
    {
        return Err(anyhow!("invalid review decision status"));
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        update review_items
           set status = $2,
               edited_patch = coalesce($3, edited_patch),
               reviewed_by = $4,
               reviewed_at = now(),
               conflict_fields = '[]'::jsonb,
               conflicted_at = null
         where id = $1 and status = 'pending'
        returning run_id, job_id, paperless_document_id, stage, suggested_patch, edited_patch
        "#,
    )
    .bind(review_id)
    .bind(status)
    .bind(&edited_patch)
    .bind(actor_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        let exists: bool =
            sqlx::query_scalar("select exists (select 1 from review_items where id = $1)")
                .bind(review_id)
                .fetch_one(pool)
                .await?;
        return Err(if exists {
            ReviewDecisionError::NotPending
        } else {
            ReviewDecisionError::NotFound
        }
        .into());
    };

    // None only when the originating run was pruned by retention (terminal
    // runs only) — impossible for a still-pending review in practice, but
    // decoded defensively since migration 0041 made the column nullable.
    let run_id: Option<Uuid> = row.try_get("run_id")?;
    let job_id: Option<Uuid> = row.try_get("job_id")?;
    let document_id: i32 = row.try_get("paperless_document_id")?;
    let stage_text: String = row.try_get("stage")?;
    let suggested_patch: Value = row.try_get("suggested_patch")?;
    let stored_edited_patch: Option<Value> = row.try_get("edited_patch")?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: format!("review.{status}"),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id,
            job_id,
            paperless_document_id: Some(document_id),
            before: Some(suggested_patch),
            after: edited_patch.or(stored_edited_patch),
            metadata: Some(json!({ "review_id": review_id, "stage": stage_text })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    if status == "rejected"
        && let Some(job_id) = job_id
    {
        finalize_review_aggregate_tx(
            &mut tx,
            job_id,
            review_id,
            "user",
            Some(actor_id.to_string()),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn apply_intent_from_row(row: PgRow) -> Result<ApplyIntentRecord> {
    Ok(ApplyIntentRecord {
        attempt_id: row.try_get("attempt_id")?,
        source: row.try_get("source")?,
        source_key: row.try_get("source_key")?,
        owner_type: row.try_get("owner_type")?,
        owner_id: row.try_get("owner_id")?,
        paperless_document_id: row.try_get("paperless_document_id")?,
        run_id: row.try_get("run_id")?,
        job_id: row.try_get("job_id")?,
        review_id: row.try_get("review_id")?,
        patch_hash: row.try_get("patch_hash")?,
        patch: row.try_get("patch")?,
        before: row.try_get("before_state")?,
        response: row.try_get("response_state")?,
        metadata: row.try_get("metadata")?,
        review_revert_status: row.try_get("review_revert_status")?,
        state: row.try_get("state")?,
        last_error: row.try_get("last_error")?,
        request_started_at: row.try_get("request_started_at")?,
        confirmed_at: row.try_get("confirmed_at")?,
        finalized_at: row.try_get("finalized_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

const APPLY_INTENT_COLUMNS: &str = r#"
    attempt_id, source, source_key, owner_type, owner_id,
    paperless_document_id, run_id, job_id, review_id, patch_hash, patch,
    before_state, response_state, metadata, review_revert_status, state,
    last_error, request_started_at, confirmed_at, finalized_at,
    created_at, updated_at
"#;

fn apply_audit_patch(patch: &Value) -> Value {
    let Some(source) = patch.as_object() else {
        return json!({ "redacted": true, "sha256": short_hash(&patch.to_string()) });
    };
    let mut audit = serde_json::Map::new();
    for (key, value) in source {
        match key.as_str() {
            "content" => {
                let text = value.as_str().unwrap_or_default();
                audit.insert(
                    key.clone(),
                    json!({
                        "redacted": true,
                        "sha256": short_hash(text),
                        "chars": text.chars().count()
                    }),
                );
            }
            "custom_fields" => {
                audit.insert(
                    key.clone(),
                    json!({
                        "redacted": true,
                        "sha256": short_hash(&value.to_string())
                    }),
                );
            }
            _ => {
                audit.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(audit)
}

/// Persist the immutable intent for one logical Paperless PATCH. Repeating the
/// same source-key/hash pair returns its original attempt ID and state.
pub async fn prepare_apply_intent(
    pool: &DbPool,
    input: &ApplyIntentInput,
) -> Result<ApplyIntentRecord> {
    if input.source.trim().is_empty()
        || input.source_key.trim().is_empty()
        || input.owner_id.trim().is_empty()
        || input.patch_hash.trim().is_empty()
        || !matches!(input.owner_type.as_str(), "user" | "worker")
        || input
            .review_revert_status
            .as_deref()
            .is_some_and(|status| !matches!(status, "pending" | "approved" | "edited"))
    {
        return Err(anyhow!("invalid Paperless apply intent"));
    }

    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        insert into paperless_apply_intents (
          source, source_key, owner_type, owner_id, paperless_document_id,
          run_id, job_id, review_id, patch_hash, patch, before_state,
          metadata, review_revert_status
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        on conflict (source_key, patch_hash) do nothing
        returning {APPLY_INTENT_COLUMNS}
        "#
    )))
    .bind(&input.source)
    .bind(&input.source_key)
    .bind(&input.owner_type)
    .bind(&input.owner_id)
    .bind(input.paperless_document_id)
    .bind(input.run_id)
    .bind(input.job_id)
    .bind(input.review_id)
    .bind(&input.patch_hash)
    .bind(&input.patch)
    .bind(&input.before)
    .bind(&input.metadata)
    .bind(&input.review_revert_status)
    .fetch_optional(&mut *tx)
    .await?;

    let (record, was_inserted) = if let Some(row) = inserted {
        (apply_intent_from_row(row)?, true)
    } else {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {APPLY_INTENT_COLUMNS} from paperless_apply_intents where source_key = $1 and patch_hash = $2"
        )))
        .bind(&input.source_key)
        .bind(&input.patch_hash)
        .fetch_one(&mut *tx)
        .await?;
        let record = apply_intent_from_row(row)?;
        if record.paperless_document_id != input.paperless_document_id
            || record.patch != input.patch
        {
            tx.rollback().await?;
            return Err(anyhow!("Paperless apply intent hash collision"));
        }
        if record.state == "failed" && record.finalized_at.is_some() {
            // A failed attempt proved (by 4xx or a post-error GET) that the
            // PATCH did not take effect, and finalization means its review was
            // already returned to a decidable state. A new decision for the
            // same patch may therefore start a fresh attempt instead of being
            // blocked forever by "already failed". #389
            let row = sqlx::query(sqlx::AssertSqlSafe(format!(
                r#"
                update paperless_apply_intents
                   set state = 'prepared', owner_type = $2, owner_id = $3,
                       run_id = $4, job_id = $5, before_state = $6, metadata = $7,
                       review_revert_status = $8, response_state = null,
                       request_started_at = null, confirmed_at = null,
                       finalized_at = null, updated_at = now()
                 where attempt_id = $1 and state = 'failed' and finalized_at is not null
                returning {APPLY_INTENT_COLUMNS}
                "#
            )))
            .bind(record.attempt_id)
            .bind(&input.owner_type)
            .bind(&input.owner_id)
            .bind(input.run_id)
            .bind(input.job_id)
            .bind(&input.before)
            .bind(&input.metadata)
            .bind(&input.review_revert_status)
            .fetch_optional(&mut *tx)
            .await?;
            match row {
                Some(row) => (apply_intent_from_row(row)?, true),
                None => (record, false),
            }
        } else {
            (record, false)
        }
    };

    if was_inserted {
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: "document.patch_intent".to_owned(),
                actor_type: record.owner_type.clone(),
                actor_id: Some(record.owner_id.clone()),
                run_id: record.run_id,
                job_id: record.job_id,
                paperless_document_id: Some(record.paperless_document_id),
                before: record.before.clone(),
                after: Some(json!({
                    "attempt_id": record.attempt_id,
                    "patch_hash": record.patch_hash,
                    "source": record.source,
                    "state": "prepared"
                })),
                metadata: Some(json!({
                    "source_key": record.source_key,
                    "context": record.metadata
                })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(record)
}

pub async fn get_apply_intent(
    pool: &DbPool,
    attempt_id: Uuid,
) -> Result<Option<ApplyIntentRecord>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "select {APPLY_INTENT_COLUMNS} from paperless_apply_intents where attempt_id = $1"
    )))
    .bind(attempt_id)
    .fetch_optional(pool)
    .await?;
    row.map(apply_intent_from_row).transpose()
}

/// Return the durable unfinished attempt for a logical caller. This lets a
/// reacquired job lease resume the original body even when a fresh Paperless
/// read would prune fields that were already applied.
pub async fn get_recoverable_apply_intent_by_source_key(
    pool: &DbPool,
    source_key: &str,
) -> Result<Option<ApplyIntentRecord>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        select {APPLY_INTENT_COLUMNS}
          from paperless_apply_intents
         where source_key = $1
           and state in ('prepared', 'in_flight', 'confirmed', 'reconciled')
           and finalized_at is null
         order by created_at desc
         limit 1
        "#
    )))
    .bind(source_key)
    .fetch_optional(pool)
    .await?;
    row.map(apply_intent_from_row).transpose()
}

/// Claim a prepared attempt immediately before HTTP. A recovery worker may
/// take over a prepared intent because no request has started yet.
pub async fn mark_apply_intent_in_flight(
    pool: &DbPool,
    attempt_id: Uuid,
    owner_id: &str,
) -> Result<bool> {
    let updated = sqlx::query(
        r#"
        update paperless_apply_intents
           set state = 'in_flight', owner_id = $2,
               request_started_at = now(), updated_at = now()
         where attempt_id = $1 and state = 'prepared'
        "#,
    )
    .bind(attempt_id)
    .bind(owner_id)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

pub async fn mark_apply_intent_confirmed(
    pool: &DbPool,
    attempt_id: Uuid,
    owner_id: &str,
    response: Option<Value>,
    duration_ms: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        update paperless_apply_intents
           set state = 'confirmed', response_state = $3,
               confirmed_at = now(), updated_at = now(), last_error = null
         where attempt_id = $1 and owner_id = $2 and state = 'in_flight'
        returning source, source_key, owner_type, owner_id, run_id, job_id,
                  review_id, paperless_document_id, patch_hash, patch,
                  before_state, metadata
        "#,
    )
    .bind(attempt_id)
    .bind(owner_id)
    .bind(&response)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(false);
    };
    let run_id: Option<Uuid> = row.try_get("run_id")?;
    let job_id: Option<Uuid> = row.try_get("job_id")?;
    let document_id: i32 = row.try_get("paperless_document_id")?;
    let patch: Value = row.try_get("patch")?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "document.patch_confirmed".to_owned(),
            actor_type: row.try_get("owner_type")?,
            actor_id: Some(row.try_get("owner_id")?),
            run_id,
            job_id,
            paperless_document_id: Some(document_id),
            before: row.try_get("before_state")?,
            after: Some(apply_audit_patch(&patch)),
            metadata: Some(json!({
                "attempt_id": attempt_id,
                "patch_hash": row.try_get::<String, _>("patch_hash")?,
                "source": row.try_get::<String, _>("source")?,
                "source_key": row.try_get::<String, _>("source_key")?,
                "duration_ms": duration_ms,
                "context": row.try_get::<Value, _>("metadata")?
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    increment_metric_counter_tx(&mut tx, "apply_success_total", 1).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn reconcile_apply_intent(
    pool: &DbPool,
    attempt_id: Uuid,
    reconciled_by: &str,
    response: Option<Value>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        update paperless_apply_intents
           set state = 'reconciled', owner_id = $2, response_state = $3,
               confirmed_at = now(), updated_at = now(), last_error = null
         where attempt_id = $1 and state = 'in_flight'
        returning source, source_key, owner_type, owner_id, run_id, job_id,
                  paperless_document_id, patch_hash, patch, before_state, metadata
        "#,
    )
    .bind(attempt_id)
    .bind(reconciled_by)
    .bind(&response)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(false);
    };
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "document.patch_reconciled".to_owned(),
            actor_type: row.try_get("owner_type")?,
            actor_id: Some(row.try_get("owner_id")?),
            run_id: row.try_get("run_id")?,
            job_id: row.try_get("job_id")?,
            paperless_document_id: Some(row.try_get("paperless_document_id")?),
            before: row.try_get("before_state")?,
            after: Some(apply_audit_patch(&row.try_get("patch")?)),
            metadata: Some(json!({
                "attempt_id": attempt_id,
                "patch_hash": row.try_get::<String, _>("patch_hash")?,
                "source": row.try_get::<String, _>("source")?,
                "source_key": row.try_get::<String, _>("source_key")?,
                "context": row.try_get::<Value, _>("metadata")?
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    increment_metric_counter_tx(&mut tx, "apply_success_total", 1).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn fail_apply_intent(
    pool: &DbPool,
    attempt_id: Uuid,
    failed_by: &str,
    error: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        update paperless_apply_intents
           set state = 'failed', owner_id = $2, last_error = $3, updated_at = now()
         where attempt_id = $1 and state in ('prepared', 'in_flight')
        returning source, source_key, owner_type, owner_id, run_id, job_id,
                  paperless_document_id, patch_hash, patch, before_state, metadata
        "#,
    )
    .bind(attempt_id)
    .bind(failed_by)
    .bind(error)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(false);
    };
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "document.patch_failed".to_owned(),
            actor_type: row.try_get("owner_type")?,
            actor_id: Some(row.try_get("owner_id")?),
            run_id: row.try_get("run_id")?,
            job_id: row.try_get("job_id")?,
            paperless_document_id: Some(row.try_get("paperless_document_id")?),
            before: row.try_get("before_state")?,
            after: Some(apply_audit_patch(&row.try_get("patch")?)),
            metadata: Some(json!({
                "attempt_id": attempt_id,
                "patch_hash": row.try_get::<String, _>("patch_hash")?,
                "source": row.try_get::<String, _>("source")?,
                "source_key": row.try_get::<String, _>("source_key")?,
                "context": row.try_get::<Value, _>("metadata")?
            })),
            outcome: "failed".to_owned(),
            error_message: Some(error.to_owned()),
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    increment_metric_counter_tx(&mut tx, "apply_failure_total", 1).await?;
    tx.commit().await?;
    Ok(true)
}

/// After a transient PATCH failure (5xx, network, timeout) whose follow-up GET
/// proved the document was not changed, return the attempt to `prepared` so
/// the owner (job retry or review recovery) may send the same body again
/// instead of the patch being blocked forever. Bounded by `max_retries`
/// (tracked in `metadata.transient_failures`); returns `false` once the budget
/// is exhausted, in which case the caller fails the intent terminally. #389
pub async fn release_transient_apply_intent(
    pool: &DbPool,
    attempt_id: Uuid,
    owner_id: &str,
    error: &str,
    max_retries: i32,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        update paperless_apply_intents
           set state = 'prepared', request_started_at = null, last_error = $3,
               metadata = jsonb_set(
                 metadata, '{transient_failures}',
                 to_jsonb(coalesce((metadata->>'transient_failures')::int, 0) + 1)
               ),
               updated_at = now()
         where attempt_id = $1 and owner_id = $2 and state = 'in_flight'
           and coalesce((metadata->>'transient_failures')::int, 0) < $4
        returning source, source_key, owner_type, owner_id, run_id, job_id,
                  paperless_document_id, patch_hash, patch, before_state, metadata
        "#,
    )
    .bind(attempt_id)
    .bind(owner_id)
    .bind(error)
    .bind(max_retries)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(false);
    };
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "document.patch_retry_scheduled".to_owned(),
            actor_type: row.try_get("owner_type")?,
            actor_id: Some(row.try_get("owner_id")?),
            run_id: row.try_get("run_id")?,
            job_id: row.try_get("job_id")?,
            paperless_document_id: Some(row.try_get("paperless_document_id")?),
            before: row.try_get("before_state")?,
            after: Some(apply_audit_patch(&row.try_get("patch")?)),
            metadata: Some(json!({
                "attempt_id": attempt_id,
                "patch_hash": row.try_get::<String, _>("patch_hash")?,
                "source": row.try_get::<String, _>("source")?,
                "source_key": row.try_get::<String, _>("source_key")?,
                "context": row.try_get::<Value, _>("metadata")?
            })),
            outcome: "failed".to_owned(),
            error_message: Some(error.to_owned()),
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    increment_metric_counter_tx(&mut tx, "apply_failure_total", 1).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn finalize_apply_intent(pool: &DbPool, attempt_id: Uuid) -> Result<bool> {
    let updated = sqlx::query(
        r#"
        update paperless_apply_intents
           set state = 'finalized', finalized_at = now(), updated_at = now()
         where attempt_id = $1 and state in ('confirmed', 'reconciled')
        "#,
    )
    .bind(attempt_id)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// Mark a failed intent as locally settled without making it look successful.
/// Keeping `state = 'failed'` ensures the same source key/hash cannot later be
/// mistaken for an already-applied request.
pub async fn finalize_failed_apply_intent(pool: &DbPool, attempt_id: Uuid) -> Result<bool> {
    let updated = sqlx::query(
        r#"
        update paperless_apply_intents
           set finalized_at = now(), updated_at = now()
         where attempt_id = $1 and state = 'failed' and finalized_at is null
        "#,
    )
    .bind(attempt_id)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// Settle every failed, not yet finalized intent of a review after the caller
/// already returned the review to a decidable status. A finalized failed
/// intent may be re-prepared by a later decision for the same patch. #389
pub async fn finalize_failed_review_apply_intents(pool: &DbPool, review_id: Uuid) -> Result<u64> {
    let updated = sqlx::query(
        r#"
        update paperless_apply_intents
           set finalized_at = now(), updated_at = now()
         where review_id = $1 and state = 'failed' and finalized_at is null
        "#,
    )
    .bind(review_id)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected())
}

/// Whether reverting an `applying` review would make an ambiguous Paperless
/// side effect blindly retryable. On lookup errors callers should fail closed
/// and leave the review in `applying` for the recovery worker.
pub async fn review_has_nonterminal_apply_intent(pool: &DbPool, review_id: Uuid) -> Result<bool> {
    sqlx::query_scalar(
        r#"
        select exists (
          select 1
            from paperless_apply_intents
           where review_id = $1
             and state in ('prepared', 'in_flight', 'confirmed', 'reconciled')
             and finalized_at is null
        )
        "#,
    )
    .bind(review_id)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

pub async fn get_review_status(pool: &DbPool, review_id: Uuid) -> Result<Option<String>> {
    sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

pub async fn list_recoverable_review_apply_intents(
    pool: &DbPool,
    limit: i64,
) -> Result<Vec<ApplyIntentRecord>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        select {APPLY_INTENT_COLUMNS}
          from paperless_apply_intents candidate
         where candidate.review_id is not null
           and candidate.state in ('prepared', 'in_flight', 'confirmed', 'reconciled', 'failed')
           and candidate.finalized_at is null
           and not exists (
             select 1
               from paperless_apply_intents newer
              where newer.review_id = candidate.review_id
                and newer.finalized_at is null
                and newer.created_at > candidate.created_at
           )
         order by candidate.created_at asc
         limit $1
        "#
    )))
    .bind(limit.clamp(1, 1000))
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(apply_intent_from_row).collect()
}

/// Atomically claim an approved/edited review item for application by moving
/// it to the intermediate `applying` status, returning the record with its
/// *prior* status (so a failed apply can revert precisely). Returns `None`
/// when no row is in an applyable status — i.e. another apply (a second
/// operator, or the autopilot drain) already owns it. This is the fence that
/// prevents a document being PATCHed to Paperless twice. #253.
pub async fn claim_review_for_apply(
    pool: &DbPool,
    review_id: Uuid,
) -> Result<Option<ReviewItemRecord>> {
    let row = sqlx::query(
        r#"
        with prev as (
          select status from review_items where id = $1
        )
        update review_items
           -- Re-stamp reviewed_at so the stale-applying recovery sweep (which
           -- keys its 300s timer off reviewed_at) measures from the claim, not
           -- the original approval — otherwise a slow apply could be reverted
           -- mid-flight and double-applied. #295
           set status = 'applying', reviewed_at = now()
         where id = $1 and status in ('approved', 'edited')
        returning id, run_id, job_id, paperless_document_id, stage,
                  (select status from prev) as status,
                  suggested_patch, edited_patch, baseline,
                  conflict_fields, conflicted_at, validation_warnings, created_at
        "#,
    )
    .bind(review_id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        let stage: String = row.try_get("stage")?;
        Ok(ReviewItemRecord {
            id: row.try_get("id")?,
            run_id: row.try_get("run_id")?,
            job_id: row.try_get("job_id")?,
            paperless_document_id: row.try_get("paperless_document_id")?,
            stage: stage.parse()?,
            status: row.try_get("status")?,
            suggested_patch: row.try_get("suggested_patch")?,
            edited_patch: row.try_get("edited_patch")?,
            baseline: row.try_get("baseline")?,
            conflict_fields: row.try_get("conflict_fields")?,
            conflicted_at: row.try_get("conflicted_at")?,
            validation_warnings: row.try_get("validation_warnings")?,
            debug_context: None,
            paperless_title: None,
            created_at: row.try_get("created_at")?,
        })
    })
    .transpose()
}

/// Finalize the shared job only after every sibling review is terminal.
///
/// The job row is the aggregate lock. Concurrent last decisions serialize on
/// it; after the first transaction commits, the waiter observes the complete
/// sibling set and performs the single conditional job transition.
async fn finalize_review_aggregate_tx(
    tx: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
    triggering_review_id: Uuid,
    actor_type: &str,
    actor_id: Option<String>,
) -> Result<bool> {
    let Some(job) = sqlx::query(
        r#"
        select run_id, paperless_document_id, stage, status
          from jobs
         where id = $1
         for update
        "#,
    )
    .bind(job_id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(false);
    };
    let job_status: String = job.try_get("status")?;
    if job_status != "waiting_review" {
        return Ok(false);
    }

    let counts = sqlx::query(
        r#"
        select count(*)::bigint as total,
               count(*) filter (where status in ('applied', 'rejected'))::bigint as terminal,
               count(*) filter (where status = 'applied')::bigint as applied,
               count(*) filter (where status = 'rejected')::bigint as rejected
          from review_items
         where job_id = $1
        "#,
    )
    .bind(job_id)
    .fetch_one(&mut **tx)
    .await?;
    let total: i64 = counts.try_get("total")?;
    let terminal: i64 = counts.try_get("terminal")?;
    let applied: i64 = counts.try_get("applied")?;
    let rejected: i64 = counts.try_get("rejected")?;
    if total == 0 || terminal != total {
        return Ok(false);
    }

    let run_id: Uuid = job.try_get("run_id")?;
    let document_id: i32 = job.try_get("paperless_document_id")?;
    let stage_text: String = job.try_get("stage")?;
    let stage: Stage = stage_text.parse()?;
    let aggregate_status = if applied > 0 {
        "succeeded"
    } else {
        "cancelled"
    };
    let updated = sqlx::query(
        r#"
        update jobs
           set status = $2,
               lease_owner = null,
               lease_until = null,
               updated_at = now()
         where id = $1 and status = 'waiting_review'
        "#,
    )
    .bind(job_id)
    .bind(aggregate_status)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(false);
    }

    if applied == 0 {
        set_inventory_stage_status_tx(
            tx,
            document_id,
            stage,
            "rejected",
            None,
            false,
            Some(run_id),
        )
        .await?;
        cancel_active_run_jobs_tx(tx, run_id, None).await?;
        transition_run_tx(tx, run_id, RunTransition::Rejected, None).await?;
        // `set_inventory_stage_status_tx` just pointed `last_run_id` at this
        // run, so the guarded mirror always reaches the row. #410/#414
        mirror_run_status_tx(tx, &[run_id], None).await?;
    } else {
        set_inventory_stage_status_tx(
            tx,
            document_id,
            stage,
            "succeeded",
            None,
            false,
            Some(run_id),
        )
        .await?;
        settle_run_after_stage_tx(tx, run_id).await?;
    }

    append_audit_tx(
        tx,
        AuditEventInput {
            event_type: "review.aggregate_finalized".to_owned(),
            actor_type: actor_type.to_owned(),
            actor_id,
            run_id: Some(run_id),
            job_id: Some(job_id),
            paperless_document_id: Some(document_id),
            before: Some(json!({ "status": "waiting_review" })),
            after: Some(json!({
                "status": aggregate_status,
                "total": total,
                "applied": applied,
                "rejected": rejected
            })),
            metadata: Some(json!({
                "stage": stage,
                "triggering_review_id": triggering_review_id
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    Ok(true)
}

pub async fn mark_review_applied(pool: &DbPool, review_id: Uuid, actor_id: Uuid) -> Result<()> {
    settle_review_applied(pool, review_id, Some(actor_id)).await
}

/// [`ReviewTransition::Applied`] for both apply owners: a human apply
/// (`actor_id = Some`, audited as the user) and the autopilot drain
/// (`None`, audited as the worker with `trigger = "autopilot_drain"`).
/// Gated on `applying`: the caller claimed the row before patching
/// Paperless, so the terminal transition is only valid from that owned
/// state; a missing row means another actor already finished it. #253, #439
async fn settle_review_applied(
    pool: &DbPool,
    review_id: Uuid,
    actor_id: Option<Uuid>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let Some(row) = sqlx::query(
        r#"
        update review_items
           set status = 'applied',
               reviewed_by = coalesce(reviewed_by, $2),
               reviewed_at = coalesce(reviewed_at, now()),
               conflict_fields = '[]'::jsonb,
               conflicted_at = null
         where id = $1 and status = 'applying'
        returning run_id, job_id, paperless_document_id, stage
        "#,
    )
    .bind(review_id)
    .bind(actor_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(());
    };

    let job_id: Option<Uuid> = row.try_get("job_id")?;
    let stage: Stage = row.try_get::<String, _>("stage")?.parse()?;
    let document_id: i32 = row.try_get("paperless_document_id")?;
    // None only when the originating run was pruned by retention (terminal
    // runs only) — decoded defensively since migration 0041 made the column
    // nullable; the run-progress block below is skipped without a run.
    let run_id: Option<Uuid> = row.try_get("run_id")?;
    let (actor_type, actor, metadata) = match actor_id {
        Some(actor_id) => (
            "user",
            Some(actor_id.to_string()),
            json!({ "stage": stage }),
        ),
        None => (
            "worker",
            None,
            json!({ "stage": stage, "trigger": "autopilot_drain" }),
        ),
    };
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "review.applied".to_owned(),
            actor_type: actor_type.to_owned(),
            actor_id: actor.clone(),
            run_id,
            job_id,
            paperless_document_id: Some(document_id),
            before: None,
            after: Some(json!({ "review_id": review_id })),
            metadata: Some(metadata),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    if let Some(job_id) = job_id {
        finalize_review_aggregate_tx(&mut tx, job_id, review_id, actor_type, actor).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// List `pending` review_items for the autopilot drain.
///
/// Oldest-first so the operator-visible backlog is whittled down from the
/// front (and any "stuck for hours" rows leave the dashboard first). The
/// returned shape is identical to [`pending_review_for_apply`] so the worker
/// drain can reuse the same apply path.
///
/// Items that failed validation are never drained (#403): the worker stores
/// hard `ValidationError`s (`{"LowConfidence": ...}`, `"EmptyOutput"`, ...)
/// in `validation_warnings`, and such a suggestion needs a human decision.
/// Free-text soft warnings (e.g. the dry-run notice or a date-format note on
/// an otherwise valid suggestion) do not block the drain. A non-array value
/// is treated as blocking because its shape cannot be classified.
pub async fn list_pending_review_items_for_autopilot_drain(
    pool: &DbPool,
    limit: i64,
) -> Result<Vec<ReviewItemRecord>> {
    let rows = sqlx::query(
        r#"
        select id, run_id, job_id, paperless_document_id, stage, status,
               suggested_patch, edited_patch, baseline,
               conflict_fields, conflicted_at, validation_warnings, created_at
          from review_items
         where status = 'pending'
           and (
             validation_warnings is null
             or jsonb_typeof(validation_warnings) = 'null'
             or (
               jsonb_typeof(validation_warnings) = 'array'
               and not exists (
                 select 1
                   from jsonb_array_elements(validation_warnings) as warning(value)
                  where jsonb_typeof(warning.value) = 'object'
                     or warning.value #>> '{}' in ('EmptyOutput', 'InvalidTitle')
               )
             )
           )
           -- A terminally failed Paperless attempt is left for a human
           -- decision; re-draining it every tick would loop forever on the
           -- same failure. #389
           and not exists (
             select 1
               from paperless_apply_intents pai
              where pai.review_id = review_items.id
                and pai.state = 'failed'
           )
         order by created_at asc
         limit $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: String = row.try_get("stage")?;
            Ok(ReviewItemRecord {
                id: row.try_get("id")?,
                run_id: row.try_get("run_id")?,
                job_id: row.try_get("job_id")?,
                paperless_document_id: row.try_get("paperless_document_id")?,
                stage: stage.parse()?,
                status: row.try_get("status")?,
                suggested_patch: row.try_get("suggested_patch")?,
                edited_patch: row.try_get("edited_patch")?,
                baseline: row.try_get("baseline")?,
                conflict_fields: row.try_get("conflict_fields")?,
                conflicted_at: row.try_get("conflicted_at")?,
                validation_warnings: row.try_get("validation_warnings")?,
                debug_context: None,
                paperless_title: None,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

/// For each document, the finish time of its most recent pipeline run when
/// that run is terminal (`succeeded`, `rejected`, `failed`, `cancelled`).
/// Documents whose latest run is still active, or that never had a run, are
/// absent from the map. The trigger poller uses this to skip documents whose
/// trigger tag survived a terminal run without any later Paperless change,
/// so a trigger tag that could not be removed (dry-run, Paperless error,
/// review rejected) no longer requeues the document every poll (#400).
pub async fn latest_terminal_run_finished_at(
    pool: &DbPool,
    paperless_document_ids: &[i32],
) -> Result<HashMap<i32, DateTime<Utc>>> {
    if paperless_document_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(concat!(
        r#"
        select paperless_document_id, finished_at
          from (
            select distinct on (paperless_document_id)
                   paperless_document_id, status,
                   coalesce(finished_at, updated_at) as finished_at
              from pipeline_runs
             where paperless_document_id = any($1)
             order by paperless_document_id, created_at desc, id desc
          ) latest
         where status in ("#,
        sql_terminal_run_statuses!(),
        r#")
        "#
    ))
    .bind(paperless_document_ids)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("paperless_document_id")?,
                row.try_get("finished_at")?,
            ))
        })
        .collect()
}

/// Tag ids that Archivist itself added to a document through a landed
/// Paperless apply (`patch.tags` minus the recorded `before_state.tags`),
/// excluding workflow tags. This is the "AI-managed" set that the
/// `replace_ai_managed` tag strategy may replace (#411). Intents pruned by
/// retention are forgotten, which errs on the side of keeping tags.
pub async fn ai_managed_tag_ids_for_document(
    pool: &DbPool,
    paperless_document_id: i32,
) -> Result<Vec<i32>> {
    let rows = sqlx::query(
        r#"
        select distinct (added.value)::integer as tag_id
          from paperless_apply_intents intent
          cross join lateral jsonb_array_elements(intent.patch -> 'tags') as added(value)
         where intent.paperless_document_id = $1
           and intent.state in ('confirmed', 'reconciled', 'finalized')
           and jsonb_typeof(intent.patch -> 'tags') = 'array'
           and jsonb_typeof(intent.before_state -> 'tags') = 'array'
           and jsonb_typeof(added.value) = 'number'
           and not (intent.before_state -> 'tags') @> jsonb_build_array(added.value)
           and not exists (
             select 1 from paperless_tags tag
              where tag.id = (added.value)::integer
                and tag.is_workflow
           )
         order by tag_id
        "#,
    )
    .bind(paperless_document_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get("tag_id").context("ai-managed tag id"))
        .collect()
}

/// `(id, name, is_workflow)` for the given tag ids from the local Paperless
/// tag mirror. Ids missing from the mirror are simply absent.
pub async fn tag_catalog_entries_for_ids(
    pool: &DbPool,
    ids: &[i32],
) -> Result<Vec<(i32, String, bool)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "select id, name, is_workflow from paperless_tags where id = any($1) order by id",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("id")?,
                row.try_get("name")?,
                row.try_get("is_workflow")?,
            ))
        })
        .collect()
}

/// Atomically claim a pending review item for autopilot drain.
///
/// Flips the row from `pending` → `approved` and stamps a `review.approved`
/// audit event with `actor_type = "worker"` and `trigger = "autopilot_drain"`
/// in the metadata so post-hoc analysis can distinguish these from
/// human-initiated approvals. Returns the claimed record when the transition
/// succeeded, or `Ok(None)` if the row was no longer pending (raced by a
/// human reviewer or another worker tick).
pub async fn claim_pending_review_for_autopilot_drain(
    pool: &DbPool,
    review_id: Uuid,
) -> Result<Option<ReviewItemRecord>> {
    let mut tx = pool.begin().await?;
    // Claim straight into `applying` (not `approved`): the drain is about to
    // PATCH Paperless, so the row must be in the owned state that blocks a
    // concurrent human apply from patching the same document. #253.
    let Some(row) = sqlx::query(
        r#"
        update review_items
           set status = 'applying',
               reviewed_at = now()
         where id = $1 and status = 'pending'
        returning id, run_id, job_id, paperless_document_id, stage, status,
                  suggested_patch, edited_patch, baseline,
                  conflict_fields, conflicted_at, validation_warnings, created_at
        "#,
    )
    .bind(review_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(None);
    };

    let stage_text: String = row.try_get("stage")?;
    let stage: Stage = stage_text.parse()?;
    let record = ReviewItemRecord {
        id: row.try_get("id")?,
        run_id: row.try_get("run_id")?,
        job_id: row.try_get("job_id")?,
        paperless_document_id: row.try_get("paperless_document_id")?,
        stage,
        status: row.try_get("status")?,
        suggested_patch: row.try_get("suggested_patch")?,
        edited_patch: row.try_get("edited_patch")?,
        baseline: row.try_get("baseline")?,
        conflict_fields: row.try_get("conflict_fields")?,
        conflicted_at: row.try_get("conflicted_at")?,
        validation_warnings: row.try_get("validation_warnings")?,
        debug_context: None,
        paperless_title: None,
        created_at: row.try_get("created_at")?,
    };

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "review.approved".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: record.run_id,
            job_id: record.job_id,
            paperless_document_id: Some(record.paperless_document_id),
            before: Some(record.suggested_patch.clone()),
            after: record.edited_patch.clone(),
            metadata: Some(json!({
                "review_id": record.id,
                "stage": stage_text,
                "trigger": "autopilot_drain"
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Some(record))
}

/// Mark a review_item as applied via the autopilot drain: the worker-actor
/// variant of [`mark_review_applied`] (same transition, `trigger =
/// "autopilot_drain"` audit metadata).
pub async fn mark_review_auto_applied(pool: &DbPool, review_id: Uuid) -> Result<()> {
    settle_review_applied(pool, review_id, None).await
}

/// Record a field-level optimistic-concurrency conflict without exposing the
/// document values. The review returns to a retryable state while its shared
/// job, run, and inventory deliberately remain `waiting_review`.
pub async fn mark_review_apply_conflict(
    pool: &DbPool,
    review_id: Uuid,
    to_status: &str,
    fields: &[String],
    actor_type: &str,
    actor_id: Option<String>,
) -> Result<bool> {
    let to = review_revert_target(to_status)?;
    let mut fields = fields.to_vec();
    fields.sort();
    fields.dedup();
    let mut tx = pool.begin().await?;
    let Some(row) =
        revert_review_from_applying_tx(&mut tx, review_id, to, Some(&json!(fields))).await?
    else {
        tx.rollback().await?;
        return Ok(false);
    };

    let run_id: Option<Uuid> = row.try_get("run_id")?;
    let job_id: Option<Uuid> = row.try_get("job_id")?;
    let document_id: i32 = row.try_get("paperless_document_id")?;
    let stage: String = row.try_get("stage")?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "review.apply_conflict".to_owned(),
            actor_type: actor_type.to_owned(),
            actor_id,
            run_id,
            job_id,
            paperless_document_id: Some(document_id),
            before: None,
            after: None,
            metadata: Some(json!({
                "review_id": review_id,
                "fields": fields
            })),
            outcome: "failed".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tracing::warn!(
        %review_id,
        paperless_document_id = document_id,
        %stage,
        "review apply stopped by optimistic-concurrency conflict"
    );
    tx.commit().await?;
    Ok(true)
}

/// Release an apply claim after the Paperless PATCH failed: move the row
/// from `applying` back to `to_status` so it can be retried. The single
/// revert for the human apply (API), the autopilot drain (worker) and the
/// intent recovery (`archivist-apply`); conflicts use
/// [`mark_review_apply_conflict`], which shares the same transition. The API
/// and the drain revert to `pending` (#388); `approved`/`edited` remain
/// accepted for legacy intents persisted with that revert status. #253, #439
pub async fn revert_review_from_applying(
    pool: &DbPool,
    review_id: Uuid,
    to_status: &str,
) -> Result<()> {
    let to = review_revert_target(to_status)?;
    let mut tx = pool.begin().await?;
    revert_review_from_applying_tx(&mut tx, review_id, to, None).await?;
    tx.commit().await?;
    Ok(())
}
