use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use aes_gcm::aead::{Aead, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{Context, Result, anyhow};
use archivist_core::{
    AiArtifactStorageMode, AuditEventInput, BacklogCounts, DashboardBacklogPoint,
    DashboardComparison, DashboardCostBucket, DashboardLiveFailure, DashboardLiveJob,
    DashboardLiveLlmEvent, DashboardLiveRun, DashboardLiveStatus, DashboardRange,
    DashboardStageStatus, DashboardStats, DashboardStatusCount, DashboardTimeBucket,
    DocumentChatSource, DocumentInventoryItem, DuplicateDocument, DuplicateGroup,
    LanguageDetection, NeedsAttentionItem, ProcessingMode, ProviderUsageStats, QualityStats, Role,
    RuntimeSettings, ServiceProcessingStatus, Stage, WorkflowRules, WorkflowSafetyStatus,
    redact_sensitive_json,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDate, Timelike, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnection, PgPoolOptions, PgRow};
use sqlx::{Connection, PgPool, Postgres, QueryBuilder, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

mod transitions;

mod chat;
mod inventory;
mod pool;
mod settings;
mod stats;
mod users;

pub use chat::*;
pub use inventory::*;
pub use pool::*;
pub use settings::*;
pub use stats::*;
pub use users::*;

pub use transitions::{
    JobStatus, JobTransition, RECOVERED_STUCK_RUN_ERROR, ReviewStatus, ReviewTransition, RunStatus,
    RunTransition,
};
use transitions::{
    mirror_run_status_tx, revert_review_from_applying_tx, review_revert_target,
    sql_active_job_statuses, sql_active_run_statuses, sql_terminal_run_statuses,
    sql_terminal_stage_statuses, transition_run_tx, transition_runs_tx,
};

pub type DbPool = PgPool;
/// Transaction handle for callers that batch several helpers in one TX
/// without depending on sqlx directly (worker sync batches, #408).
pub type DbTransaction<'a> = Transaction<'a, Postgres>;

static AUDIT_INTEGRITY_VERIFY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Expected outcomes of a review decision that are not server faults
/// (double click, two reviewers racing, stale UI). #391
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum ReviewDecisionError {
    #[error("review item does not exist")]
    NotFound,
    #[error("review item is not pending")]
    NotPending,
}

/// A referenced aggregate does not exist (or, for API tokens, is already
/// revoked). An expected client condition, mapped to 404 by the API. #441
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum NotFoundError {
    #[error("user does not exist")]
    User,
    #[error("API token not found or already revoked")]
    ApiToken,
    #[error("prompt does not exist")]
    Prompt,
}

/// Request context that audit events written while serving an HTTP request
/// inherit when the caller did not set `source_ip` / `user_agent` itself.
/// The API scopes every request with [`with_audit_request_context`]; worker
/// code never sets it, so its events keep `null`. #441
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditRequestContext {
    pub source_ip: Option<String>,
    pub user_agent: Option<String>,
}

tokio::task_local! {
    static AUDIT_REQUEST_CONTEXT: AuditRequestContext;
}

/// Run `future` with `context` as the audit request context. Tasks spawned
/// from it do not inherit the context; re-scope them with
/// [`current_audit_request_context`] when they write audit events.
pub async fn with_audit_request_context<F: std::future::Future>(
    context: AuditRequestContext,
    future: F,
) -> F::Output {
    AUDIT_REQUEST_CONTEXT.scope(context, future).await
}

/// The audit request context of the current task, if any.
pub fn current_audit_request_context() -> Option<AuditRequestContext> {
    AUDIT_REQUEST_CONTEXT.try_with(Clone::clone).ok()
}

fn apply_audit_request_context(event: &mut AuditEventInput) {
    let _ = AUDIT_REQUEST_CONTEXT.try_with(|context| {
        if event.source_ip.is_none() {
            event.source_ip.clone_from(&context.source_ip);
        }
        if event.user_agent.is_none() {
            event.user_agent.clone_from(&context.user_agent);
        }
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: Uuid,
    pub run_id: Uuid,
    pub paperless_document_id: i32,
    pub stage: Stage,
    pub mode: ProcessingMode,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub payload: Value,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEventRecord {
    pub id: Uuid,
    pub event_type: String,
    pub actor_type: String,
    pub actor_id: Option<String>,
    pub paperless_document_id: Option<i32>,
    pub outcome: String,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub metadata: Option<Value>,
    pub prev_event_hash: Option<String>,
    pub event_hash: Option<String>,
    pub hash_version: Option<i16>,
    /// Username of a `user` actor, resolved for display (#448).
    #[serde(default)]
    pub actor_username: Option<String>,
    /// True when the event stored a before and/or after snapshot, i.e. the
    /// detail view (`GET /api/audit/{id}`) has a diff to show (#448).
    #[serde(default)]
    pub has_changes: bool,
}

/// One audit event with its before/after snapshots and request origin, for
/// the audit detail / diff view (#448).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEventDetail {
    #[serde(flatten)]
    pub event: AuditEventRecord,
    pub run_id: Option<Uuid>,
    pub job_id: Option<Uuid>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub source_ip: Option<String>,
    pub user_agent: Option<String>,
}

/// Server-side filters for the audit log (#448). Every field is optional and
/// the filters are AND-ed. `before` is the keyset cursor: only events
/// strictly older than `(created_at, id)` are returned.
#[derive(Debug, Clone, Default)]
pub struct AuditEventFilter {
    pub actor_id: Option<String>,
    pub actor_type: Option<String>,
    pub paperless_document_id: Option<i32>,
    pub event_types: Vec<String>,
    pub outcome: Option<String>,
    /// Inclusive lower bound on created_at.
    pub from: Option<DateTime<Utc>>,
    /// Exclusive upper bound on created_at.
    pub to: Option<DateTime<Utc>>,
    pub before: Option<(DateTime<Utc>, Uuid)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditIntegrityReport {
    pub ok: bool,
    pub checked_events: i64,
    pub legacy_events: i64,
    pub v1_events: i64,
    pub v2_events: i64,
    /// Hashed events written before timestamp canonicalization whose original
    /// sub-microsecond suffix was reconstructed, persisted as a validated
    /// lookup hint, and verified without changing the stored event or hash.
    pub legacy_precision_events: i64,
    pub latest_event_hash: Option<String>,
    pub broken_event_id: Option<Uuid>,
    pub broken_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionResult {
    pub audit_events_deleted: i64,
    pub ai_artifacts_deleted: i64,
    pub ocr_page_cache_deleted: i64,
    /// Terminal pipeline_runs pruned past `runs_retention_days`; their jobs
    /// and ai_artifacts go with them via ON DELETE CASCADE.
    #[serde(default)]
    pub pipeline_runs_deleted: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryCandidate {
    pub run_id: Uuid,
    pub job_id: Option<Uuid>,
    pub paperless_document_id: i32,
    pub stage: Option<Stage>,
    pub status: String,
    pub lease_owner: Option<String>,
    pub lease_until: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverySummary {
    pub stale_leases_requeued: i64,
    pub stuck_runs_failed: i64,
    pub stuck_runs_completed: i64,
}

/// Computes the cross-run job priority for an auto-selected document.
///
/// Newer Paperless document ids win (smaller priority value). Saturating math keeps the
/// result in `[1, 1_000_000]` so even synthetic doc ids beyond a million never collide with
/// the manual-trigger priority of 0.
pub fn age_derived_priority(paperless_document_id: i32) -> i64 {
    1_000_000_i64
        .saturating_sub(paperless_document_id as i64)
        .max(1)
}

pub async fn create_run_with_jobs(
    pool: &DbPool,
    paperless_document_id: i32,
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
) -> Result<Uuid> {
    create_run_with_jobs_with_priority(
        pool,
        paperless_document_id,
        stages,
        mode,
        trigger_tag,
        actor,
        None,
    )
    .await
}

/// Variant of [`create_run_with_jobs`] that lets callers stamp an explicit cross-run priority on
/// every job. `None` falls back to the age-derived priority (newer doc -> claimed first).
///
/// Manual triggers should pass `Some(0)`; auto-selector / delta-sync paths should pass `None`
/// (or [`age_derived_priority`]). Job payload carries TWO priority values:
///
///   * `priority`        — cross-run ordering (smaller wins)
///   * `stage_priority`  — within-run stage ordering (smaller wins)
///
/// Splitting them in v1.4.0 lets the age-derived value live in `priority` without breaking the
/// existing claim_jobs subquery that enforces stage ordering via the second column.
pub async fn create_run_with_jobs_with_priority(
    pool: &DbPool,
    paperless_document_id: i32,
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
    priority: Option<i64>,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let run_id = create_run_with_jobs_on_tx(
        &mut tx,
        paperless_document_id,
        stages,
        mode,
        trigger_tag,
        actor,
        priority,
    )
    .await?;
    tx.commit().await?;
    Ok(run_id)
}

/// Bulk re-run: create one run (with the given stages) per document, committed in bounded
/// chunks of [`CREATE_RUNS_CHUNK_SIZE`] documents.
///
/// Used by the `/api/batches/rerun` endpoint so operators can re-trigger a hand-picked set of
/// "succeeded-but-wrong" documents in one shot instead of one trigger at a time. The active-run
/// guard inside [`prepare_run_with_jobs_on_tx`] still applies per document, so ids that already
/// have an in-flight run are silently reused (no duplicate run). Returns the number of documents
/// processed (the de-duplicated input set; each contributes exactly one run, new or reused).
pub async fn create_runs_for_documents(
    pool: &DbPool,
    document_ids: &[i32],
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
    priority: Option<i64>,
) -> Result<i64> {
    if document_ids.is_empty() {
        return Ok(0);
    }
    let mut document_ids = document_ids.to_vec();
    document_ids.sort_unstable();
    document_ids.dedup();

    let mut queued: i64 = 0;
    // One transaction per bounded chunk: each document holds a transaction
    // scoped advisory lock until commit, so a "rerun all failed" over
    // thousands of documents in one transaction overflowed the shared lock
    // table ("out of shared memory") and blocked the worker's per-document
    // locks for the whole batch. Chunks stay in ascending ID order, so the
    // canonical lock order below is preserved across chunks. A failure keeps
    // the chunks committed before it (each document is idempotent through the
    // active-run guard, so a retry does not duplicate runs). #390
    for chunk in document_ids.chunks(CREATE_RUNS_CHUNK_SIZE) {
        // Amortise one transaction across the chunk instead of a begin+commit per document.
        let mut tx = pool.begin().await?;
        // Acquire every document lock before the first audit append. Audit chaining
        // also uses a transaction-scoped advisory lock, so taking all document
        // locks first gives every batch the same deadlock-free lock order.
        lock_active_run_documents_tx(&mut tx, chunk).await?;
        let mut audit_events = Vec::new();
        for &document_id in chunk {
            let prepared = prepare_run_with_jobs_on_tx(
                &mut tx,
                document_id,
                stages,
                mode,
                trigger_tag,
                actor,
                priority,
            )
            .await?;
            if let Some(event) = prepared.audit_event {
                audit_events.push(event);
            }
            queued += 1;
        }
        for event in audit_events {
            append_audit_tx(&mut tx, event).await?;
        }
        tx.commit().await?;
    }
    Ok(queued)
}

/// Documents per [`create_runs_for_documents`] transaction. #390
pub const CREATE_RUNS_CHUNK_SIZE: usize = 250;

/// Document ids the dashboard surfaces as "failed" — a stage (`ocr` or
/// `metadata`) is in `failed` state — and that are NOT currently being
/// reprocessed (no active run). Backs the "re-run all failed" maintenance
/// action so an operator does not have to select them by hand. The active-run
/// exclusion avoids shadowing an in-flight auto-selected run; `create_runs_for_documents`'
/// own per-document guard is a second line of defence.
pub async fn failed_document_ids(pool: &DbPool) -> Result<Vec<i32>> {
    let rows = sqlx::query(concat!(
        r#"
        select paperless_document_id
          from document_inventory
         where (ocr_status = 'failed' or metadata_status = 'failed')
           and coalesce(current_run_status, '') not in ("#,
        sql_active_run_statuses!(),
        r#")
         order by paperless_document_id
        "#
    ))
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get::<i32, _>("paperless_document_id"))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

struct PreparedRunCreation {
    run_id: Uuid,
    audit_event: Option<AuditEventInput>,
}

/// Resolve or create one active run and materialise its jobs/inventory state,
/// but leave the `run.created` audit append to the caller. Batch callers first
/// prepare every document, then take the global audit-chain lock; this keeps
/// all unique-index conflict waits ahead of audit serialization.
async fn prepare_run_with_jobs_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    paperless_document_id: i32,
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
    priority: Option<i64>,
) -> Result<PreparedRunCreation> {
    if stages.is_empty() {
        return Err(anyhow!("cannot create a run without stages"));
    }

    lock_active_run_document_tx(tx, paperless_document_id).await?;

    let cross_run_priority =
        priority.unwrap_or_else(|| age_derived_priority(paperless_document_id));
    let stages_json = serde_json::to_value(stages)?;
    let mode_text = mode.to_string();
    let run_id = loop {
        let inserted = sqlx::query(concat!(
            r#"
            insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
            values ($1, $2, $3, 'queued', $4)
            on conflict (paperless_document_id)
              where status in ("#,
            sql_active_run_statuses!(),
            r#")
            do nothing
            returning id
            "#
        ))
        .bind(paperless_document_id)
        .bind(&mode_text)
        .bind(trigger_tag)
        .bind(&stages_json)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(row) = inserted {
            break row.try_get("id")?;
        }

        // `ON CONFLICT DO NOTHING` waits for an in-flight conflicting
        // transaction. A concurrent active-to-terminal transition can remove
        // that conflict before this follow-up snapshot; retry in that narrow
        // window instead of surfacing a missing-row error.
        if let Some(row) = sqlx::query(concat!(
            r#"
            select id from pipeline_runs
             where paperless_document_id = $1
               and status in ("#,
            sql_active_run_statuses!(),
            r#")
             order by created_at desc
             limit 1
            "#
        ))
        .bind(paperless_document_id)
        .fetch_optional(&mut **tx)
        .await?
        {
            return Ok(PreparedRunCreation {
                run_id: row.try_get("id")?,
                audit_event: None,
            });
        }
    };

    for (index, stage) in stages.iter().enumerate() {
        sqlx::query(
            r#"
            insert into jobs (run_id, paperless_document_id, stage, status, payload)
            values ($1, $2, $3, 'queued', $4)
            "#,
        )
        .bind(run_id)
        .bind(paperless_document_id)
        .bind(stage.to_string())
        .bind(json!({
            "priority": cross_run_priority,
            "stage_priority": ((index as i32) + 1) * 10,
        }))
        .execute(&mut **tx)
        .await?;
    }

    sqlx::query(
        r#"
        insert into document_inventory (paperless_document_id, current_run_status, last_run_id, updated_at)
        values ($1, 'queued', $2, now())
        on conflict (paperless_document_id)
        do update set current_run_status = 'queued',
                      last_run_id = excluded.last_run_id,
                      updated_at = now()
        "#,
    )
    .bind(paperless_document_id)
    .bind(run_id)
    .execute(&mut **tx)
    .await?;

    Ok(PreparedRunCreation {
        run_id,
        audit_event: Some(AuditEventInput {
            event_type: "run.created".to_owned(),
            actor_type: actor.to_owned(),
            actor_id: None,
            run_id: Some(run_id),
            job_id: None,
            paperless_document_id: Some(paperless_document_id),
            before: None,
            after: Some(json!({ "stages": stages, "mode": mode, "trigger_tag": trigger_tag })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        }),
    })
}

/// Per-document convenience wrapper for single-run callers. Multi-document
/// callers use `prepare_run_with_jobs_on_tx` directly and defer all audits
/// until every get-or-create decision has completed.
async fn create_run_with_jobs_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    paperless_document_id: i32,
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
    priority: Option<i64>,
) -> Result<Uuid> {
    let prepared = prepare_run_with_jobs_on_tx(
        tx,
        paperless_document_id,
        stages,
        mode,
        trigger_tag,
        actor,
        priority,
    )
    .await?;
    if let Some(event) = prepared.audit_event {
        append_audit_tx(tx, event).await?;
    }
    Ok(prepared.run_id)
}

/// Serialize the active-run lookup and creation only for one Paperless
/// document. The unique partial index remains the final database invariant;
/// this lock turns its expected concurrency conflict into get-or-create
/// behavior for every caller sharing this transaction helper.
async fn lock_active_run_document_tx(
    tx: &mut Transaction<'_, Postgres>,
    paperless_document_id: i32,
) -> Result<()> {
    sqlx::query(
        r#"
        select pg_advisory_xact_lock(
          hashtext('paperless_archivist_active_run_document'),
          $1
        )
        "#,
    )
    .bind(paperless_document_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Acquire a batch's document locks in canonical order before any run-created
/// audit event takes the global audit-chain lock. Re-acquiring a lock later in
/// `prepare_run_with_jobs_on_tx` is transaction-local and re-entrant.
async fn lock_active_run_documents_tx(
    tx: &mut Transaction<'_, Postgres>,
    document_ids: &[i32],
) -> Result<()> {
    let mut document_ids = document_ids.to_vec();
    document_ids.sort_unstable();
    document_ids.dedup();
    for &document_id in &document_ids {
        lock_active_run_document_tx(tx, document_id).await?;
    }
    Ok(())
}

pub async fn queue_missing_stage(
    pool: &DbPool,
    stage: Stage,
    mode: ProcessingMode,
    actor: &str,
    rules: &WorkflowRules,
    max_documents: Option<i64>,
) -> Result<i64> {
    let column = status_column_for_stage(stage)?;
    let include_tags = WorkflowRules::normalized_tags(&rules.include_tags);
    let exclude_tags = WorkflowRules::normalized_tags(&rules.exclude_tags);
    // Eligibility is fully expressible in SQL for this function, so push the budget as `limit $3`
    // and avoid materialising the entire candidate set in Rust.
    let limit_clause = match max_documents {
        Some(_) => "limit $4",
        None => "",
    };
    // #410: same terminal-status list as `stage_needs_work` (it used to miss
    // `rejected`, so batches re-queued rejected documents).
    let query = format!(
        r#"
        select paperless_document_id
          from document_inventory
         where {column} <> all($3::text[])
           and coalesce(current_run_status, '') not in ({active_runs})
           and ($1::text[] = '{{}}' or current_tags && $1::text[])
           and not (current_tags && $2::text[])
         order by paperless_document_id
         {limit_clause}
        "#,
        active_runs = sql_active_run_statuses!(),
    );
    // SAFETY: `query` is assembled from static fragments plus the validated
    // `limit_clause`; all caller data flows through bind parameters below.
    let mut builder = sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(&include_tags)
        .bind(&exclude_tags)
        .bind(TERMINAL_STAGE_STATUSES);
    if let Some(limit) = max_documents {
        builder = builder.bind(limit);
    }
    let rows = builder.fetch_all(pool).await?;
    let document_ids = rows
        .into_iter()
        .map(|row| row.try_get::<i32, _>("paperless_document_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // Amortise one transaction across the whole batch instead of a begin+commit per document.
    let mut tx = pool.begin().await?;
    // Lock the complete ordered candidate set before the first run-created
    // audit takes the audit-chain lock.
    lock_active_run_documents_tx(&mut tx, &document_ids).await?;
    let mut created = 0;
    let mut audit_events = Vec::new();
    for document_id in document_ids {
        // Age-derived priority — newer documents jump ahead of older ones in claim_jobs.
        // "manual-batch" is the operator-initiated bulk path, but we still rank by age so
        // a fresh scan doesn't get blocked behind a backfill triggered minutes earlier.
        let prepared = prepare_run_with_jobs_on_tx(
            &mut tx,
            document_id,
            &[stage],
            mode,
            "manual-batch",
            actor,
            Some(age_derived_priority(document_id)),
        )
        .await?;
        if let Some(event) = prepared.audit_event {
            audit_events.push(event);
        }
        created += 1;
    }
    for event in audit_events {
        append_audit_tx(&mut tx, event).await?;
    }
    tx.commit().await?;
    Ok(created)
}

/// Return documents whose enabled business stages are resolved but whose
/// authoritative global Paperless completion tag is still missing. Active and
/// review-waiting runs are excluded so reconciliation cannot finalize work in
/// progress.
pub async fn completed_document_ids_missing_full_tag(
    pool: &DbPool,
    enabled_stages: &[Stage],
) -> Result<Vec<i32>> {
    let ocr_enabled = enabled_stages.contains(&Stage::Ocr);
    let metadata_enabled = enabled_stages.contains(&Stage::Metadata);
    if !ocr_enabled && !metadata_enabled {
        return Ok(Vec::new());
    }

    sqlx::query_scalar(concat!(
        r#"
        select paperless_document_id
          from document_inventory
         where not has_full_completion_tag
           and coalesce(current_run_status, '') not in (
                 "#,
        sql_active_run_statuses!(),
        r#"
               )
           and (
                 not $1
                 or ocr_status = any($3::text[])
               )
           and (
                 not $2
                 or metadata_status = any($3::text[])
               )
         order by paperless_document_id
        "#
    ))
    .bind(ocr_enabled)
    .bind(metadata_enabled)
    .bind(TERMINAL_STAGE_STATUSES)
    .fetch_all(pool)
    .await
    .context("select documents missing the full completion tag")
}

/// Recheck status-based completion-tag eligibility under the per-document
/// advisory lock used by run creation and, when still eligible, reserve the
/// document by recording the global completion tag in the inventory
/// (`has_full_completion_tag = complete = true`) before the Paperless write.
///
/// #410: the previous guard kept this transaction — and with it the advisory
/// lock and a pooled connection — open across the Paperless PATCH. The
/// reservation commits immediately instead: once `has_full_completion_tag`
/// is set, neither candidate discovery nor the auto-selector
/// (`missing_pipeline_stages_for_inventory`) pick the document up again. If
/// the Paperless write fails, call [`release_completion_tag_reservation`];
/// the next Paperless sync re-derives the flag from the real tags either way.
/// Returns `false` when the document is no longer eligible.
pub async fn reserve_completion_tag_reconcile(
    pool: &DbPool,
    paperless_document_id: i32,
    enabled_stages: &[Stage],
) -> Result<bool> {
    let ocr_enabled = enabled_stages.contains(&Stage::Ocr);
    let metadata_enabled = enabled_stages.contains(&Stage::Metadata);
    if !ocr_enabled && !metadata_enabled {
        return Ok(false);
    }

    let mut tx = pool.begin().await?;
    lock_active_run_document_tx(&mut tx, paperless_document_id).await?;
    let reserved = sqlx::query(concat!(
        r#"
        update document_inventory
           set has_full_completion_tag = true,
               complete = true,
               updated_at = now()
         where paperless_document_id = $1
           and not has_full_completion_tag
           and coalesce(current_run_status, '') not in (
                 "#,
        sql_active_run_statuses!(),
        r#"
               )
           and (
                 not $2
                 or ocr_status = any($4::text[])
               )
           and (
                 not $3
                 or metadata_status = any($4::text[])
               )
        "#
    ))
    .bind(paperless_document_id)
    .bind(ocr_enabled)
    .bind(metadata_enabled)
    .bind(TERMINAL_STAGE_STATUSES)
    .execute(&mut *tx)
    .await
    .context("recheck and reserve completion-tag reconciliation")?
    .rows_affected()
        > 0;
    tx.commit().await?;
    Ok(reserved)
}

/// Undo [`reserve_completion_tag_reconcile`] after the Paperless tag write
/// failed. #410
pub async fn release_completion_tag_reservation(
    pool: &DbPool,
    paperless_document_id: i32,
) -> Result<()> {
    sqlx::query(
        r#"
        update document_inventory
           set has_full_completion_tag = false,
               complete = false,
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .execute(pool)
    .await
    .context("release completion-tag reservation")?;
    Ok(())
}

/// Record that the global completion tag now exists in Paperless, so the
/// inventory reflects the write without waiting for the next sync. #410
pub async fn record_full_completion_tag(pool: &DbPool, paperless_document_id: i32) -> Result<()> {
    sqlx::query(
        r#"
        update document_inventory
           set has_full_completion_tag = true,
               complete = true,
               updated_at = now()
         where paperless_document_id = $1
           and not (has_full_completion_tag and complete)
        "#,
    )
    .bind(paperless_document_id)
    .execute(pool)
    .await
    .context("record full completion tag")?;
    Ok(())
}

/// Trigger tag used by the worker's automatic document selector.
pub const AUTO_SELECTOR_TRIGGER: &str = "auto-selector";
/// #401: after this many consecutive failed runs (failed runs since the last
/// succeeded run) the auto-selector stops picking the document; an operator
/// rerun or manual batch is required.
pub const AUTO_SELECTOR_FAILED_RUN_CAP: i64 = 5;
/// #401: base cool-off after a failed run before the auto-selector may pick the
/// document again; doubles per consecutive failed run (1h, 2h, 4h, 8h).
pub const AUTO_SELECTOR_FAILED_COOLOFF_BASE_SECONDS: f64 = 3600.0;

pub async fn queue_missing_pipeline(
    pool: &DbPool,
    enabled_stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
    rules: &WorkflowRules,
    max_documents: Option<i64>,
) -> Result<i64> {
    let include_tags = WorkflowRules::normalized_tags(&rules.include_tags);
    let exclude_tags = WorkflowRules::normalized_tags(&rules.exclude_tags);
    // Eligibility depends on which stages are enabled (Rust-side filter), so we fetch in
    // capped chunks of ~2x budget keyset-paginated by paperless_document_id rather than push
    // a brittle predicate into SQL. When the budget is None, fetch everything in one shot.
    let chunk_size = max_documents.map(|limit| limit.saturating_mul(2).max(16));
    // #401: `stage_needs_work("failed")` is true, so without a cool-off a
    // permanently failing document is re-queued on every selector tick and
    // (ordered by id) can consume the whole hourly/daily budget. Only the
    // automatic selector is throttled; operator-initiated batches and manual
    // reruns bypass the cool-off.
    let apply_failed_cooloff = trigger_tag == AUTO_SELECTOR_TRIGGER;

    // Amortise one transaction across every chunk + per-doc insert. Candidate
    // discovery completes before document locks are taken so no transaction
    // ever acquires a new document lock after taking the audit-chain lock.
    let mut tx = pool.begin().await?;
    let mut candidates: Vec<(i32, Vec<Stage>)> = Vec::new();
    let mut last_seen: i32 = i32::MIN;
    loop {
        if max_documents.is_some_and(|limit| candidates.len() as i64 >= limit) {
            break;
        }
        let limit_clause = match chunk_size {
            Some(_) => "limit $7",
            None => "",
        };
        let query = format!(
            r#"
            select di.paperless_document_id,
                   di.ocr_status,
                   di.metadata_status,
                   di.has_ocr_completion_tag,
                   di.has_tagging_completion_tag,
                   di.has_full_completion_tag
              from document_inventory di
             where coalesce(di.current_run_status, '') not in ({active_runs})
               and ($1::text[] = '{{}}' or di.current_tags && $1::text[])
               and not (di.current_tags && $2::text[])
               and di.paperless_document_id > $3
               -- #401: failed-run cool-off for the auto-selector only.
               and (not $4 or not exists (
                     select 1
                       from (
                         select count(*) as failed_runs,
                                max(coalesce(fr.finished_at, fr.updated_at)) as last_failed_at
                           from pipeline_runs fr
                          where fr.paperless_document_id = di.paperless_document_id
                            and fr.status = 'failed'
                            and fr.created_at > coalesce((
                                  select max(sr.created_at)
                                    from pipeline_runs sr
                                   where sr.paperless_document_id = di.paperless_document_id
                                     and sr.status = 'succeeded'
                                ), '-infinity'::timestamptz)
                       ) f
                      where f.failed_runs >= $5
                         or (f.failed_runs > 0
                             and f.last_failed_at > now() - make_interval(
                                   secs => $6 * power(2, least(f.failed_runs - 1, 10))
                                 ))
                   ))
             order by di.paperless_document_id
             {limit_clause}
            "#,
            active_runs = sql_active_run_statuses!(),
        );
        // SAFETY: `query` is assembled from static fragments plus the validated
        // `limit_clause`; all caller data flows through bind parameters below.
        let mut builder = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(&include_tags)
            .bind(&exclude_tags)
            .bind(last_seen)
            .bind(apply_failed_cooloff)
            .bind(AUTO_SELECTOR_FAILED_RUN_CAP)
            .bind(AUTO_SELECTOR_FAILED_COOLOFF_BASE_SECONDS);
        if let Some(size) = chunk_size {
            builder = builder.bind(size);
        }
        let rows = builder.fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            break;
        }
        let fetched = rows.len();
        for row in rows {
            let document_id: i32 = row.try_get("paperless_document_id")?;
            last_seen = document_id.max(last_seen);
            if max_documents.is_some_and(|limit| candidates.len() as i64 >= limit) {
                break;
            }
            let stages = missing_pipeline_stages_for_inventory(
                enabled_stages,
                InventoryStageState {
                    ocr_status: row.try_get("ocr_status")?,
                    metadata_status: row.try_get("metadata_status")?,
                    has_ocr_completion_tag: row.try_get("has_ocr_completion_tag")?,
                    has_tagging_completion_tag: row.try_get("has_tagging_completion_tag")?,
                    has_full_completion_tag: row.try_get("has_full_completion_tag")?,
                },
            );
            if stages.is_empty() {
                continue;
            }

            candidates.push((document_id, stages));
        }
        // No budget set means we already fetched everything once.
        if chunk_size.is_none() {
            break;
        }
        // No more rows possible than we fetched in this round.
        if chunk_size.is_some_and(|size| fetched < size as usize) {
            break;
        }
    }

    let document_ids = candidates
        .iter()
        .map(|(document_id, _)| *document_id)
        .collect::<Vec<_>>();
    lock_active_run_documents_tx(&mut tx, &document_ids).await?;
    let mut audit_events = Vec::new();
    for (document_id, stages) in &candidates {
        // Age-derived priority — newer documents drain through the full
        // pipeline (OCR -> Metadata) before older queued documents.
        let prepared = prepare_run_with_jobs_on_tx(
            &mut tx,
            *document_id,
            stages,
            mode,
            trigger_tag,
            actor,
            Some(age_derived_priority(*document_id)),
        )
        .await?;
        if let Some(event) = prepared.audit_event {
            audit_events.push(event);
        }
    }
    for event in audit_events {
        append_audit_tx(&mut tx, event).await?;
    }
    tx.commit().await?;
    Ok(candidates.len() as i64)
}

struct InventoryStageState {
    ocr_status: String,
    metadata_status: String,
    has_ocr_completion_tag: bool,
    has_tagging_completion_tag: bool,
    has_full_completion_tag: bool,
}

fn missing_pipeline_stages_for_inventory(
    enabled_stages: &[Stage],
    state: InventoryStageState,
) -> Vec<Stage> {
    if state.has_full_completion_tag {
        return Vec::new();
    }

    enabled_stages
        .iter()
        .copied()
        .filter(|stage| match stage {
            Stage::Ocr => !state.has_ocr_completion_tag && stage_needs_work(&state.ocr_status),
            // The consolidated stage's own status column decides. The six
            // legacy per-field columns this arm used to OR in (v1.3 snapshot
            // support) were dropped in migration 0039 — they had been the
            // constant 'unknown' on every row since the v1.4.0 consolidation,
            // which made this arm enqueue Metadata for EVERY document without
            // the legacy tagging-completion tag, even ones whose
            // metadata_status was already 'succeeded'.
            Stage::Metadata => {
                !state.has_tagging_completion_tag && stage_needs_work(&state.metadata_status)
            }
            Stage::Apply => false,
        })
        .collect()
}

/// Inventory stage statuses that count as resolved: no further automatic work.
/// `rejected` is deliberately terminal — an operator declined the suggestion,
/// so the document gets the global processed tag via completion
/// reconciliation and is excluded from further automatic selection; a manual
/// rerun remains possible. Single source for `stage_needs_work`,
/// `queue_missing_stage` and completion reconciliation. #410
pub const TERMINAL_STAGE_STATUSES: &[&str] = &["succeeded", "skipped", "not_needed", "rejected"];

fn stage_needs_work(status: &str) -> bool {
    !TERMINAL_STAGE_STATUSES.contains(&status)
}

/// One index-ordered pass of [`claim_jobs`]. #412.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimPass {
    /// `running` jobs whose lease expired (worker crash/OOM) with attempts left.
    StaleLease,
    /// `queued` jobs that already failed at least once (retry bias).
    Retry,
    /// Every other runnable `queued` job.
    Queued,
}

/// Predicate shared by every claim pass: a job may only run once all earlier
/// stages of its run are resolved.
const CLAIM_STAGE_ORDER_GUARD: &str = concat!(
    r#"
             and not exists (
               select 1
                 from jobs prev
                where prev.run_id = jobs.run_id
                  and prev.stage_priority < jobs.stage_priority
                  and prev.status in ("#,
    sql_active_job_statuses!(),
    r#", 'failed')
             )"#
);

/// The candidate SELECT for one claim pass: filter and ORDER BY are shaped so
/// the planner can walk an index in order and stop after `limit` rows instead
/// of sorting the backlog. #412. Public so the DB tests can `EXPLAIN` it.
pub fn claim_jobs_candidate_sql(pass: ClaimPass) -> String {
    let (filter, order) = match pass {
        ClaimPass::StaleLease => (
            // #402: exhausted rows are failed by `fail_exhausted_stale_jobs_tx`
            // and never re-leased here.
            "status = 'running' and lease_until < now() and attempts < max_attempts",
            "lease_until",
        ),
        ClaimPass::Retry => (
            "status = 'queued' and error_message is not null and attempts > 0 and run_after <= now()",
            "priority, stage_priority, run_after, created_at",
        ),
        ClaimPass::Queued => (
            "status = 'queued' and run_after <= now()",
            "priority, stage_priority, run_after, created_at",
        ),
    };
    format!(
        r#"
          select id,
                 status as prior_status,
                 lease_owner as prior_lease_owner,
                 attempts as prior_attempts
            from jobs
           where {filter}{CLAIM_STAGE_ORDER_GUARD}
           order by {order}
           for update skip locked
           limit $1"#
    )
}

fn claim_jobs_pass_sql(pass: ClaimPass) -> String {
    let candidates = claim_jobs_candidate_sql(pass);
    format!(
        r#"
        with claimed as ({candidates}
        ),
        updated as (
          update jobs j
             set status = 'running',
                 lease_owner = $2,
                 lease_until = now() + make_interval(secs => $3),
                 attempts = attempts + 1,
                 updated_at = now()
            from claimed
           where j.id = claimed.id
          returning j.id, j.run_id, j.paperless_document_id, j.stage, j.status,
                    j.attempts, j.max_attempts, j.payload,
                    claimed.prior_status, claimed.prior_lease_owner, claimed.prior_attempts
        )
        select u.id, u.run_id, u.paperless_document_id, u.stage, r.mode, u.status,
               u.attempts, u.max_attempts, u.payload,
               u.prior_status, u.prior_lease_owner, u.prior_attempts
          from updated u
          join pipeline_runs r on r.id = u.run_id
        "#
    )
}

/// #402: fail `running` jobs whose lease expired after they had already used
/// their last attempt. Such a job took the worker down (OOM kill, abort) before
/// `fail_job` could enforce the retry budget; reclaiming it again would just
/// crash the next worker. Returns the number of jobs failed.
async fn fail_exhausted_stale_jobs_tx(
    tx: &mut Transaction<'_, Postgres>,
    limit: i64,
) -> Result<usize> {
    let rows = sqlx::query(
        r#"
        with exhausted as (
          select id
            from jobs
           where status = 'running'
             and lease_until < now()
             and attempts >= max_attempts
           order by lease_until
           for update skip locked
           limit $1
        )
        update jobs j
           set status = 'failed',
               error_message = format(
                 'lease expired after attempt %s of %s without a result; the worker likely crashed (OOM/panic) while processing this job',
                 j.attempts, j.max_attempts
               ),
               lease_owner = null,
               lease_until = null,
               updated_at = now()
          from exhausted
         where j.id = exhausted.id
        returning j.id, j.run_id, j.paperless_document_id, j.stage, j.attempts,
                  j.max_attempts, j.error_message
        "#,
    )
    .bind(limit)
    .fetch_all(&mut **tx)
    .await?;
    for row in &rows {
        let job_id: Uuid = row.try_get("id")?;
        let run_id: Uuid = row.try_get("run_id")?;
        let document_id: i32 = row.try_get("paperless_document_id")?;
        let stage: Stage = row.try_get::<String, _>("stage")?.parse()?;
        let error: String = row.try_get("error_message")?;
        tracing::warn!(
            job_id = %job_id,
            run_id = %run_id,
            stage = %stage,
            "failing job with expired lease and exhausted retry budget"
        );
        apply_permanent_job_failure_tx(tx, job_id, run_id, document_id, stage, &error).await?;
        append_audit_tx(
            tx,
            AuditEventInput {
                event_type: "job.failed".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: Some(run_id),
                job_id: Some(job_id),
                paperless_document_id: Some(document_id),
                before: None,
                after: Some(json!({ "status": "failed", "retry": false })),
                metadata: Some(json!({
                    "stage": stage,
                    "reason": "lease_expired_attempts_exhausted",
                    "attempts": row.try_get::<i32, _>("attempts")?,
                    "max_attempts": row.try_get::<i32, _>("max_attempts")?,
                })),
                outcome: "failed".to_owned(),
                error_message: Some(error),
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }
    if !rows.is_empty() {
        increment_metric_counter_tx(tx, "job_failures_total", rows.len() as i64).await?;
    }
    Ok(rows.len())
}

/// Permanent-failure follow-ups shared by `fail_job` and the #402 stale-lease
/// path: inventory stage `failed`, run `failed`, and cancel the run's siblings.
async fn apply_permanent_job_failure_tx(
    tx: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
    run_id: Uuid,
    paperless_document_id: i32,
    stage: Stage,
    error: &str,
) -> Result<()> {
    set_inventory_stage_status_tx(
        tx,
        paperless_document_id,
        stage,
        "failed",
        Some(error),
        false,
        Some(run_id),
    )
    .await?;
    transition_run_tx(tx, run_id, RunTransition::Failed, Some(error)).await?;
    // `set_inventory_stage_status_tx` already flipped the badge to `failed`;
    // the mirror keeps it equal to the run should the run not have been
    // active any more. #439
    mirror_run_status_tx(tx, &[run_id], None).await?;
    // A permanent failure aborts the whole run, so cancel the sibling jobs in the same TX.
    // Mirrors the reject path: leaving them `queued` makes them unclaimable (the claim guard
    // blocks them behind the failed stage) yet still scanned on every poll, inflating
    // `jobs_queued` forever.
    cancel_active_run_jobs_tx(tx, run_id, Some(job_id)).await?;
    Ok(())
}

/// [`JobTransition::Cancelled`] for every still-active job of a run except
/// `keep_job_id`. Shared by the permanent-failure and all-rejected paths. #439
async fn cancel_active_run_jobs_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    keep_job_id: Option<Uuid>,
) -> Result<u64> {
    let updated = sqlx::query(concat!(
        r#"
        update jobs
           set status = 'cancelled',
               lease_owner = null,
               lease_until = null,
               updated_at = now()
         where run_id = $1
           and ($2::uuid is null or id <> $2)
           and status in ("#,
        sql_active_job_statuses!(),
        ")"
    ))
    .bind(run_id)
    .bind(keep_job_id)
    .execute(&mut **tx)
    .await?;
    Ok(updated.rows_affected())
}

/// Run follow-up after one stage of it settled successfully (direct
/// `complete_job` or the review aggregate): the run succeeds once no job is
/// active any more, otherwise it goes back to `queued` for the next stage.
/// The inventory badge follows the run (#303/#414), `complete` the Paperless
/// completion tag (#410). #439
async fn settle_run_after_stage_tx(tx: &mut Transaction<'_, Postgres>, run_id: Uuid) -> Result<()> {
    let event = if no_remaining_jobs_tx(tx, run_id).await? {
        RunTransition::Succeeded
    } else {
        RunTransition::StageAdvanced
    };
    transition_run_tx(tx, run_id, event, None).await?;
    mirror_run_status_tx(tx, &[run_id], None).await?;
    Ok(())
}

pub async fn claim_jobs(
    pool: &DbPool,
    limit: i64,
    lease_owner: &str,
    lease_seconds: i64,
) -> Result<Vec<JobRecord>> {
    // v1.4.0: `priority` now carries the cross-run (age-derived) value while `stage_priority`
    // enforces within-run stage ordering. The inner subquery uses stage_priority so all jobs
    // of one run share the same `priority` value without losing OCR -> Metadata ordering. The
    // outer ORDER BY claims newer documents first (smaller priority), then earlier stages
    // (smaller stage_priority), then FIFO as a tiebreaker. The retry bias (failed jobs first)
    // stays first in the order so a stuck retry never starves out.
    // #412: the former single query ORed `queued`/`running` in WHERE and led the ORDER BY with
    // a CASE retry-bias expression, so no index could serve it and every poll sorted the whole
    // backlog. The claim now runs as up to three index-ordered passes in one TX, each taking
    // only what the previous passes left of `limit`:
    //   1. stale-lease reclaim (`jobs_lease_until_idx`), budget-exhausted rows excluded (#402);
    //   2. queued retries (`idx_jobs_claim_retry`, migration 0053) — keeps the retry bias;
    //   3. regular queued jobs (`idx_jobs_claim`) in (priority, stage_priority, run_after,
    //      created_at) order.
    // The claim and its run/inventory follow-ups run in one TX so a crash between them can't
    // leave jobs `running` while their run/inventory rows stay `queued`.
    let mut tx = pool.begin().await?;
    // #402: a job whose lease expired after it already consumed its last attempt (worker
    // OOM-killed or crashed mid-job) is failed here instead of being reclaimed forever.
    fail_exhausted_stale_jobs_tx(&mut tx, limit.max(1)).await?;
    let mut rows = Vec::new();
    for pass in [ClaimPass::StaleLease, ClaimPass::Retry, ClaimPass::Queued] {
        let remaining = limit - rows.len() as i64;
        if remaining <= 0 {
            break;
        }
        let query = claim_jobs_pass_sql(pass);
        // SAFETY: `query` is assembled from static per-pass fragments only; all
        // caller data flows through bind parameters.
        let pass_rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(remaining)
            .bind(lease_owner)
            .bind(lease_seconds as f64)
            .fetch_all(&mut *tx)
            .await?;
        rows.extend(pass_rows);
    }

    let mut jobs = Vec::new();
    // (job_id, run_id, document_id, prior_lease_owner, prior_attempts) for stale-lease reclaims.
    let mut reclaimed: Vec<(Uuid, Uuid, i32, Option<String>, i32)> = Vec::new();
    for row in rows {
        let stage: String = row.try_get("stage")?;
        let mode: String = row.try_get("mode")?;
        let prior_status: String = row.try_get("prior_status")?;
        let job = JobRecord {
            id: row.try_get("id")?,
            run_id: row.try_get("run_id")?,
            paperless_document_id: row.try_get("paperless_document_id")?,
            stage: stage.parse()?,
            mode: mode.parse()?,
            status: row.try_get("status")?,
            attempts: row.try_get("attempts")?,
            max_attempts: row.try_get("max_attempts")?,
            payload: row.try_get("payload")?,
        };
        // A prior status of `running` means we reclaimed a job whose lease expired (worker
        // crash/OOM). The `attempts + 1` above silently burns one of `max_attempts` with no
        // terminal outcome, so leave a breadcrumb to keep that lost attempt attributable.
        if prior_status == "running" {
            let prior_lease_owner: Option<String> = row.try_get("prior_lease_owner")?;
            let prior_attempts: i32 = row.try_get("prior_attempts")?;
            tracing::warn!(
                job_id = %job.id,
                run_id = %job.run_id,
                stage = %job.stage,
                prior_lease_owner = prior_lease_owner.as_deref().unwrap_or("<unknown>"),
                attempts = job.attempts,
                "reclaiming job with expired lease; previous attempt consumed without a terminal outcome"
            );
            reclaimed.push((
                job.id,
                job.run_id,
                job.paperless_document_id,
                prior_lease_owner,
                prior_attempts,
            ));
        }
        jobs.push(job);
    }

    for (job_id, run_id, document_id, prior_lease_owner, prior_attempts) in &reclaimed {
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: "job.lease_reclaimed".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: Some(lease_owner.to_owned()),
                run_id: Some(*run_id),
                job_id: Some(*job_id),
                paperless_document_id: Some(*document_id),
                before: None,
                after: None,
                metadata: Some(json!({
                    "prior_lease_owner": prior_lease_owner,
                    "prior_attempts": prior_attempts,
                    "new_lease_owner": lease_owner,
                })),
                outcome: "warning".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }

    if !jobs.is_empty() {
        // Coalesce the per-job mark_run_running follow-up into a pair of bulk UPDATEs so the
        // claim path issues O(1) queries per batch instead of O(N).
        let mut run_ids: Vec<Uuid> = jobs.iter().map(|job| job.run_id).collect();
        run_ids.sort_unstable();
        run_ids.dedup();
        transition_runs_tx(&mut tx, &run_ids, RunTransition::Claimed, None).await?;
        // #408: the mirror locks inventory rows in ascending id order (the
        // order the batched Paperless sync upserts them in) so the two cannot
        // deadlock. #414: only rows whose `last_run_id` is a claimed run.
        mirror_run_status_tx(&mut tx, &run_ids, None).await?;
    }
    tx.commit().await?;
    Ok(jobs)
}

/// Extend the lease on a job we are actively processing. Long multi-page OCR
/// jobs can outlive the lease window granted by `claim_jobs`, which would let a
/// second replica reclaim the stale lease and double-apply the work. The
/// processing worker calls this after each unit of progress to push
/// `lease_until` forward by the same window. Returns `true` when our lease was
/// still held (a row matched) and `false` when the lease was lost — the caller
/// should stop in that case so it can't fight a replica that already took over.
pub async fn bump_job_lease(
    pool: &DbPool,
    job_id: Uuid,
    lease_owner: &str,
    lease_seconds: i64,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        update jobs
           set lease_until = now() + make_interval(secs => $3),
               updated_at = now()
         where id = $1
           and lease_owner = $2
           and status = 'running'
        "#,
    )
    .bind(job_id)
    .bind(lease_owner)
    .bind(lease_seconds as f64)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Current `lease_until` of a job while `lease_owner` still holds it; `None`
/// once the lease was released, completed or taken over. Used by the worker's
/// no-progress watchdog. #407
pub async fn job_lease_until(
    pool: &DbPool,
    job_id: Uuid,
    lease_owner: &str,
) -> Result<Option<DateTime<Utc>>> {
    let lease_until: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "select lease_until from jobs where id = $1 and lease_owner = $2 and status in ('running', 'waiting_review')",
    )
    .bind(job_id)
    .bind(lease_owner)
    .fetch_optional(pool)
    .await?;
    Ok(lease_until.flatten())
}

/// Mark a single run + inventory row as running. `claim_jobs` issues equivalent updates in bulk;
/// this helper exists for callers outside the claim path that legitimately need to flip exactly
/// one run.
#[allow(dead_code)]
pub async fn mark_run_running(pool: &DbPool, run_id: Uuid, _document_id: i32) -> Result<()> {
    let mut tx = pool.begin().await?;
    transition_run_tx(&mut tx, run_id, RunTransition::Claimed, None).await?;
    mirror_run_status_tx(&mut tx, &[run_id], None).await?;
    tx.commit().await?;
    Ok(())
}

/// Mark a job succeeded, but only if `lease_owner` still owns the lease. Returns
/// `true` when our row matched (we owned the lease and applied the completion)
/// and `false` when no row matched — the lease was lost to another replica that
/// reclaimed it. Callers must treat `false` as "lease lost, skip" rather than an
/// error: writing unconditionally would let a worker that already lost its lease
/// double-apply over the replica that legitimately took over.
pub async fn complete_job(
    pool: &DbPool,
    job: &JobRecord,
    lease_owner: &str,
    result: Value,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        r#"
        update jobs
           set status = 'succeeded',
               result = $2,
               lease_owner = null,
               lease_until = null,
               error_message = null,
               updated_at = now()
         where id = $1
           and lease_owner = $3
        "#,
    )
    .bind(job.id)
    .bind(&result)
    .bind(lease_owner)
    .execute(&mut *tx)
    .await?;

    // Lease lost: another replica reclaimed the stale lease and owns this job
    // now. Roll back without touching inventory/run state so we don't fight the
    // worker that took over.
    if updated.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }

    set_inventory_stage_status_tx(
        &mut tx,
        job.paperless_document_id,
        job.stage,
        "succeeded",
        None,
        false,
        Some(job.run_id),
    )
    .await?;

    // Succeed the run once nothing is left, otherwise reset it to 'queued'
    // while stage N+1 is pending. Without the reset, runs that went through a
    // stage via the direct (non-review) full_auto path stayed stuck on
    // 'running' forever ("N stuck run(s)" on the dashboard) although the
    // next-stage jobs were waiting in the queue. #410/#414 via the mirror.
    settle_run_after_stage_tx(&mut tx, job.run_id).await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "job.succeeded".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: None,
            after: Some(result),
            metadata: Some(json!({ "stage": job.stage })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn is_last_active_job(pool: &DbPool, run_id: Uuid, current_job_id: Uuid) -> Result<bool> {
    let row = sqlx::query(concat!(
        r#"
        select not exists(
          select 1 from jobs
           where run_id = $1
             and id <> $2
             and status in ("#,
        sql_active_job_statuses!(),
        r#")
        ) as is_last
        "#
    ))
    .bind(run_id)
    .bind(current_job_id)
    .fetch_one(pool)
    .await?;
    row.try_get("is_last").context("read last active job state")
}

/// Mark a job failed (or schedule a retry), but only if `lease_owner` still owns
/// the lease. Returns `true` when our row matched and `false` when the lease was
/// lost to a replica that reclaimed it. As with `complete_job`, callers must
/// treat `false` as "lease lost, skip" — a worker that lost its lease must not
/// overwrite the state of the replica that took over.
pub async fn fail_job(
    pool: &DbPool,
    job: &JobRecord,
    lease_owner: &str,
    error: &str,
    retryable: bool,
    retry_ceiling: Option<i32>,
) -> Result<bool> {
    // A transient *infrastructure* failure (e.g. the Paperless gateway briefly
    // unreachable, #245) may pass a `retry_ceiling` higher than the per-job
    // `max_attempts`: the document is blameless for an upstream outage, so it
    // should ride the outage out instead of burning its small retry budget and
    // failing permanently. `None` keeps the normal budget; the ceiling never
    // *lowers* it. Bounded (not unbounded like the cooldown release) so a
    // permanently-broken gateway still fails eventually. #305.
    let ceiling = retry_ceiling
        .map(|c| c.max(job.max_attempts))
        .unwrap_or(job.max_attempts);
    let retry = retryable && job.attempts < ceiling;
    let status = if retry { "queued" } else { "failed" };
    let base_delay = (2_i64.pow(job.attempts.clamp(0, 6) as u32)) * 30;
    // +/-25% uniform jitter avoids thundering-herd retries when many workers
    // hit the same transient upstream failure (e.g. provider restart).
    let jitter = (rand::random::<f64>() - 0.5) * 0.5 * base_delay as f64;
    let delay_seconds = ((base_delay as f64) + jitter).max(1.0);
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        r#"
        update jobs
           set status = $2,
               error_message = $3,
               lease_owner = null,
               lease_until = null,
               run_after = case when $2 = 'queued' then now() + make_interval(secs => $4) else run_after end,
               updated_at = now()
         where id = $1
           and lease_owner = $5
        "#,
    )
    .bind(job.id)
    .bind(status)
    .bind(error)
    .bind(delay_seconds)
    .bind(lease_owner)
    .execute(&mut *tx)
    .await?;

    // Lease lost: a replica reclaimed the stale lease. Roll back without
    // cancelling siblings or flipping run state so we don't clobber the worker
    // that took over.
    if updated.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }

    if !retry {
        apply_permanent_job_failure_tx(
            &mut tx,
            job.id,
            job.run_id,
            job.paperless_document_id,
            job.stage,
            error,
        )
        .await?;
    }

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: if retry {
                "job.retry_scheduled"
            } else {
                "job.failed"
            }
            .to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: None,
            after: Some(json!({ "status": status, "retry": retry })),
            metadata: Some(json!({ "stage": job.stage })),
            outcome: if retry { "retry" } else { "failed" }.to_owned(),
            error_message: Some(error.to_owned()),
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    if retry {
        increment_metric_counter_tx(&mut tx, "job_retries_scheduled_total", 1).await?;
    } else {
        // Keep this in the same transaction as the permanent job.failed audit
        // event and state transition. Retries and lease-lost no-ops never reach
        // this branch, so Prometheus rate()/increase() sees a true monotone
        // count of newly permanent failures.
        increment_metric_counter_tx(&mut tx, "job_failures_total", 1).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Release a claimed job lease back to the queue without burning an attempt —
/// used when the worker discovers the active provider for the job's stage is
/// in cooldown. `attempts` is decremented to undo the increment performed by
/// `claim_jobs`, so the per-job retry budget is preserved for the next cycle.
/// `run_after` is set to the cooldown expiry so the job is not re-claimed
/// before the provider is plausibly back.
///
/// Fenced on lease ownership and the running state (like `complete_job` /
/// `fail_job`): a worker whose lease was already reclaimed must not requeue
/// and decrement attempts on a job another replica is now legitimately
/// running. Returns `false` when the lease was lost (no row matched). #253.
///
/// The released job's run is flipped back to `queued` and the change is
/// mirrored onto `document_inventory.current_run_status` in the same
/// transaction, like `claim_jobs`/`recover_stale_leases` do. The pre-#303
/// worker-local variant only updated `jobs`, which parked the run on
/// `running` with zero running jobs until the startup repair
/// `reset_stuck_running_pipeline_runs` flipped it — without mirroring. That
/// pair of gaps is what left ~10% of production inventory rows with a stale
/// `running` badge. #303.
pub async fn release_job_lease_for_cooldown(
    pool: &DbPool,
    job: &JobRecord,
    lease_owner: &str,
    cooldown_until: DateTime<Utc>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let released = sqlx::query(
        r#"
        update jobs
           set status      = 'queued',
               run_after   = $3,
               attempts    = greatest(attempts - 1, 0),
               lease_owner = null,
               lease_until = null,
               updated_at  = now()
         where id = $1
           and lease_owner = $2
           and status = 'running'
        "#,
    )
    .bind(job.id)
    .bind(lease_owner)
    .bind(cooldown_until)
    .execute(&mut *tx)
    .await?;

    // Lease lost: another replica reclaimed the stale lease and owns this job
    // now. Roll back without touching run/inventory state so we don't fight
    // the worker that took over.
    if released.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }

    transition_run_tx(&mut tx, job.run_id, RunTransition::LeaseReleased, None).await?;
    // Mirror whatever the run's status now is, rather than hardcoding
    // 'queued': if the guard above skipped the run flip, copying the actual
    // status keeps the mirror invariant (current_run_status follows the run
    // that last_run_id points at) instead of introducing the opposite drift.
    mirror_run_status_tx(&mut tx, &[job.run_id], None).await?;

    tx.commit().await?;
    Ok(true)
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

pub struct AiArtifactInput<'a> {
    pub run_id: Uuid,
    pub job_id: Uuid,
    pub stage: Stage,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_id: Option<Uuid>,
    pub input_hash: &'a str,
    pub request: Option<Value>,
    pub response: Option<Value>,
    pub normalized_output: Option<Value>,
    pub duration_ms: i32,
    pub storage_mode: AiArtifactStorageMode,
}

/// Look up a cached OCR result for a (document, page_index, page_hash)
/// triple. Returns the cached text when present. Added in v1.5.14 to
/// short-circuit expensive vision-model calls when the exact same
/// rendered page has been transcribed before.
pub async fn lookup_ocr_page_cache(
    pool: &DbPool,
    paperless_document_id: i32,
    page_index: i32,
    page_hash: &str,
) -> Result<Option<String>> {
    let row = sqlx::query(
        r#"
        select ocr_text from ocr_page_cache
         where paperless_document_id = $1
           and page_index = $2
           and page_hash = $3
         limit 1
        "#,
    )
    .bind(paperless_document_id)
    .bind(page_index)
    .bind(page_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|r| r.try_get::<String, _>("ocr_text"))
        .transpose()?)
}

/// Insert (or update) an OCR-page-cache row. Idempotent on the
/// (document, page_index, page_hash) primary key — the second call for
/// the same triple overwrites the cached text and bumps `created_at`.
pub async fn upsert_ocr_page_cache(
    pool: &DbPool,
    paperless_document_id: i32,
    page_index: i32,
    page_hash: &str,
    ocr_text: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into ocr_page_cache
            (paperless_document_id, page_index, page_hash, ocr_text, provider, model)
        values ($1, $2, $3, $4, $5, $6)
        on conflict (paperless_document_id, page_index, page_hash)
        do update set
            ocr_text = excluded.ocr_text,
            provider = excluded.provider,
            model = excluded.model,
            created_at = now()
        "#,
    )
    .bind(paperless_document_id)
    .bind(page_index)
    .bind(page_hash)
    .bind(ocr_text)
    .bind(provider)
    .bind(model)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record the OCR-content-hash on `document_inventory`. Used as the
/// key for the content-hash dedup helper below. Idempotent.
pub async fn set_document_inventory_ocr_content_hash(
    pool: &DbPool,
    paperless_document_id: i32,
    ocr_content_hash: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        update document_inventory
           set ocr_content_hash = $2,
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .bind(ocr_content_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist the (sanitized) OCR body on `document_inventory` so it can be
/// indexed for full-text retrieval in chat search (#217). The companion
/// generated `ocr_body_tsv` column is recomputed by Postgres on write.
/// Idempotent; best-effort from the worker's point of view (a failure
/// here does not fail the OCR stage).
pub async fn set_document_inventory_ocr_body(
    pool: &DbPool,
    paperless_document_id: i32,
    ocr_body: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        update document_inventory
           set ocr_body = $2,
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .bind(ocr_body)
    .execute(pool)
    .await?;
    Ok(())
}

/// Find a recent document whose OCR text hash matches and whose
/// metadata stage has already settled (succeeded). Used by the
/// metadata stage to short-circuit a re-extraction when the same
/// document content has been processed before. Returns the dedup
/// source's `paperless_document_id` and the most recent succeeded
/// metadata `ai_artifacts.normalized` payload so the caller can clone
/// the patch.
pub async fn find_metadata_dedup_source(
    pool: &DbPool,
    current_document_id: i32,
    ocr_content_hash: &str,
) -> Result<Option<(i32, Value)>> {
    let row = sqlx::query(
        r#"
        select di.paperless_document_id as source_id,
               aa.normalized_output as metadata_payload
          from document_inventory di
          join pipeline_runs pr
            on pr.paperless_document_id = di.paperless_document_id
          join ai_artifacts aa
            on aa.run_id = pr.id
           and aa.stage = 'metadata'
         where di.ocr_content_hash = $1
           and di.paperless_document_id <> $2
           and di.metadata_status = 'succeeded'
         order by aa.created_at desc
         limit 1
        "#,
    )
    .bind(ocr_content_hash)
    .bind(current_document_id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| -> Result<(i32, Value)> {
        Ok((r.try_get("source_id")?, r.try_get("metadata_payload")?))
    })
    .transpose()
}

/// Extract the typed token counters persisted on `ai_artifacts.input_tokens`
/// / `output_tokens` (migration 0040) from a raw provider response.
///
/// Mirrors the SQL extraction the usage queries performed before 0040 (and
/// that the 0040 backfill still uses): OpenAI/Anthropic-style
/// `usage.prompt_tokens`/`input_tokens` + `usage.completion_tokens`/
/// `output_tokens`, Ollama top-level `prompt_eval_count`/`eval_count`, and
/// the per-page `pages[]` fallback for OCR responses without a flattened
/// top-level `usage` block. Only plain non-negative integers count — redacted
/// strings and summary objects contribute 0, like the SQL regexp guard.
///
/// Runs on the response BEFORE storage-mode preparation, so the counters stay
/// correct even when `metadata_only` storage strips the Ollama top-level
/// counters from the persisted jsonb.
fn ai_response_token_usage(response: Option<&Value>) -> (i64, i64) {
    fn counter(value: &Value, path: &[&str]) -> i64 {
        let mut current = value;
        for key in path {
            match current.get(key) {
                Some(next) => current = next,
                None => return 0,
            }
        }
        current.as_i64().filter(|count| *count >= 0).unwrap_or(0)
    }
    fn tokens(value: &Value) -> (i64, i64) {
        (
            counter(value, &["usage", "prompt_tokens"])
                + counter(value, &["usage", "input_tokens"])
                + counter(value, &["prompt_eval_count"]),
            counter(value, &["usage", "completion_tokens"])
                + counter(value, &["usage", "output_tokens"])
                + counter(value, &["eval_count"]),
        )
    }

    let Some(response) = response else {
        return (0, 0);
    };
    let (mut input, mut output) = tokens(response);
    // Pages fallback exactly when no top-level `usage` KEY exists (a JSON
    // null `usage` suppresses it, matching the SQL `-> 'usage' is null`
    // semantics of the 0040 backfill), so flattened post-#259 OCR responses
    // are never double counted.
    if response.get("usage").is_none()
        && let Some(pages) = response.get("pages").and_then(Value::as_array)
    {
        for page in pages {
            let (page_input, page_output) = tokens(page);
            input += page_input;
            output += page_output;
        }
    }
    (input, output)
}

pub async fn insert_ai_artifact(pool: &DbPool, input: AiArtifactInput<'_>) -> Result<Uuid> {
    // Token counters are read off the raw response before the storage-mode
    // redaction/summarization so they reflect what the provider reported.
    let (input_tokens, output_tokens) = ai_response_token_usage(input.response.as_ref());
    let request = prepare_ai_artifact_value(input.request, input.storage_mode);
    let response = prepare_ai_artifact_value(input.response, input.storage_mode);
    let normalized_output = input.normalized_output.map(|mut value| {
        replace_nul_in_json(&mut value);
        value
    });

    let id = sqlx::query(
        r#"
        insert into ai_artifacts (
          run_id, job_id, stage, provider, model, prompt_id, input_hash, request, response, normalized_output, duration_ms,
          input_tokens, output_tokens
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        returning id
        "#,
    )
    .bind(input.run_id)
    .bind(input.job_id)
    .bind(input.stage.to_string())
    .bind(input.provider)
    .bind(input.model)
    .bind(input.prompt_id)
    .bind(input.input_hash)
    .bind(request)
    .bind(response)
    .bind(normalized_output)
    .bind(input.duration_ms)
    .bind(input_tokens)
    .bind(output_tokens)
    .fetch_one(pool)
    .await?
    .try_get("id")?;
    Ok(id)
}

/// Replace every U+0000 in JSON strings and object keys with U+FFFD, which
/// PostgreSQL `jsonb`/`text` can store. #415
pub fn replace_nul_in_json(value: &mut Value) {
    const NUL: char = '\u{0}';
    const REPLACEMENT: &str = "\u{FFFD}";
    match value {
        Value::String(text) if text.contains(NUL) => {
            *text = text.replace(NUL, REPLACEMENT);
        }
        Value::Array(items) => items.iter_mut().for_each(replace_nul_in_json),
        Value::Object(map) => {
            if map.keys().any(|key| key.contains(NUL)) {
                let entries = std::mem::take(map);
                *map = entries
                    .into_iter()
                    .map(|(key, value)| (key.replace(NUL, REPLACEMENT), value))
                    .collect();
            }
            map.values_mut().for_each(replace_nul_in_json);
        }
        _ => {}
    }
}

fn prepare_ai_artifact_value(
    value: Option<Value>,
    storage_mode: AiArtifactStorageMode,
) -> Option<Value> {
    let mut value = value?;
    redact_sensitive_json(&mut value);
    // #415: PostgreSQL rejects U+0000 in jsonb; model output occasionally
    // contains it, which failed the insert in `Full` mode.
    replace_nul_in_json(&mut value);
    match storage_mode {
        AiArtifactStorageMode::Full => Some(value),
        AiArtifactStorageMode::Redacted => {
            redact_ai_artifact_content(&mut value);
            Some(value)
        }
        AiArtifactStorageMode::MetadataOnly => Some(ai_artifact_metadata_only(&value)),
    }
}

fn redact_ai_artifact_content(value: &mut Value) {
    const CONTENT_KEYS: &[&str] = &[
        "content",
        "text",
        "prompt",
        "system_prompt",
        "user_prompt",
        "response",
        "images",
        "image",
        "bytes",
        "b64_json",
        "base64",
    ];

    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                let content_key = CONTENT_KEYS
                    .iter()
                    .any(|needle| key.to_ascii_lowercase().contains(needle));
                // `prompt_tokens` / `prompt_eval_count` match the "prompt"
                // substring but are numeric counters, not document content —
                // summarizing them away destroys token statistics.
                if content_key && !matches!(nested, Value::Number(_) | Value::Bool(_)) {
                    *nested = ai_artifact_redaction_summary(nested);
                } else {
                    redact_ai_artifact_content(nested);
                }
            }
        }
        Value::Array(values) => {
            for nested in values {
                redact_ai_artifact_content(nested);
            }
        }
        _ => {}
    }
}

fn ai_artifact_redaction_summary(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({
            "redacted": true,
            "kind": "text",
            "sha256": short_hash(text),
            "chars": text.chars().count()
        }),
        Value::Array(items) => json!({
            "redacted": true,
            "kind": "array",
            "items": items.len()
        }),
        Value::Object(map) => json!({
            "redacted": true,
            "kind": "object",
            "keys": map.len()
        }),
        Value::Null => Value::Null,
        other => json!({
            "redacted": true,
            "kind": "scalar",
            "sha256": short_hash(&other.to_string())
        }),
    }
}

fn ai_artifact_metadata_only(value: &Value) -> Value {
    let mut metadata = json!({
        "storage": "metadata_only",
        "sha256": short_hash(&value.to_string()),
        "json_bytes": value.to_string().len()
    });
    if let (Some(target), Value::Object(source)) = (metadata.as_object_mut(), value) {
        for key in [
            "model",
            "provider",
            "stage",
            "usage",
            "options",
            "done_reason",
        ] {
            if let Some(value) = source.get(key) {
                target.insert(key.to_owned(), value.clone());
            }
        }
    }
    metadata
}

pub async fn recovery_candidates(
    pool: &DbPool,
    older_than_seconds: i64,
) -> Result<Vec<RecoveryCandidate>> {
    let rows = sqlx::query(concat!(
        r#"
        select j.run_id,
               j.id as job_id,
               j.paperless_document_id,
               j.stage,
               j.status,
               j.lease_owner,
               j.lease_until,
               j.updated_at,
               'stale_lease' as reason
          from jobs j
         where j.status = 'running'
           and j.lease_until < now() - make_interval(secs => $1)
        union all
        select r.id as run_id,
               null::uuid as job_id,
               r.paperless_document_id,
               null::text as stage,
               r.status,
               null::text as lease_owner,
               null::timestamptz as lease_until,
               r.updated_at,
               'stuck_run_without_active_jobs' as reason
          from pipeline_runs r
         where r.status in ('queued', 'running', 'applying')
           and r.updated_at < now() - make_interval(secs => $1)
           and not exists (
             select 1
               from jobs j
              where j.run_id = r.id
                and j.status in ("#,
        sql_active_job_statuses!(),
        r#")
           )
         order by updated_at asc
         limit 100
        "#
    ))
    .bind(older_than_seconds as f64)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: Option<String> = row.try_get("stage")?;
            Ok(RecoveryCandidate {
                run_id: row.try_get("run_id")?,
                job_id: row.try_get("job_id")?,
                paperless_document_id: row.try_get("paperless_document_id")?,
                stage: stage.map(|stage| stage.parse()).transpose()?,
                status: row.try_get("status")?,
                lease_owner: row.try_get("lease_owner")?,
                lease_until: row.try_get("lease_until")?,
                updated_at: row.try_get("updated_at")?,
                reason: row.try_get("reason")?,
            })
        })
        .collect()
}

pub async fn recover_stale_leases(
    pool: &DbPool,
    older_than_seconds: i64,
    actor_id: Uuid,
) -> Result<RecoverySummary> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        r#"
        with stale as (
          select id, run_id, paperless_document_id
            from jobs
           where status = 'running'
             and lease_until < now() - make_interval(secs => $1)
           for update
        )
        update jobs j
           set status = 'queued',
               lease_owner = null,
               lease_until = null,
               run_after = now(),
               updated_at = now()
          from stale
         where j.id = stale.id
        returning j.id, j.run_id, j.paperless_document_id
        "#,
    )
    .bind(older_than_seconds as f64)
    .fetch_all(&mut *tx)
    .await?;

    let run_ids = rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("run_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let job_ids = rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    transition_runs_tx(&mut tx, &run_ids, RunTransition::StaleLeaseRequeued, None).await?;
    mirror_run_status_tx(&mut tx, &run_ids, None).await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "operations.stale_leases_requeued".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "count": job_ids.len(), "job_ids": job_ids })),
            metadata: Some(json!({ "older_than_seconds": older_than_seconds })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(RecoverySummary {
        stale_leases_requeued: job_ids.len() as i64,
        stuck_runs_failed: 0,
        stuck_runs_completed: 0,
    })
}

pub async fn recover_stuck_runs(
    pool: &DbPool,
    older_than_seconds: i64,
    actor_id: Uuid,
) -> Result<RecoverySummary> {
    let mut tx = pool.begin().await?;
    // #439: select the stuck runs, then move them through the transition
    // table (which re-checks the source status) and mirror the result.
    let completed_candidates: Vec<Uuid> = sqlx::query_scalar(
        r#"
        select r.id
          from pipeline_runs r
         where r.status in ('queued', 'running', 'applying')
           and r.updated_at < now() - make_interval(secs => $1)
           and exists (select 1 from jobs j where j.run_id = r.id)
           and not exists (
             select 1
               from jobs j
              where j.run_id = r.id
                and j.status <> 'succeeded'
           )
         for update
        "#,
    )
    .bind(older_than_seconds as f64)
    .fetch_all(&mut *tx)
    .await?;
    let completed_run_ids = transition_runs_tx(
        &mut tx,
        &completed_candidates,
        RunTransition::RecoveredSucceeded,
        None,
    )
    .await?;

    let failed_candidates: Vec<Uuid> = sqlx::query_scalar(concat!(
        r#"
        select r.id
          from pipeline_runs r
         where r.status in ('queued', 'running', 'applying')
           and r.updated_at < now() - make_interval(secs => $1)
           and not exists (
             select 1
               from jobs j
              where j.run_id = r.id
                and j.status in ("#,
        sql_active_job_statuses!(),
        r#")
           )
         for update
        "#
    ))
    .bind(older_than_seconds as f64)
    .fetch_all(&mut *tx)
    .await?;
    let failed_run_ids = transition_runs_tx(
        &mut tx,
        &failed_candidates,
        RunTransition::RecoveredFailed,
        Some(RECOVERED_STUCK_RUN_ERROR),
    )
    .await?;

    // #414: recovering a superseded run must not overwrite the inventory
    // status of the run the row now points at — the mirror is guarded on
    // `last_run_id`. #410: `complete` mirrors the Paperless completion tag.
    mirror_run_status_tx(&mut tx, &completed_run_ids, None).await?;
    mirror_run_status_tx(&mut tx, &failed_run_ids, Some(RECOVERED_STUCK_RUN_ERROR)).await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "operations.stuck_runs_recovered".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "completed": completed_run_ids.len(),
                "failed": failed_run_ids.len(),
                "completed_run_ids": completed_run_ids,
                "failed_run_ids": failed_run_ids
            })),
            metadata: Some(json!({ "older_than_seconds": older_than_seconds })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(RecoverySummary {
        stale_leases_requeued: 0,
        stuck_runs_failed: failed_run_ids.len() as i64,
        stuck_runs_completed: completed_run_ids.len() as i64,
    })
}

pub async fn append_audit(pool: &DbPool, event: AuditEventInput) -> Result<()> {
    let mut tx = pool.begin().await?;
    append_audit_tx(&mut tx, event).await?;
    tx.commit().await?;
    Ok(())
}

async fn append_audit_tx(
    tx: &mut Transaction<'_, Postgres>,
    mut event: AuditEventInput,
) -> Result<()> {
    // Fill request context before hashing so audit hash v2 binds it. #441
    apply_audit_request_context(&mut event);
    if let Some(value) = &mut event.before {
        redact_sensitive_json(value);
    }
    if let Some(value) = &mut event.after {
        redact_sensitive_json(value);
    }
    if let Some(value) = &mut event.metadata {
        redact_sensitive_json(value);
    }

    sqlx::query("select pg_advisory_xact_lock(hashtext('paperless_archivist_audit_events'))")
        .execute(&mut **tx)
        .await?;
    // Order by chain_position (a sequence assigned under this same advisory
    // lock), not created_at: the writing process's wall clock is unreliable
    // across pods, but chain_position monotonically follows append order. #254.
    let prev_event_hash: Option<String> = sqlx::query(
        r#"
        select event_hash
          from audit_events
         where event_hash is not null
         order by chain_position desc
         limit 1
        "#,
    )
    .fetch_optional(&mut **tx)
    .await?
    .map(|row| row.try_get("event_hash"))
    .transpose()?;
    let id = Uuid::now_v7();
    let created_at = postgres_timestamp_precision(Utc::now());
    let hash_version = AUDIT_HASH_VERSION_V2;
    let event_hash = audit_event_hash_v2(id, created_at, &prev_event_hash, &event);

    sqlx::query(
        r#"
        insert into audit_events (
          id, run_id, job_id, paperless_document_id, event_type, actor_type, actor_id,
          source_ip, user_agent,
          before, after, metadata, outcome, error_message, prev_event_hash, event_hash,
          hash_version, created_at, hash_created_at_ns_suffix
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
        "#,
    )
    .bind(id)
    .bind(event.run_id)
    .bind(event.job_id)
    .bind(event.paperless_document_id)
    .bind(&event.event_type)
    .bind(&event.actor_type)
    .bind(&event.actor_id)
    .bind(&event.source_ip)
    .bind(&event.user_agent)
    .bind(&event.before)
    .bind(&event.after)
    .bind(&event.metadata)
    .bind(&event.outcome)
    .bind(&event.error_message)
    .bind(&prev_event_hash)
    .bind(&event_hash)
    .bind(hash_version)
    .bind(created_at)
    .bind(0_i16)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

const AUDIT_HASH_VERSION_V1: i16 = 1;
const AUDIT_HASH_VERSION_V2: i16 = 2;

/// PostgreSQL `timestamp with time zone` stores microseconds. Canonicalize the
/// application timestamp before hashing and binding it so the hash input is
/// byte-for-byte reproducible after a database round trip on hosts whose clock
/// exposes nanoseconds.
fn postgres_timestamp_precision(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    timestamp
        .with_nanosecond(timestamp.nanosecond() / 1_000 * 1_000)
        .expect("a truncated nanosecond value is always valid")
}

fn audit_event_hash_v1(
    id: Uuid,
    created_at: DateTime<Utc>,
    prev_event_hash: &Option<String>,
    event: &AuditEventInput,
) -> String {
    let canonical = json!({
        "id": id,
        "created_at": created_at,
        "prev_event_hash": prev_event_hash,
        "run_id": event.run_id,
        "job_id": event.job_id,
        "paperless_document_id": event.paperless_document_id,
        "event_type": &event.event_type,
        "actor_type": &event.actor_type,
        "actor_id": &event.actor_id,
        "before": &event.before,
        "after": &event.after,
        "metadata": &event.metadata,
        "outcome": &event.outcome,
        "error_message": &event.error_message,
    });
    short_hash(&canonical.to_string())
}

fn audit_event_hash_v2(
    id: Uuid,
    created_at: DateTime<Utc>,
    prev_event_hash: &Option<String>,
    event: &AuditEventInput,
) -> String {
    let canonical = json!({
        "hash_version": AUDIT_HASH_VERSION_V2,
        "id": id,
        "created_at": created_at,
        "prev_event_hash": prev_event_hash,
        "run_id": event.run_id,
        "job_id": event.job_id,
        "paperless_document_id": event.paperless_document_id,
        "event_type": &event.event_type,
        "actor_type": &event.actor_type,
        "actor_id": &event.actor_id,
        "source_ip": &event.source_ip,
        "user_agent": &event.user_agent,
        "before": &event.before,
        "after": &event.after,
        "metadata": &event.metadata,
        "outcome": &event.outcome,
        "error_message": &event.error_message,
    });
    short_hash(&canonical.to_string())
}

fn audit_event_hash_for_version(
    hash_version: i16,
    id: Uuid,
    created_at: DateTime<Utc>,
    prev_event_hash: &Option<String>,
    event: &AuditEventInput,
) -> Option<String> {
    match hash_version {
        AUDIT_HASH_VERSION_V1 => Some(audit_event_hash_v1(id, created_at, prev_event_hash, event)),
        AUDIT_HASH_VERSION_V2 => Some(audit_event_hash_v2(id, created_at, prev_event_hash, event)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditHashVerification {
    Exact,
    LegacyTimestampPrecision(i16),
    Mismatch,
}

impl AuditHashVerification {
    fn validated_timestamp_suffix(self) -> Option<i16> {
        match self {
            Self::Exact => Some(0),
            Self::LegacyTimestampPrecision(suffix) => Some(suffix),
            Self::Mismatch => None,
        }
    }
}

/// Verify both canonical timestamps and timestamps produced by writers before
/// v1.17.0. Those writers hashed the host's nanosecond value, then PostgreSQL
/// stored only its microseconds. The missing three decimal digits have exactly
/// 1,000 possibilities, so we can validate the original hash without mutating
/// it or weakening verification of any other payload field.
fn verify_audit_event_hash(
    hash_version: i16,
    id: Uuid,
    stored_created_at: DateTime<Utc>,
    prev_event_hash: &Option<String>,
    event: &AuditEventInput,
    event_hash: &str,
    persisted_suffix: Option<i16>,
) -> Option<AuditHashVerification> {
    if let Some(suffix) = persisted_suffix {
        let Some(source_created_at) =
            stored_created_at.checked_add_signed(ChronoDuration::nanoseconds(i64::from(suffix)))
        else {
            return Some(AuditHashVerification::Mismatch);
        };
        let candidate = audit_event_hash_for_version(
            hash_version,
            id,
            source_created_at,
            prev_event_hash,
            event,
        )?;
        return Some(if candidate == event_hash {
            if suffix == 0 {
                AuditHashVerification::Exact
            } else {
                AuditHashVerification::LegacyTimestampPrecision(suffix)
            }
        } else {
            AuditHashVerification::Mismatch
        });
    }

    let exact =
        audit_event_hash_for_version(hash_version, id, stored_created_at, prev_event_hash, event)?;
    if exact == event_hash {
        return Some(AuditHashVerification::Exact);
    }

    for nanosecond_suffix in 1..1_000 {
        let Some(source_created_at) =
            stored_created_at.checked_add_signed(ChronoDuration::nanoseconds(nanosecond_suffix))
        else {
            return Some(AuditHashVerification::Mismatch);
        };
        let candidate = audit_event_hash_for_version(
            hash_version,
            id,
            source_created_at,
            prev_event_hash,
            event,
        )
        .expect("the validated hash version remains supported");
        if candidate == event_hash {
            return Some(AuditHashVerification::LegacyTimestampPrecision(
                nanosecond_suffix as i16,
            ));
        }
    }

    Some(AuditHashVerification::Mismatch)
}

const AUDIT_EVENT_COLUMNS: &str = "a.id, a.event_type, a.actor_type, a.actor_id, \
     a.paperless_document_id, a.outcome, a.error_message, a.created_at, a.metadata, \
     a.prev_event_hash, a.event_hash, a.hash_version, \
     (a.before is not null or a.after is not null) as has_changes, \
     (select u.username from users u \
       where a.actor_type = 'user' \
         and u.id = case when a.actor_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' \
                         then a.actor_id::uuid end) as actor_username";

/// Build the filtered, keyset-ordered audit list query (#448), prefixed with
/// `prefix` (empty for the real query; tests pass `explain ...` to assert the
/// plan). The ORDER BY matches the `(created_at desc, id desc)` trailing
/// columns of the 0057 keyset indexes, so every single-dimension filter is
/// served by one index scan with no sort.
pub fn audit_events_query_builder(
    prefix: &str,
    filter: &AuditEventFilter,
    limit: i64,
) -> QueryBuilder<Postgres> {
    let mut builder = QueryBuilder::<Postgres>::new(format!(
        "{prefix}select {AUDIT_EVENT_COLUMNS} from audit_events a where true"
    ));
    if let Some(actor_id) = &filter.actor_id {
        builder
            .push(" and a.actor_id = ")
            .push_bind(actor_id.as_str());
    }
    if let Some(actor_type) = &filter.actor_type {
        builder
            .push(" and a.actor_type = ")
            .push_bind(actor_type.as_str());
    }
    if let Some(document_id) = filter.paperless_document_id {
        builder
            .push(" and a.paperless_document_id = ")
            .push_bind(document_id);
    }
    match filter.event_types.as_slice() {
        [] => {}
        // `=` (not `= any`) keeps the (event_type, created_at, id) index
        // ordered for the common single-type filter.
        [single] => {
            builder
                .push(" and a.event_type = ")
                .push_bind(single.as_str());
        }
        many => {
            builder
                .push(" and a.event_type = any(")
                .push_bind(many.to_vec())
                .push(")");
        }
    }
    if let Some(outcome) = &filter.outcome {
        builder
            .push(" and a.outcome = ")
            .push_bind(outcome.as_str());
    }
    if let Some(from) = filter.from {
        builder.push(" and a.created_at >= ").push_bind(from);
    }
    if let Some(to) = filter.to {
        builder.push(" and a.created_at < ").push_bind(to);
    }
    if let Some((created_at, id)) = filter.before {
        builder
            .push(" and (a.created_at, a.id) < (")
            .push_bind(created_at)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    builder
        .push(" order by a.created_at desc, a.id desc limit ")
        .push_bind(limit);
    builder
}

fn audit_event_record_from_row(row: &PgRow) -> Result<AuditEventRecord> {
    Ok(AuditEventRecord {
        id: row.try_get("id")?,
        event_type: row.try_get("event_type")?,
        actor_type: row.try_get("actor_type")?,
        actor_id: row.try_get("actor_id")?,
        paperless_document_id: row.try_get("paperless_document_id")?,
        outcome: row.try_get("outcome")?,
        error_message: row.try_get("error_message")?,
        created_at: row.try_get("created_at")?,
        metadata: row.try_get("metadata")?,
        prev_event_hash: row.try_get("prev_event_hash")?,
        event_hash: row.try_get("event_hash")?,
        hash_version: row.try_get("hash_version")?,
        actor_username: row.try_get("actor_username")?,
        has_changes: row.try_get("has_changes")?,
    })
}

/// Newest-first page of the audit log matching `filter` (#448).
pub async fn list_audit_events(
    pool: &DbPool,
    filter: &AuditEventFilter,
    limit: i64,
) -> Result<Vec<AuditEventRecord>> {
    let rows = audit_events_query_builder("", filter, limit)
        .build()
        .fetch_all(pool)
        .await?;
    rows.iter().map(audit_event_record_from_row).collect()
}

/// One audit event including before/after snapshots (#448). The snapshots
/// are returned as stored; the API redacts credential-like keys.
pub async fn get_audit_event(pool: &DbPool, id: Uuid) -> Result<Option<AuditEventDetail>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "select {AUDIT_EVENT_COLUMNS}, a.run_id, a.job_id, a.before, a.after, \
         a.source_ip, a.user_agent from audit_events a where a.id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(AuditEventDetail {
        event: audit_event_record_from_row(&row)?,
        run_id: row.try_get("run_id")?,
        job_id: row.try_get("job_id")?,
        before: row.try_get("before")?,
        after: row.try_get("after")?,
        source_ip: row.try_get("source_ip")?,
        user_agent: row.try_get("user_agent")?,
    }))
}

async fn verify_audit_integrity_tx(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<AuditIntegrityReport> {
    let coverage = sqlx::query(
        r#"
        select count(*) filter (where event_hash is null)::bigint as legacy_events,
               count(*) filter (where event_hash is not null and hash_version = 1)::bigint as v1_events,
               count(*) filter (where event_hash is not null and hash_version = 2)::bigint as v2_events
          from audit_events
        "#,
    )
    .fetch_one(&mut **tx)
    .await?;
    let legacy_events: i64 = coverage.try_get("legacy_events")?;
    let v1_events: i64 = coverage.try_get("v1_events")?;
    let v2_events: i64 = coverage.try_get("v2_events")?;

    let mut checked_events = 0_i64;
    let mut legacy_precision_events = 0_i64;
    let mut latest_event_hash: Option<String> = None;
    let mut last_chain_position = 0_i64;
    loop {
        // Bounded pages keep memory stable while the transaction-level
        // advisory lock provides one cluster-wide verifier/backfill flight.
        let rows = sqlx::query(
            r#"
            select id, run_id, job_id, paperless_document_id, event_type, actor_type, actor_id,
                   source_ip, user_agent, before, after, metadata, outcome, error_message,
                   created_at, prev_event_hash, event_hash, hash_version,
                   hash_created_at_ns_suffix, chain_position
              from audit_events
             where event_hash is not null
               and chain_position > $1
             order by chain_position asc
             limit 256
            "#,
        )
        .bind(last_chain_position)
        .fetch_all(&mut **tx)
        .await?;
        if rows.is_empty() {
            break;
        }

        for row in rows {
            let id: Uuid = row.try_get("id")?;
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            let prev_event_hash: Option<String> = row.try_get("prev_event_hash")?;
            let event_hash: String = row.try_get("event_hash")?;
            let hash_version: Option<i16> = row.try_get("hash_version")?;
            let persisted_suffix: Option<i16> = row.try_get("hash_created_at_ns_suffix")?;
            last_chain_position = row.try_get("chain_position")?;
            if let Some(expected_prev) = &latest_event_hash
                && prev_event_hash.as_ref() != Some(expected_prev)
            {
                return Ok(AuditIntegrityReport {
                    ok: false,
                    checked_events,
                    legacy_events,
                    v1_events,
                    v2_events,
                    legacy_precision_events,
                    latest_event_hash,
                    broken_event_id: Some(id),
                    broken_reason: Some("previous event hash does not match chain".to_owned()),
                });
            }
            let event = AuditEventInput {
                run_id: row.try_get("run_id")?,
                job_id: row.try_get("job_id")?,
                paperless_document_id: row.try_get("paperless_document_id")?,
                event_type: row.try_get("event_type")?,
                actor_type: row.try_get("actor_type")?,
                actor_id: row.try_get("actor_id")?,
                before: row.try_get("before")?,
                after: row.try_get("after")?,
                metadata: row.try_get("metadata")?,
                outcome: row.try_get("outcome")?,
                error_message: row.try_get("error_message")?,
                source_ip: row.try_get("source_ip")?,
                user_agent: row.try_get("user_agent")?,
            };
            let Some(version) = hash_version else {
                return Ok(AuditIntegrityReport {
                    ok: false,
                    checked_events,
                    legacy_events,
                    v1_events,
                    v2_events,
                    legacy_precision_events,
                    latest_event_hash,
                    broken_event_id: Some(id),
                    broken_reason: Some("unsupported or missing audit hash version".to_owned()),
                });
            };
            let hash_verification = if persisted_suffix.is_some() {
                verify_audit_event_hash(
                    version,
                    id,
                    created_at,
                    &prev_event_hash,
                    &event,
                    &event_hash,
                    persisted_suffix,
                )
            } else {
                // The one-time 999-suffix discovery can hash large JSON payloads
                // repeatedly. Keep it off Tokio's async executor; the validated
                // suffix is persisted below so later scans perform one hash.
                let previous = prev_event_hash.clone();
                let expected_hash = event_hash.clone();
                tokio::task::spawn_blocking(move || {
                    verify_audit_event_hash(
                        version,
                        id,
                        created_at,
                        &previous,
                        &event,
                        &expected_hash,
                        None,
                    )
                })
                .await
                .context("join audit timestamp precision verification task")?
            };
            let Some(hash_verification) = hash_verification else {
                return Ok(AuditIntegrityReport {
                    ok: false,
                    checked_events,
                    legacy_events,
                    v1_events,
                    v2_events,
                    legacy_precision_events,
                    latest_event_hash,
                    broken_event_id: Some(id),
                    broken_reason: Some("unsupported or missing audit hash version".to_owned()),
                });
            };
            if hash_verification == AuditHashVerification::Mismatch {
                return Ok(AuditIntegrityReport {
                    ok: false,
                    checked_events,
                    legacy_events,
                    v1_events,
                    v2_events,
                    legacy_precision_events,
                    latest_event_hash,
                    broken_event_id: Some(id),
                    broken_reason: Some("event hash does not match event payload".to_owned()),
                });
            }
            if matches!(
                hash_verification,
                AuditHashVerification::LegacyTimestampPrecision(_)
            ) {
                legacy_precision_events += 1;
            }
            if persisted_suffix.is_none() {
                let validated_suffix = hash_verification
                    .validated_timestamp_suffix()
                    .expect("a verified event always has a timestamp suffix");
                sqlx::query(
                    r#"
                update audit_events
                   set hash_created_at_ns_suffix = $2
                 where id = $1
                   and hash_created_at_ns_suffix is null
                "#,
                )
                .bind(id)
                .bind(validated_suffix)
                .execute(&mut **tx)
                .await?;
            }
            checked_events += 1;
            latest_event_hash = Some(event_hash);
        }
    }

    Ok(AuditIntegrityReport {
        ok: true,
        checked_events,
        legacy_events,
        v1_events,
        v2_events,
        legacy_precision_events,
        latest_event_hash,
        broken_event_id: None,
        broken_reason: None,
    })
}

async fn verify_audit_integrity_session(
    connection: &mut PgConnection,
) -> Result<AuditIntegrityReport> {
    // The session-level advisory lock is already held before this transaction
    // starts, so REPEATABLE READ cannot capture a stale pre-lock snapshot.
    let mut tx = connection.begin().await?;
    sqlx::query("set transaction isolation level repeatable read")
        .execute(&mut *tx)
        .await?;

    let result = verify_audit_integrity_tx(&mut tx).await;
    match result {
        Ok(report) => {
            tx.commit().await?;
            Ok(report)
        }
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

pub async fn verify_audit_integrity(pool: &DbPool) -> Result<AuditIntegrityReport> {
    let _process_guard = AUDIT_INTEGRITY_VERIFY_LOCK.lock().await;
    let mut connection = pool.acquire().await?;
    // Session advisory locks survive a transaction. Closing instead of
    // returning this connection to the pool guarantees lock release even if
    // the request future is cancelled before the explicit unlock below.
    connection.close_on_drop();
    sqlx::query("select pg_advisory_lock(hashtext('paperless_archivist_audit_integrity_verify'))")
        .execute(&mut *connection)
        .await?;

    let result = verify_audit_integrity_session(&mut connection).await;
    let unlock = sqlx::query_scalar::<_, bool>(
        "select pg_advisory_unlock(hashtext('paperless_archivist_audit_integrity_verify'))",
    )
    .fetch_one(&mut *connection)
    .await;

    match (result, unlock) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(_), Ok(false)) => Err(anyhow!("audit integrity advisory lock was not held")),
        (Ok(report), Ok(true)) => Ok(report),
    }
}

pub async fn apply_security_retention(
    pool: &DbPool,
    settings: &RuntimeSettings,
    actor_id: Uuid,
) -> Result<RetentionResult> {
    let security = settings.clone().normalized().security;
    let now = Utc::now();
    let artifact_cutoff = now - ChronoDuration::days(security.ai_artifact_retention_days);
    let audit_cutoff = now - ChronoDuration::days(security.audit_retention_days);
    let runs_cutoff = now - ChronoDuration::days(security.runs_retention_days);

    // ocr_page_cache holds the full OCR text of every processed page and must
    // not outlive the artifact retention. Deleted in bounded batches outside
    // the audit transaction so a years-old backlog can't hold one giant lock.
    let mut ocr_page_cache_deleted: i64 = 0;
    loop {
        let deleted = sqlx::query(
            r#"
            delete from ocr_page_cache
             where ctid in (
               select ctid from ocr_page_cache
                where created_at < $1
                limit 5000
             )
            "#,
        )
        .bind(artifact_cutoff)
        .execute(pool)
        .await?
        .rows_affected() as i64;
        ocr_page_cache_deleted += deleted;
        if deleted < 5000 {
            break;
        }
    }

    // dashboard_snapshots accumulate one row every ~5 minutes forever (#273).
    // Prune to the audit retention window in bounded batches, like the OCR
    // cache above.
    let mut dashboard_snapshots_deleted: i64 = 0;
    loop {
        let deleted = sqlx::query(
            r#"
            delete from dashboard_snapshots
             where ctid in (
               select ctid from dashboard_snapshots
                where captured_at < $1
                limit 5000
             )
            "#,
        )
        .bind(audit_cutoff)
        .execute(pool)
        .await?
        .rows_affected() as i64;
        dashboard_snapshots_deleted += deleted;
        if deleted < 5000 {
            break;
        }
    }

    // Batch the artifact/audit deletes too (#275): a single unbounded DELETE
    // holds a long lock and bloats one transaction on a large backlog. The
    // audit chain tolerates a truncated prefix, so deleting the old rows
    // before appending the retention event keeps the chain consistent.
    let mut ai_artifacts_deleted: i64 = 0;
    loop {
        let deleted = sqlx::query(
            r#"
            delete from ai_artifacts
             where ctid in (
               select ctid from ai_artifacts where created_at < $1 limit 5000
             )
            "#,
        )
        .bind(artifact_cutoff)
        .execute(pool)
        .await?
        .rows_affected() as i64;
        ai_artifacts_deleted += deleted;
        if deleted < 5000 {
            break;
        }
    }
    // Delete a true chain_position PREFIX, not a created_at prefix (#285). The
    // chain verifies in chain_position order (#254), so deleting by created_at
    // under cross-pod clock skew could remove a lower-chain_position row while
    // keeping a higher one, leaving a hole that verify reports as a broken
    // chain. Compute the smallest chain_position we must keep (the oldest row
    // still inside the retention window) and delete everything strictly below
    // it; if nothing is inside the window the whole table is expired.
    let keep_boundary: i64 = sqlx::query_scalar(
        "select coalesce(min(chain_position), $2) from audit_events where created_at >= $1",
    )
    .bind(audit_cutoff)
    .bind(i64::MAX)
    .fetch_one(pool)
    .await?;
    let mut audit_events_deleted: i64 = 0;
    loop {
        let deleted = sqlx::query(
            r#"
            delete from audit_events
             where ctid in (
               select ctid from audit_events where chain_position < $1 limit 5000
             )
            "#,
        )
        .bind(keep_boundary)
        .execute(pool)
        .await?
        .rows_affected() as i64;
        audit_events_deleted += deleted;
        if deleted < 5000 {
            break;
        }
    }

    // pipeline_runs were the last unbounded store (#310): ~400 runs/day with
    // no pruning at all. Delete TERMINAL runs only — active statuses (queued/
    // running/waiting_review/applying) are never touched, so in-flight work
    // and open reviews keep their run. Jobs and ai_artifacts cascade with the
    // run (artifacts on a pruned run are months past their own retention by
    // default); review_items and audit_events keep their rows with run_id
    // nulled, and document_inventory.last_run_id nulls out — all four FK
    // rules flipped/added in migration 0041 BEFORE this code first ran.
    let mut pipeline_runs_deleted: i64 = 0;
    loop {
        let deleted = sqlx::query(concat!(
            r#"
            delete from pipeline_runs
             where ctid in (
               select ctid from pipeline_runs
                where status in ("#,
            sql_terminal_run_statuses!(),
            r#")
                  and created_at < $1
                limit 5000
             )
            "#
        ))
        .bind(runs_cutoff)
        .execute(pool)
        .await?
        .rows_affected() as i64;
        pipeline_runs_deleted += deleted;
        if deleted < 5000 {
            break;
        }
    }

    let mut tx = pool.begin().await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "audit.retention_applied".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({
                "audit_retention_days": security.audit_retention_days,
                "ai_artifact_retention_days": security.ai_artifact_retention_days,
                "runs_retention_days": security.runs_retention_days,
                "audit_events_deleted": audit_events_deleted,
                "ai_artifacts_deleted": ai_artifacts_deleted,
                "ocr_page_cache_deleted": ocr_page_cache_deleted,
                "dashboard_snapshots_deleted": dashboard_snapshots_deleted,
                "pipeline_runs_deleted": pipeline_runs_deleted
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;

    Ok(RetentionResult {
        audit_events_deleted,
        ai_artifacts_deleted,
        ocr_page_cache_deleted,
        pipeline_runs_deleted,
    })
}

async fn no_remaining_jobs_tx(tx: &mut Transaction<'_, Postgres>, run_id: Uuid) -> Result<bool> {
    let row = sqlx::query(concat!(
        r#"
        select not exists(
          select 1 from jobs
           where run_id = $1
             and status in ("#,
        sql_active_job_statuses!(),
        r#")
        ) as done
        "#
    ))
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await?;
    row.try_get("done").context("read run completion state")
}

async fn set_inventory_stage_status_tx(
    tx: &mut Transaction<'_, Postgres>,
    paperless_document_id: i32,
    stage: Stage,
    status: &str,
    error: Option<&str>,
    needs_review: bool,
    run_id: Option<Uuid>,
) -> Result<()> {
    let column = status_column_for_stage(stage)?;
    let sql = format!(
        r#"
        update document_inventory
           set {column} = $2,
               current_run_status = case when $2 = 'failed' then 'failed' else current_run_status end,
               last_error = $3,
               needs_review = $4,
               last_run_id = coalesce($5, last_run_id),
               updated_at = now()
         where paperless_document_id = $1
        "#
    );
    // SAFETY: `sql` is a static literal built above with no user-controlled
    // interpolation; only bind parameters carry caller data.
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(paperless_document_id)
        .bind(status)
        .bind(error)
        .bind(needs_review)
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Thin wrapper around [`Stage::inventory_status_column`] that surfaces a typed
/// error when a caller passes an orchestration-only stage. The returned string is
/// a static literal — callers may safely interpolate it into SQL.
fn status_column_for_stage(stage: Stage) -> Result<&'static str> {
    stage
        .inventory_status_column()
        .ok_or_else(|| anyhow!("stage does not map to inventory status: {stage}"))
}

/// A one-shot worker startup repair, identified by `name` and `version`
/// (#443). The worker runs a repair at most once per (name, version): the
/// marker in `startup_repairs` (migration 0054) is written after a
/// successful pass. Bump `version` when a repair's logic changes and it has
/// to run once more (e.g. a raised num_ctx floor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupRepair {
    pub name: &'static str,
    pub version: i32,
}

impl StartupRepair {
    /// `bump_vision_num_ctx_if_too_small`.
    pub const VISION_NUM_CTX_FLOOR: StartupRepair = StartupRepair {
        name: "ollama_vision_num_ctx_floor",
        version: 1,
    };
    /// `bump_text_num_ctx_if_too_small`.
    pub const TEXT_NUM_CTX_FLOOR: StartupRepair = StartupRepair {
        name: "ollama_text_num_ctx_floor",
        version: 1,
    };
    /// `requeue_vision_crashed_jobs` (only recorded while the runtime
    /// setting `requeue_vision_crashes_on_startup` enables it).
    pub const VISION_CRASH_REQUEUE: StartupRepair = StartupRepair {
        name: "vision_crash_requeue",
        version: 1,
    };
    /// `backfill_metadata_stage_for_ocr_only_runs`.
    pub const METADATA_STAGE_BACKFILL: StartupRepair = StartupRepair {
        name: "metadata_stage_backfill",
        version: 1,
    };
    /// `rebalance_backfilled_metadata_priorities`.
    pub const METADATA_PRIORITY_REBALANCE: StartupRepair = StartupRepair {
        name: "metadata_priority_rebalance",
        version: 1,
    };
    /// `reset_stuck_running_pipeline_runs`.
    pub const STUCK_RUNNING_RUNS_RESET: StartupRepair = StartupRepair {
        name: "stuck_running_runs_reset",
        version: 1,
    };

    pub const ALL: &'static [StartupRepair] = &[
        StartupRepair::VISION_NUM_CTX_FLOOR,
        StartupRepair::TEXT_NUM_CTX_FLOOR,
        StartupRepair::VISION_CRASH_REQUEUE,
        StartupRepair::METADATA_STAGE_BACKFILL,
        StartupRepair::METADATA_PRIORITY_REBALANCE,
        StartupRepair::STUCK_RUNNING_RUNS_RESET,
    ];
}

/// Whether `repair` already ran at its current version. #443
pub async fn startup_repair_applied(pool: &DbPool, repair: StartupRepair) -> Result<bool> {
    sqlx::query_scalar(
        "select exists (select 1 from startup_repairs where name = $1 and version = $2)",
    )
    .bind(repair.name)
    .bind(repair.version)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Record that `repair` ran successfully, with the running `app_version` and
/// the repair's summary as `details`, plus a `worker.startup_repair_applied`
/// audit event in the same transaction. Returns `false` when another replica
/// recorded the marker first (both passes are idempotent, so a concurrent
/// double run is harmless). #443
pub async fn record_startup_repair(
    pool: &DbPool,
    repair: StartupRepair,
    app_version: &str,
    details: Value,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        r#"
        insert into startup_repairs (name, version, app_version, details)
        values ($1, $2, $3, $4)
        on conflict (name, version) do nothing
        "#,
    )
    .bind(repair.name)
    .bind(repair.version)
    .bind(app_version)
    .bind(&details)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !inserted {
        tx.rollback().await?;
        return Ok(false);
    }
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "worker.startup_repair_applied".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(details),
            metadata: Some(json!({
                "repair": repair.name,
                "version": repair.version,
                "app_version": app_version,
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// SQL `ilike` patterns matching the vision-runtime-crash error-message
/// signatures (`GGML_ASSERT(...)`, "runner process no longer running", "signal
/// arrived during cgo execution"). Kept in sync with
/// `archivist_worker::is_vision_model_runtime_crash`.
pub const VISION_CRASH_SQL_PATTERNS: &[&str] = &[
    "%GGML_ASSERT%",
    "%runner process no longer running%",
    "%signal arrived during cgo execution%",
];

/// Summary of a one-shot startup requeue pass. Helpful for log lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisionCrashRequeueSummary {
    pub jobs_requeued: i64,
}

/// One-shot, idempotent helper run on worker startup that lifts `failed` OCR-stage jobs
/// whose error message matches the vision-runtime-crash signature back into the queue so
/// they get one more attempt under the new fallback machinery. We bump `max_attempts` by
/// one (rather than resetting `attempts`) so a job that has already burned through its
/// retry budget on the broken primary model still has one fresh attempt to run under the
/// fallback, but does not get an unbounded budget.
///
/// Also flips the matching `pipeline_runs` row back to `queued`, and resets the
/// inventory stage status, so the dashboard reflects the second chance.
///
/// All writes happen in a single transaction; either the newest matching run per
/// document is requeued or none are. Returns the number of jobs that were lifted.
pub async fn requeue_vision_crashed_jobs(pool: &DbPool) -> Result<VisionCrashRequeueSummary> {
    let mut tx = pool.begin().await?;
    // A failed run becomes active again below. Discover candidate documents
    // without locking rows, then take the same canonical document locks used
    // by run creation before changing jobs or acquiring the audit-chain lock.
    let candidate_document_ids = sqlx::query_scalar::<_, i32>(concat!(
        r#"
        select distinct j.paperless_document_id
          from jobs j
          join pipeline_runs pr on pr.id = j.run_id
         where j.status = 'failed'
           and j.stage = 'ocr'
           and pr.status = 'failed'
           and (
                j.error_message ilike $1
             or j.error_message ilike $2
             or j.error_message ilike $3
           )
           -- #406: one-shot per job; a crash-looping pod must not raise
           -- max_attempts again on every start.
           and not coalesce((j.payload ->> 'vision_requeued')::boolean, false)
           and not exists (
             select 1
               from pipeline_runs active
              where active.paperless_document_id = j.paperless_document_id
                and active.id <> j.run_id
                and active.status in ("#,
        sql_active_run_statuses!(),
        r#")
           )
         order by j.paperless_document_id
        "#
    ))
    .bind(VISION_CRASH_SQL_PATTERNS[0])
    .bind(VISION_CRASH_SQL_PATTERNS[1])
    .bind(VISION_CRASH_SQL_PATTERNS[2])
    .fetch_all(&mut *tx)
    .await?;
    lock_active_run_documents_tx(&mut tx, &candidate_document_ids).await?;

    let rows = sqlx::query(concat!(
        r#"
        with eligible_runs as (
          select distinct on (j.paperless_document_id)
                 j.run_id,
                 j.paperless_document_id
            from jobs j
            join pipeline_runs pr on pr.id = j.run_id
           where j.status = 'failed'
             and j.stage = 'ocr'
             and pr.status = 'failed'
             and j.paperless_document_id = any($4)
             and (
                  j.error_message ilike $1
               or j.error_message ilike $2
               or j.error_message ilike $3
             )
             and not coalesce((j.payload ->> 'vision_requeued')::boolean, false)
             and not exists (
               select 1
                 from pipeline_runs active
                where active.paperless_document_id = j.paperless_document_id
                  and active.id <> j.run_id
                  and active.status in ("#,
        sql_active_run_statuses!(),
        r#")
             )
           order by j.paperless_document_id, pr.created_at desc, pr.id desc
        ),
        crashed as (
          select j.id, j.run_id, j.paperless_document_id, j.max_attempts
            from jobs j
            join eligible_runs eligible on eligible.run_id = j.run_id
           where j.status = 'failed'
             and j.stage = 'ocr'
             and (
                  j.error_message ilike $1
               or j.error_message ilike $2
               or j.error_message ilike $3
             )
             and not coalesce((j.payload ->> 'vision_requeued')::boolean, false)
           for update of j
        )
        update jobs j
           set status = 'queued',
               max_attempts = j.max_attempts + 1,
               payload = j.payload || '{"vision_requeued": true}'::jsonb,
               run_after = now(),
               lease_owner = null,
               lease_until = null,
               error_message = null,
               updated_at = now()
          from crashed
         where j.id = crashed.id
        returning j.id, j.run_id, j.paperless_document_id
        "#
    ))
    .bind(VISION_CRASH_SQL_PATTERNS[0])
    .bind(VISION_CRASH_SQL_PATTERNS[1])
    .bind(VISION_CRASH_SQL_PATTERNS[2])
    .bind(&candidate_document_ids)
    .fetch_all(&mut *tx)
    .await?;

    if rows.is_empty() {
        tx.commit().await?;
        return Ok(VisionCrashRequeueSummary { jobs_requeued: 0 });
    }

    let run_ids: Vec<Uuid> = rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("run_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let document_ids: Vec<i32> = rows
        .iter()
        .map(|row| row.try_get::<i32, _>("paperless_document_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let job_ids: Vec<Uuid> = rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    transition_runs_tx(&mut tx, &run_ids, RunTransition::VisionCrashRequeued, None).await?;

    // #406: `fail_job` cancelled the run's later-stage siblings (metadata)
    // when OCR failed. Restore them too, otherwise the OCR job looks like the
    // run's last active job, sets the global completion tag and metadata
    // never runs for this document.
    sqlx::query(
        r#"
        update jobs
           set status = 'queued',
               run_after = now(),
               lease_owner = null,
               lease_until = null,
               updated_at = now()
         where run_id = any($1)
           and status = 'cancelled'
        "#,
    )
    .bind(&run_ids)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        update document_inventory
           set ocr_status = 'queued',
               updated_at = now()
         where paperless_document_id = any($1)
           and ocr_status = 'failed'
        "#,
    )
    .bind(&document_ids)
    .execute(&mut *tx)
    .await?;
    mirror_run_status_tx(&mut tx, &run_ids, None).await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "worker.vision_crash_jobs_requeued".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "count": job_ids.len(),
                "job_ids": job_ids,
            })),
            metadata: Some(json!({
                "trigger": "startup_one_shot",
                "patterns": VISION_CRASH_SQL_PATTERNS,
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(VisionCrashRequeueSummary {
        jobs_requeued: job_ids.len() as i64,
    })
}

/// Summary of a one-shot metadata-stage backfill pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataStageBackfillSummary {
    pub runs_updated: i64,
    pub jobs_inserted: i64,
}

/// One-shot, idempotent helper run on worker startup that lifts the
/// historical OCR-only `pipeline_runs` (created before v1.5.4 by trigger
/// polling against documents tagged only with the OCR trigger) up to include
/// the consolidated `metadata` stage as well. Without this, those runs
/// terminate after OCR with no Title/Correspondent/Tags suggestion ever
/// being produced, so the Review queue is full of `{"content": "..."}`-only
/// review items that the operator cannot meaningfully act on.
///
/// What this does, in one transaction:
///   * Find at most one relevant `pipeline_runs` row per document whose
///     `stages` jsonb array contains "ocr" but does NOT contain "metadata",
///     and which does not already have a `metadata`-stage `jobs` row. Prefer
///     the active run, otherwise use the newest succeeded run.
///   * Append "metadata" to `pipeline_runs.stages`.
///   * Insert a queued `metadata` job for the run with `stage_priority=20`
///     so it sequences AFTER the OCR job (priority 10).
///   * For runs that were already `succeeded` (OCR is done and either auto-
///     applied or never produced a review): flip status back to `queued`
///     and clear `finished_at`, so the worker re-picks the run up to claim
///     the new metadata job. For runs in `waiting_review`/`queued`/`running`,
///     status is left alone — the natural cascade in `mark_review_auto_applied`
///     / OCR completion will flip the run back to `queued` once the OCR side
///     settles, and the metadata job becomes claimable from the existing
///     run-still-has-work path.
///
/// Idempotent: re-running this finds nothing to do because of the
/// `NOT EXISTS metadata job` predicate. Safe to run on every worker startup.
pub async fn backfill_metadata_stage_for_ocr_only_runs(
    pool: &DbPool,
) -> Result<MetadataStageBackfillSummary> {
    let mut tx = pool.begin().await?;

    // Succeeded OCR-only runs are reactivated below. Coordinate that
    // terminal-to-active transition with concurrent creates, and take every
    // document lock before any row or audit-chain lock.
    let candidate_document_ids = sqlx::query_scalar::<_, i32>(concat!(
        r#"
        select distinct pr.paperless_document_id
          from pipeline_runs pr
         where pr.stages @> '["ocr"]'::jsonb
           and not (pr.stages @> '["metadata"]'::jsonb)
           and pr.status in ("#,
        sql_active_run_statuses!(),
        r#", 'succeeded')
           and not exists (
             select 1 from jobs j
              where j.run_id = pr.id and j.stage = 'metadata'
           )
           and (
             pr.status <> 'succeeded'
             or not exists (
               select 1
                 from pipeline_runs active
                where active.paperless_document_id = pr.paperless_document_id
                  and active.id <> pr.id
                  and active.status in ("#,
        sql_active_run_statuses!(),
        r#")
             )
           )
         order by pr.paperless_document_id
        "#
    ))
    .fetch_all(&mut *tx)
    .await?;
    lock_active_run_documents_tx(&mut tx, &candidate_document_ids).await?;

    // Step 1: identify the target runs. Lock them for update so a parallel
    // worker doesn't race us into queueing duplicate metadata jobs.
    let target_rows = sqlx::query(concat!(
        r#"
        with ranked_targets as (
          select pr.id,
                 row_number() over (
                   partition by pr.paperless_document_id
                   order by
                     case
                       when pr.status in ("#,
        sql_active_run_statuses!(),
        r#")
                       then 0 else 1
                     end,
                     pr.created_at desc,
                     pr.id desc
                 ) as document_rank
            from pipeline_runs pr
           where pr.paperless_document_id = any($1)
             and pr.stages @> '["ocr"]'::jsonb
             and not (pr.stages @> '["metadata"]'::jsonb)
             and pr.status in ("#,
        sql_active_run_statuses!(),
        r#", 'succeeded')
             and not exists (
               select 1 from jobs j
                where j.run_id = pr.id and j.stage = 'metadata'
             )
             and (
               pr.status <> 'succeeded'
               or not exists (
                 select 1
                   from pipeline_runs active
                  where active.paperless_document_id = pr.paperless_document_id
                    and active.id <> pr.id
                    and active.status in ("#,
        sql_active_run_statuses!(),
        r#")
               )
             )
        )
        select pr.id as run_id,
               pr.paperless_document_id,
               pr.status as current_status
          from pipeline_runs pr
          join ranked_targets target
            on target.id = pr.id
           and target.document_rank = 1
        for update of pr skip locked
        "#
    ))
    .bind(&candidate_document_ids)
    .fetch_all(&mut *tx)
    .await?;

    if target_rows.is_empty() {
        tx.commit().await?;
        return Ok(MetadataStageBackfillSummary::default());
    }

    let run_ids: Vec<Uuid> = target_rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("run_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // Step 2: append "metadata" to the stages array and reset succeeded
    // runs back to queued so the worker re-claims the new metadata job.
    sqlx::query(
        r#"
        update pipeline_runs
           set stages = (
                 select coalesce(jsonb_agg(s order by s_order), '[]'::jsonb)
                   from (
                     select 'ocr'      as s, 1 as s_order
                     union all
                     select 'metadata' as s, 2 as s_order
                   ) ordered
               ),
               updated_at = now()
         where id = any($1)
        "#,
    )
    .bind(&run_ids)
    .execute(&mut *tx)
    .await?;
    // Succeeded runs reopen through the transition table (#439); active runs
    // are left alone by its `succeeded`-only source guard.
    transition_runs_tx(&mut tx, &run_ids, RunTransition::MetadataBackfilled, None).await?;

    // Step 3: insert a queued metadata job per run, with stage_priority=20
    // so it claims AFTER the OCR job (stage_priority=10). The cross-run
    // `priority` is INHERITED from the same run's OCR job — this is the
    // v1.5.6 fix for the v1.5.4 backfill bug where metadata jobs were
    // priced with `1_000_000 - document_id` (~993K-999K) while legacy
    // trigger-polling OCR jobs sit at priority=10. Since claim_jobs orders
    // by priority ASC then stage_priority ASC, mispriced metadata never
    // claimed until every OCR job globally was done. Inheriting the
    // sibling OCR's priority keeps the cross-run ordering exactly as the
    // operator who queued the run intended, and the stage_priority=20
    // alone guarantees OCR-before-metadata ordering within the run.
    let inserted = sqlx::query(
        r#"
        insert into jobs (run_id, paperless_document_id, stage, status, payload)
        select pr.id,
               pr.paperless_document_id,
               'metadata',
               'queued',
               jsonb_build_object(
                 'priority', coalesce(
                   (ocr.payload ->> 'priority')::bigint,
                   100
                 ),
                 'stage_priority', 20,
                 'backfill', true
               )
          from pipeline_runs pr
          join jobs ocr on ocr.run_id = pr.id and ocr.stage = 'ocr'
         where pr.id = any($1)
           and not exists (
             select 1 from jobs m
              where m.run_id = pr.id and m.stage = 'metadata'
           )
        "#,
    )
    .bind(&run_ids)
    .execute(&mut *tx)
    .await?;
    let jobs_inserted = inserted.rows_affected() as i64;

    // Step 4: mirror the (possibly reopened) runs onto
    // document_inventory.current_run_status so the dashboard status badges
    // match the new pipeline_runs state. #303/#410/#414
    mirror_run_status_tx(&mut tx, &run_ids, None).await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "worker.metadata_stage_backfilled".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "runs_updated": run_ids.len(),
                "jobs_inserted": jobs_inserted,
            })),
            metadata: Some(json!({
                "trigger": "startup_one_shot",
                "reason": "ocr_only_runs_missing_metadata_stage",
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;

    Ok(MetadataStageBackfillSummary {
        runs_updated: run_ids.len() as i64,
        jobs_inserted,
    })
}

/// Summary of a one-shot bump pass for the vision `num_ctx` runtime setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisionNumCtxBumpSummary {
    pub previous: Option<i64>,
    pub current: i64,
    pub bumped: bool,
}

/// One-shot, idempotent helper that raises `ai.ollama_vision_num_ctx` from
/// any value <= 16384 to 32768. v1.5.1 fixed glm-ocr GGML_ASSERT crashes by
/// pinning num_ctx=16384 on Ollama vision calls — that floor worked for
/// single-page renders but is still too small for some multi-page or
/// high-DPI documents under realistic prod load (137 OCR jobs burned through
/// their retry budget despite num_ctx=16384). Doubling to 32768 gives the
/// vision model headroom for the page-token blow-up cases without forcing
/// operators to dig through Settings to find the dial. Operators who have
/// already set a higher value get left alone.
pub async fn bump_vision_num_ctx_if_too_small(pool: &DbPool) -> Result<VisionNumCtxBumpSummary> {
    const FLOOR: i64 = 32768;
    const PREVIOUS_FIX_CEILING: i64 = 16384;

    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        select (value #>> '{ai,ollama_vision_num_ctx}')::bigint as num_ctx
          from settings
         where key = 'runtime'
         for update
        "#,
    )
    .fetch_optional(&mut *tx)
    .await?;

    let previous: Option<i64> = row.and_then(|r| r.try_get("num_ctx").ok());

    // Only bump if the current value is <= the v1.5.1 fix ceiling. Don't
    // touch operator overrides that already raised it past 16384.
    let should_bump = match previous {
        Some(v) => v <= PREVIOUS_FIX_CEILING,
        None => true,
    };

    if !should_bump {
        tx.commit().await?;
        return Ok(VisionNumCtxBumpSummary {
            previous,
            current: previous.unwrap_or(FLOOR),
            bumped: false,
        });
    }

    sqlx::query(
        r#"
        update settings
           set value = jsonb_set(
                 value,
                 '{ai,ollama_vision_num_ctx}',
                 to_jsonb($1::bigint)
               ),
               updated_at = now()
         where key = 'runtime'
        "#,
    )
    .bind(FLOOR)
    .execute(&mut *tx)
    .await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "worker.vision_num_ctx_bumped".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: previous.map(|v| json!({ "ollama_vision_num_ctx": v })),
            after: Some(json!({ "ollama_vision_num_ctx": FLOOR })),
            metadata: Some(json!({
                "trigger": "startup_one_shot",
                "reason": "ggml_assert_recurrence_at_16384",
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(VisionNumCtxBumpSummary {
        previous,
        current: FLOOR,
        bumped: true,
    })
}

/// Summary of a one-shot bump pass for the text `num_ctx` runtime setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextNumCtxBumpSummary {
    pub previous: Option<i64>,
    pub current: i64,
    pub bumped: bool,
}

/// One-shot, idempotent helper that raises `ai.ollama_text_num_ctx` to a 32768
/// floor. A large metadata prompt — the bounded OCR text plus the candidate
/// correspondent/type/tag allowlists, few-shots, and the JSON shape — can
/// exceed 16384 tokens on a long document and fail the metadata job with
/// `exceed_context_size_error` (observed in production at 18962 tokens). 32768
/// matches the vision floor and gives the bounded prompt comfortable headroom;
/// operators who already raised it past the floor are left alone. Raising the
/// floor re-bumps a deployment previously pinned at 16384 on the next startup.
pub async fn bump_text_num_ctx_if_too_small(pool: &DbPool) -> Result<TextNumCtxBumpSummary> {
    const FLOOR: i64 = 32768;

    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        select (value #>> '{ai,ollama_text_num_ctx}')::bigint as num_ctx
          from settings
         where key = 'runtime'
         for update
        "#,
    )
    .fetch_optional(&mut *tx)
    .await?;

    let previous: Option<i64> = row.and_then(|r| r.try_get("num_ctx").ok());
    let should_bump = match previous {
        Some(v) => v < FLOOR,
        None => true,
    };

    if !should_bump {
        tx.commit().await?;
        return Ok(TextNumCtxBumpSummary {
            previous,
            current: previous.unwrap_or(FLOOR),
            bumped: false,
        });
    }

    sqlx::query(
        r#"
        update settings
           set value = jsonb_set(value, '{ai,ollama_text_num_ctx}', to_jsonb($1::bigint)),
               updated_at = now()
         where key = 'runtime'
        "#,
    )
    .bind(FLOOR)
    .execute(&mut *tx)
    .await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "worker.text_num_ctx_bumped".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: previous.map(|v| json!({ "ollama_text_num_ctx": v })),
            after: Some(json!({ "ollama_text_num_ctx": FLOOR })),
            metadata: Some(json!({
                "trigger": "startup_one_shot",
                "reason": "text_context_overflow_above_16384",
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(TextNumCtxBumpSummary {
        previous,
        current: FLOOR,
        bumped: true,
    })
}

/// Summary of a one-shot pass that resets stuck `pipeline_runs.status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StuckRunStatusFixSummary {
    pub runs_reset: i64,
}

/// Recover review items stranded in `applying`: a worker or API request that
/// crashed between claiming a row (for a human apply or autopilot drain) and
/// recording the terminal status leaves the row owned-but-never-finished, and
/// neither the drain (lists `pending`) nor the operator (lists
/// `approved`/`edited`) would ever pick it up again. A row in `applying`
/// older than `older_than_seconds` is reverted to `pending` only when it has no
/// durable, non-terminal Paperless apply intent. Intent-backed rows must be
/// reconciled/finalized by the apply recovery state machine; blindly requeuing
/// them could repeat an externally successful PATCH. Returns the number of
/// rows recovered. #253, #342.
///
/// The same sweep also returns `approved`/`edited` rows to `pending` once
/// they are older than the window and have no unfinalized intent at all: a
/// human apply claims its row immediately after the decision, so such a row
/// was abandoned (handler future dropped by a proxy timeout, or a legacy
/// revert to the pre-claim status) and nothing else would ever apply it. #388
pub async fn reset_stale_applying_reviews(pool: &DbPool, older_than_seconds: i64) -> Result<i64> {
    let reset = sqlx::query(
        r#"
        update review_items
           set status = 'pending',
               reviewed_at = null
         where (
                 (status = 'applying'
                  and not exists (
                    select 1
                      from paperless_apply_intents pai
                     where pai.review_id = review_items.id
                       and pai.state in ('prepared', 'in_flight', 'confirmed', 'reconciled')
                  ))
                 or (status in ('approved', 'edited')
                  and not exists (
                    select 1
                      from paperless_apply_intents pai
                     where pai.review_id = review_items.id
                       and pai.finalized_at is null
                  ))
               )
           and reviewed_at < now() - make_interval(secs => $1)
        "#,
    )
    .bind(older_than_seconds as f64)
    .execute(pool)
    .await?
    .rows_affected() as i64;
    Ok(reset)
}

/// One-shot, idempotent helper that fixes `pipeline_runs.status='running'`
/// rows whose underlying jobs are all in a settled state (no `running` job
/// for that run). v1.5.4+ left these behind because `complete_job` only
/// flipped the run to `succeeded` when ALL stages were done; intermediate
/// stage successes (e.g. OCR done with metadata still queued) silently
/// stayed on `running`, surfacing as "N stuck run(s)" on the dashboard.
///
/// v1.5.7 fixes the live path in `complete_job` so new runs don't get
/// trapped, AND this helper cleans up the historical residue. Targets:
/// runs with status='running' AND no jobs.status='running' for that run —
/// flip to 'queued' if any queued job exists, else 'succeeded'.
pub async fn reset_stuck_running_pipeline_runs(pool: &DbPool) -> Result<StuckRunStatusFixSummary> {
    let mut tx = pool.begin().await?;
    // Two phases: (a) flip to 'queued' if there's at least one queued job for
    // the run that has no blocking prior-stage job; (b) otherwise (all jobs
    // succeeded/failed/cancelled), flip to 'succeeded' and set finished_at.
    // #439: candidates are selected here; the transition table re-checks the
    // `running` source status and that no job of the run is running.
    let queued_candidates: Vec<Uuid> = sqlx::query_scalar(
        r#"
        select pr.id
          from pipeline_runs pr
         where pr.status = 'running'
           and exists (
             select 1 from jobs j
              where j.run_id = pr.id and j.status in ('queued', 'waiting_review')
           )
         for update
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;
    let to_queued = transition_runs_tx(
        &mut tx,
        &queued_candidates,
        RunTransition::StuckRunningRequeued,
        None,
    )
    .await?;
    let succeeded_candidates: Vec<Uuid> = sqlx::query_scalar(concat!(
        r#"
        select pr.id
          from pipeline_runs pr
         where pr.status = 'running'
           and not exists (
             select 1 from jobs j
              where j.run_id = pr.id
                and j.status in ("#,
        sql_active_job_statuses!(),
        r#", 'failed')
           )
         for update
        "#
    ))
    .fetch_all(&mut *tx)
    .await?;
    let to_succeeded = transition_runs_tx(
        &mut tx,
        &succeeded_candidates,
        RunTransition::StuckRunningSucceeded,
        None,
    )
    .await?;
    let flipped_run_ids: Vec<Uuid> = to_queued
        .iter()
        .chain(to_succeeded.iter())
        .copied()
        .collect();

    // Mirror the new run status onto document_inventory.current_run_status.
    // Pre-#303 this helper skipped the mirror, so every cooldown-released
    // run it repaired left its inventory row showing a stale 'running' badge
    // (~10% of production rows) that polluted the inventory run-status
    // filter, the dashboard stage running counts and the running KPI. Only
    // rows whose last_run_id points at a flipped run are touched — if a newer
    // run owns the row, that run's own write sites govern the mirror. #303.
    mirror_run_status_tx(&mut tx, &flipped_run_ids, None).await?;
    let runs_reset = (to_queued.len() + to_succeeded.len()) as i64;

    if runs_reset > 0 {
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: "worker.stuck_running_runs_reset".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: None,
                after: Some(json!({
                    "to_queued": to_queued.len(),
                    "to_succeeded": to_succeeded.len(),
                })),
                metadata: Some(json!({
                    "trigger": "startup_one_shot",
                    "reason": "complete_job_status_bug_pre_v1.5.7",
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
    Ok(StuckRunStatusFixSummary { runs_reset })
}

/// Summary of a one-shot rebalance pass for backfilled metadata jobs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataPriorityRebalanceSummary {
    pub jobs_repriced: i64,
}

/// One-shot, idempotent fix for the v1.5.4 metadata-stage backfill bug.
///
/// The v1.5.4 backfill priced every new metadata job with
/// `payload.priority = 1_000_000 - paperless_document_id` (~993K–999K),
/// but the legacy trigger-polling OCR jobs sit at `payload.priority = 10`.
/// Since `claim_jobs` orders by `priority ASC` first, those metadata jobs
/// could not be claimed until every OCR job globally was succeeded — even
/// for runs whose own OCR was already done — which meant the backfilled
/// 5953 metadata jobs sat queued indefinitely behind the OCR backlog.
///
/// This helper finds every still-queued metadata job that has the
/// `payload.backfill = true` marker AND whose stored `payload.priority`
/// disagrees with the sibling OCR job's `payload.priority` for the same
/// `run_id`. It rewrites the metadata job's payload to inherit the OCR's
/// priority. Single transaction, idempotent — once every backfilled
/// metadata job's priority matches its OCR sibling, subsequent startups
/// find nothing to do.
pub async fn rebalance_backfilled_metadata_priorities(
    pool: &DbPool,
) -> Result<MetadataPriorityRebalanceSummary> {
    let mut tx = pool.begin().await?;
    let result = sqlx::query(
        r#"
        update jobs m
           set payload = jsonb_set(
                 m.payload,
                 '{priority}',
                 to_jsonb(coalesce((ocr.payload ->> 'priority')::bigint, 100))
               ),
               updated_at = now()
          from jobs ocr
         where m.stage = 'metadata'
           and m.status = 'queued'
           and (m.payload ->> 'backfill')::boolean = true
           and ocr.run_id = m.run_id
           and ocr.stage = 'ocr'
           and (m.payload ->> 'priority')::bigint
             is distinct from coalesce((ocr.payload ->> 'priority')::bigint, 100)
        "#,
    )
    .execute(&mut *tx)
    .await?;
    let jobs_repriced = result.rows_affected() as i64;

    if jobs_repriced > 0 {
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: "worker.metadata_priority_rebalanced".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: None,
                after: Some(json!({ "jobs_repriced": jobs_repriced })),
                metadata: Some(json!({
                    "trigger": "startup_one_shot",
                    "reason": "v1.5.4_backfill_priority_bug",
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
    Ok(MetadataPriorityRebalanceSummary { jobs_repriced })
}

// ---------------------------------------------------------------------------
// Metadata-trace diagnostic helpers (v1.5.21).
//
// Power the `GET /api/inventory/{document_id}/metadata-trace` endpoint: given a
// Paperless document id, fetch the most recent metadata-stage `pipeline_runs`
// row, the LLM `ai_artifacts` payload (if any), all `review_items` for the run,
// and the apply-time `audit_events` row (if any). The API layer composes these
// into a per-field outcome view.
//
// SQL quirk to remember: `ai_artifacts` has NO `paperless_document_id` column —
// the link to a document goes through `pipeline_runs.id` via `ai_artifacts.run_id`.
// The v1.5.14 → v1.5.19 regression was exactly this kind of "non-existent column
// resolved at runtime" bug; `tests/migration_smoke.rs` calls each helper below
// against the empty fresh-migration DB so the SQL parser catches it in CI.

/// Header for the most recent metadata-stage `pipeline_runs` row of a document.
/// Mirrors the fields the diagnostic UI needs without depending on the global
/// `pipeline_runs` row struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataRunHeader {
    pub run_id: Uuid,
    pub paperless_document_id: i32,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Most recent `ai_artifacts` row for a given run + `stage = 'metadata'`.
/// Carries model/provider for the run header and `normalized_output` so the
/// frontend can render the raw LLM suggestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataArtifact {
    pub id: Uuid,
    pub model: String,
    pub provider: String,
    pub normalized_output: Option<Value>,
    pub created_at: DateTime<Utc>,
}

/// One `review_items` row scoped to a metadata-stage run. Stage stays as a
/// string (not parsed `Stage`) because legacy in-flight v1.3 runs may still be
/// drained against legacy per-field stages and the diagnostic should not crash
/// on an unrecognised value — it just won't be matched against any of the six
/// metadata fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataReviewItem {
    pub id: Uuid,
    pub run_id: Uuid,
    pub stage: String,
    pub status: String,
    pub suggested_patch: Value,
    pub edited_patch: Option<Value>,
    pub validation_warnings: Value,
    pub created_at: DateTime<Utc>,
}

/// The most recent `audit_events` row carrying `event_type = 'document.patch_applied'`
/// for the run. Its `after` payload is the patch the worker pushed to Paperless,
/// keyed by `title` / `correspondent` (id) / `document_type` (id) / `created`
/// (document_date) / `tags` (Vec<i32>) / `custom_fields` (redacted summary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataApplyAudit {
    pub id: Uuid,
    pub run_id: Uuid,
    pub after: Option<Value>,
    pub outcome: String,
    pub created_at: DateTime<Utc>,
}

/// Most recent metadata-stage `pipeline_runs` row for a document.
///
/// "Metadata-stage" means: the run has a `jobs` row with `stage = 'metadata'`
/// or, for legacy v1.3.x runs queued before the consolidation, any of the
/// per-field stages (`title`, `correspondent`, `document_type`,
/// `document_date`, `tags`, `fields`). We use the broad set so the diagnostic
/// still works against in-flight legacy runs.
pub async fn latest_metadata_run_for_document(
    pool: &DbPool,
    paperless_document_id: i32,
) -> Result<Option<MetadataRunHeader>> {
    let row = sqlx::query(
        r#"
        select pr.id as run_id,
               pr.paperless_document_id as paperless_document_id,
               pr.status as status,
               pr.created_at as created_at,
               pr.finished_at as finished_at
          from pipeline_runs pr
         where pr.paperless_document_id = $1
           and exists (
             select 1
               from jobs j
              where j.run_id = pr.id
                and j.stage in (
                  'metadata',
                  'title',
                  'correspondent',
                  'document_type',
                  'document_date',
                  'tags',
                  'fields'
                )
           )
         order by pr.created_at desc
         limit 1
        "#,
    )
    .bind(paperless_document_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| -> Result<MetadataRunHeader> {
        Ok(MetadataRunHeader {
            run_id: row.try_get("run_id")?,
            paperless_document_id: row.try_get("paperless_document_id")?,
            status: row.try_get("status")?,
            created_at: row.try_get("created_at")?,
            finished_at: row.try_get("finished_at")?,
        })
    })
    .transpose()
}

/// Most recent `ai_artifacts` row for a metadata-stage run.
///
/// Returns `None` when the LLM call has not produced an artifact yet (e.g. the
/// run is still `queued` or failed before the metadata stage executed).
pub async fn latest_metadata_artifact_for_run(
    pool: &DbPool,
    run_id: Uuid,
) -> Result<Option<MetadataArtifact>> {
    let row = sqlx::query(
        r#"
        select id,
               model,
               provider,
               normalized_output,
               created_at
          from ai_artifacts
         where run_id = $1
           and stage = 'metadata'
         order by created_at desc
         limit 1
        "#,
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| -> Result<MetadataArtifact> {
        Ok(MetadataArtifact {
            id: row.try_get("id")?,
            model: row.try_get("model")?,
            provider: row.try_get("provider")?,
            normalized_output: row.try_get("normalized_output")?,
            created_at: row.try_get("created_at")?,
        })
    })
    .transpose()
}

/// All `review_items` rows for a metadata-stage run, ordered by creation time.
///
/// Returns an empty `Vec` for runs that produced no review items (full-auto
/// happy path) or for runs that have not reached the validation stage yet.
pub async fn metadata_review_items_for_run(
    pool: &DbPool,
    run_id: Uuid,
) -> Result<Vec<MetadataReviewItem>> {
    let rows = sqlx::query(
        r#"
        select id,
               run_id,
               stage,
               status,
               suggested_patch,
               edited_patch,
               validation_warnings,
               created_at
          from review_items
         where run_id = $1
         order by created_at asc
        "#,
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| -> Result<MetadataReviewItem> {
            Ok(MetadataReviewItem {
                id: row.try_get("id")?,
                run_id: row.try_get("run_id")?,
                stage: row.try_get("stage")?,
                status: row.try_get("status")?,
                suggested_patch: row.try_get("suggested_patch")?,
                edited_patch: row.try_get("edited_patch")?,
                validation_warnings: row.try_get("validation_warnings")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

/// Most recent `audit_events.document.patch_applied` row for the run.
///
/// Used to detect that the metadata patch made it all the way to Paperless and
/// to derive `latest_run.applied_at`. The `after` payload mirrors the
/// `DocumentPatch` shape the worker pushed (title/tags/correspondent/
/// document_type/created/custom_fields).
pub async fn latest_apply_audit_for_run(
    pool: &DbPool,
    run_id: Uuid,
) -> Result<Option<MetadataApplyAudit>> {
    let row = sqlx::query(
        r#"
        select id,
               run_id,
               after,
               outcome,
               created_at
          from audit_events
         where run_id = $1
           and event_type = 'document.patch_applied'
         order by created_at desc
         limit 1
        "#,
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| -> Result<MetadataApplyAudit> {
        // `audit_events.run_id` is nullable in the schema, but the row is
        // filtered by `run_id = $1` so the bind value is the canonical id.
        // Pulling it from `row.try_get` would force `Option<Uuid>` handling
        // for no real benefit.
        let _: Option<Uuid> = row.try_get("run_id")?;
        Ok(MetadataApplyAudit {
            id: row.try_get("id")?,
            run_id,
            after: row.try_get("after")?,
            outcome: row.try_get("outcome")?,
            created_at: row.try_get("created_at")?,
        })
    })
    .transpose()
}

// ---------------------------------------------------------------------------
// AI provider cooldowns (v1.5.27 — quota-aware backoff).
//
// When a provider replies 429 with a `usage limit` / `quota` signal in the
// body, the worker writes a cooldown record so subsequent claim cycles
// requeue jobs whose stage would route to that provider rather than burn
// per-job retries. The dashboard reads `list_active_provider_cooldowns` to
// surface "provider X paused until Y" warnings.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AiProviderCooldown {
    pub provider_name: String,
    pub cooldown_until: DateTime<Utc>,
    pub reason: String,
    pub set_at: DateTime<Utc>,
}

/// What [`upsert_provider_cooldown`] actually did. PostgreSQL 18's
/// `RETURNING old`/`new` lets the single upsert statement report this in one
/// round trip — previously it ended in `.execute()` and the call site could
/// not distinguish a fresh cooldown from an extension or a no-op against an
/// existing longer window (#317).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCooldownUpsert {
    pub outcome: CooldownUpsertOutcome,
    /// The cooldown end actually persisted: `greatest(existing, requested)`.
    /// Can be later than the requested value — callers parking jobs on the
    /// cooldown should use this, not the value they passed in.
    pub effective_until: DateTime<Utc>,
    /// The pre-upsert cooldown end (`None` when freshly inserted).
    pub previous_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownUpsertOutcome {
    /// No cooldown row existed; a fresh one was inserted.
    Inserted,
    /// An existing cooldown was extended to the (later) requested end.
    Extended,
    /// An existing equal-or-longer cooldown already covered the requested
    /// window; only `reason` and `updated_at` were refreshed.
    Unchanged,
}

impl CooldownUpsertOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::Extended => "extended",
            Self::Unchanged => "unchanged",
        }
    }
}

/// Upsert a cooldown for `provider_name`. If a cooldown row already exists,
/// the longer of (existing, new) wins — operator-set or quota-derived
/// cooldowns shouldn't be silently shortened by a follow-up 429 that
/// arrived with a smaller Retry-After. The returned
/// [`ProviderCooldownUpsert`] reports which of the three cases happened and
/// the effective cooldown end, so the caller can log/audit it.
pub async fn upsert_provider_cooldown(
    pool: &DbPool,
    provider_name: &str,
    cooldown_until: DateTime<Utc>,
    reason: &str,
) -> Result<ProviderCooldownUpsert> {
    // `old` is all-NULL on the insert arm and the pre-update row on the
    // conflict arm (PG18 RETURNING old/new); `cooldown_until` is NOT NULL,
    // so `old.cooldown_until IS NULL` identifies a fresh insert.
    let row = sqlx::query(
        r#"
        insert into ai_provider_cooldowns (provider_name, cooldown_until, reason)
        values ($1, $2, $3)
        on conflict (provider_name) do update
          set cooldown_until = greatest(ai_provider_cooldowns.cooldown_until, excluded.cooldown_until),
              reason = excluded.reason,
              updated_at = now()
        returning old.cooldown_until as previous_until,
                  new.cooldown_until as effective_until
        "#,
    )
    .bind(provider_name)
    .bind(cooldown_until)
    .bind(reason)
    .fetch_one(pool)
    .await
    .context("upsert ai_provider_cooldowns")?;
    let previous_until: Option<DateTime<Utc>> = row.try_get("previous_until")?;
    let effective_until: DateTime<Utc> = row.try_get("effective_until")?;
    let outcome = match previous_until {
        None => CooldownUpsertOutcome::Inserted,
        Some(previous) if effective_until > previous => CooldownUpsertOutcome::Extended,
        Some(_) => CooldownUpsertOutcome::Unchanged,
    };
    Ok(ProviderCooldownUpsert {
        outcome,
        effective_until,
        previous_until,
    })
}

/// Returns the cooldown end timestamp for `provider_name` if the cooldown
/// is still active (`cooldown_until > now()`). `None` means "fire away" —
/// either no cooldown was ever set or the cooldown has elapsed.
pub async fn get_active_provider_cooldown(
    pool: &DbPool,
    provider_name: &str,
) -> Result<Option<AiProviderCooldown>> {
    let row = sqlx::query(
        r#"
        select provider_name, cooldown_until, reason, set_at
          from ai_provider_cooldowns
         where provider_name = $1
           and cooldown_until > now()
        "#,
    )
    .bind(provider_name)
    .fetch_optional(pool)
    .await
    .context("query ai_provider_cooldowns")?;
    row.map(|row| {
        Ok(AiProviderCooldown {
            provider_name: row.try_get("provider_name")?,
            cooldown_until: row.try_get("cooldown_until")?,
            reason: row.try_get("reason")?,
            set_at: row.try_get("set_at")?,
        })
    })
    .transpose()
}

/// Drop a single provider's cooldown row early. Used by the operator
/// "Cooldown aufheben" action when they've just upgraded a plan / paid
/// for headroom and want the next claim cycle to retry immediately.
/// Idempotent — no error if no row matches.
pub async fn clear_provider_cooldown(pool: &DbPool, provider_name: &str) -> Result<u64> {
    let res = sqlx::query("delete from ai_provider_cooldowns where provider_name = $1")
        .bind(provider_name)
        .execute(pool)
        .await
        .context("delete ai_provider_cooldown")?;
    Ok(res.rows_affected())
}

/// Drop *all* provider cooldowns at once. The dashboard "Entsperren"
/// action calls this alongside `unblock_jobs_from_failed_predecessors`
/// so a one-click recovery clears both the dead-queue blockers and the
/// quota-cooldowns that caused them.
pub async fn clear_all_provider_cooldowns(pool: &DbPool) -> Result<u64> {
    let res = sqlx::query("delete from ai_provider_cooldowns")
        .execute(pool)
        .await
        .context("delete all ai_provider_cooldowns")?;
    Ok(res.rows_affected())
}

/// Wake jobs that a provider cooldown (or other backoff) deferred into the
/// future by resetting `run_after` to now, so the worker claims them on the
/// next poll instead of waiting out the full cooldown window. Targets only
/// `queued` jobs whose `run_after` is still in the future; in-flight and
/// already-eligible jobs are untouched, and `attempts` is preserved (this
/// reschedules, it does not reset the retry budget). Returns the number
/// released. Called by the manual "release parked jobs" operation, folded into
/// the cooldown-lift action, and triggered on an AI model/provider change so a
/// parked backlog immediately runs under the new configuration.
pub async fn release_scheduled_retries(pool: &DbPool) -> Result<u64> {
    let res = sqlx::query(
        r#"
        update jobs
           set run_after = now(),
               updated_at = now()
         where status = 'queued'
           and run_after > now()
        "#,
    )
    .execute(pool)
    .await
    .context("release scheduled job retries")?;
    Ok(res.rows_affected())
}

/// Upper bound of the regular transient-retry backoff window: `fail_job`
/// delays at most 2^6 * 30 s = 32 min, plus +25 % jitter = 2400 s. A queued
/// job parked further out than this cannot have got there via the per-job
/// retry backoff; in practice that is a provider-cooldown park
/// (`run_after = cooldown_until`, minutes to days out).
const MAX_TRANSIENT_RETRY_BACKOFF_SECONDS: f64 = 2400.0;

/// Scoped variant of [`release_scheduled_retries`] for the automatic release
/// on an AI model/provider change: wakes only jobs parked beyond the regular
/// transient-retry backoff horizon — i.e. cooldown parks — and leaves
/// unrelated in-flight backoff+jitter retries on their schedule, so a
/// settings save does not collapse the thundering-herd spacing of transient
/// retries (#313). A job parked under a shorter-than-horizon cooldown stays
/// parked at most until that (now cleared) cooldown would have ended anyway.
pub async fn release_cooldown_parked_retries(pool: &DbPool) -> Result<u64> {
    let res = sqlx::query(
        r#"
        update jobs
           set run_after = now(),
               updated_at = now()
         where status = 'queued'
           and run_after > now() + make_interval(secs => $1)
        "#,
    )
    .bind(MAX_TRANSIENT_RETRY_BACKOFF_SECONDS)
    .execute(pool)
    .await
    .context("release cooldown-parked job retries")?;
    Ok(res.rows_affected())
}

/// All currently-active cooldowns, ordered by remaining time (longest
/// first). Used by the dashboard to surface "provider X paused" warnings.
pub async fn list_active_provider_cooldowns(pool: &DbPool) -> Result<Vec<AiProviderCooldown>> {
    let rows = sqlx::query(
        r#"
        select provider_name, cooldown_until, reason, set_at
          from ai_provider_cooldowns
         where cooldown_until > now()
         order by cooldown_until desc
        "#,
    )
    .fetch_all(pool)
    .await
    .context("list ai_provider_cooldowns")?;
    rows.into_iter()
        .map(|row| {
            Ok(AiProviderCooldown {
                provider_name: row.try_get("provider_name")?,
                cooldown_until: row.try_get("cooldown_until")?,
                reason: row.try_get("reason")?,
                set_at: row.try_get("set_at")?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Operator unblock (v1.5.27).
//
// When a cascade of permanent failures (typically: provider quota
// exhausted, every retry burned) leaves a run with a `failed` predecessor
// stage, every subsequent queued job in that run is gated by the
// `claim_jobs` "NOT EXISTS earlier-stage in (failed, …)" filter. The
// queue silently stops draining. `unblock_jobs_from_failed_predecessors`
// finds those failed predecessors, resets them to `queued` with
// `attempts = 0`, and returns the count so the dashboard can report
// "N jobs unblocked".
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct UnblockSummary {
    /// How many `failed` predecessor jobs were reset back to queued.
    pub predecessors_requeued: i64,
    /// How many distinct runs are now eligible to make progress again.
    pub runs_unblocked: i64,
}

/// Reset every `failed` job that has at least one downstream `queued`
/// sibling (same run, higher `stage_priority`) back to `queued` with
/// `attempts = 0`. Optional `error_substring` filters to failures whose
/// `error_message ILIKE '%' || substring || '%'` — useful for unblocking
/// only the post-quota-exhaustion cohort while leaving genuine code-bug
/// failures pinned. Pass `None` to retry every blocking failure.
pub async fn unblock_jobs_from_failed_predecessors(
    pool: &DbPool,
    error_substring: Option<&str>,
) -> Result<UnblockSummary> {
    let row = sqlx::query(
        r#"
        with predecessors as (
            select j.id, j.run_id
              from jobs j
             where j.status = 'failed'
               and ($1::text is null or j.error_message ilike '%' || $1 || '%')
               and exists (
                   select 1 from jobs later
                    where later.run_id = j.run_id
                      and later.stage_priority > j.stage_priority
                      and later.status = 'queued'
               )
        ),
        updated as (
            update jobs j
               set status = 'queued',
                   attempts = 0,
                   error_message = null,
                   run_after = now(),
                   updated_at = now()
              from predecessors p
             where j.id = p.id
            returning j.run_id
        )
        select
            count(*)::bigint                  as predecessors_requeued,
            count(distinct run_id)::bigint    as runs_unblocked
          from updated
        "#,
    )
    .bind(error_substring)
    .fetch_one(pool)
    .await
    .context("unblock failed predecessors")?;
    Ok(UnblockSummary {
        predecessors_requeued: row.try_get("predecessors_requeued")?,
        runs_unblocked: row.try_get("runs_unblocked")?,
    })
}

/// Count queued jobs that the `claim_jobs` filter currently refuses to
/// hand out because an earlier-stage sibling is in (`failed`,
/// `waiting_review`). Surfaced on the dashboard so an operator sees the
/// dead-queue size and can decide to unblock.
pub async fn count_blocked_queued_jobs(pool: &DbPool) -> Result<BlockedQueuedCounts> {
    let row = sqlx::query(
        r#"
        select
            sum(case when blocker_status = 'failed'         then 1 else 0 end)::bigint as blocked_by_failed,
            sum(case when blocker_status = 'waiting_review' then 1 else 0 end)::bigint as blocked_by_review,
            count(*)::bigint as total
          from (
              select case
                  when exists (
                      select 1 from jobs prev
                       where prev.run_id = j.run_id
                         and prev.stage_priority < j.stage_priority
                         and prev.status = 'failed'
                  ) then 'failed'
                  when exists (
                      select 1 from jobs prev
                       where prev.run_id = j.run_id
                         and prev.stage_priority < j.stage_priority
                         and prev.status = 'waiting_review'
                  ) then 'waiting_review'
                  else null
              end as blocker_status
                from jobs j
               where j.status = 'queued'
          ) t
         where blocker_status is not null
        "#,
    )
    .fetch_one(pool)
    .await
    .context("count blocked queued jobs")?;
    Ok(BlockedQueuedCounts {
        blocked_by_failed: row
            .try_get::<Option<i64>, _>("blocked_by_failed")?
            .unwrap_or(0),
        blocked_by_review: row
            .try_get::<Option<i64>, _>("blocked_by_review")?
            .unwrap_or(0),
        total: row.try_get::<Option<i64>, _>("total")?.unwrap_or(0),
    })
}

#[derive(Debug, Clone, Default)]
pub struct BlockedQueuedCounts {
    pub blocked_by_failed: i64,
    pub blocked_by_review: i64,
    pub total: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit_hash_fixture(source_ip: Option<&str>, user_agent: Option<&str>) -> AuditEventInput {
        AuditEventInput {
            event_type: "user.roles_changed".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some("actor-17".to_owned()),
            run_id: Some(Uuid::parse_str("018f0000-0000-7000-8000-000000000001").unwrap()),
            job_id: Some(Uuid::parse_str("018f0000-0000-7000-8000-000000000002").unwrap()),
            paperless_document_id: Some(4904),
            before: Some(json!({ "roles": ["admin"] })),
            after: Some(json!({ "roles": ["viewer"] })),
            metadata: Some(json!({ "reason": "fixture" })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: source_ip.map(str::to_owned),
            user_agent: user_agent.map(str::to_owned),
        }
    }

    #[test]
    fn audit_hash_v1_canonical_fixture_is_stable() {
        let id = Uuid::parse_str("018f0000-0000-7000-8000-000000000003").unwrap();
        let created_at = "2026-07-17T08:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let previous = Some("previous-event-hash".to_owned());
        let event = audit_hash_fixture(Some("203.0.113.17"), Some("Archivist-Test/2"));

        assert_eq!(
            audit_event_hash_v1(id, created_at, &previous, &event),
            "ffd758b87049d65f9446a44190021fe0f1886a6fbaecace90a28de3c3d9368ea"
        );
    }

    #[test]
    fn audit_hash_v1_ignores_origin_but_v2_binds_values_and_nulls() {
        let id = Uuid::parse_str("018f0000-0000-7000-8000-000000000003").unwrap();
        let created_at = "2026-07-17T08:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let previous = Some("previous-event-hash".to_owned());
        let with_origin = audit_hash_fixture(Some("203.0.113.17"), Some("Archivist-Test/2"));
        let other_origin = audit_hash_fixture(Some("203.0.113.18"), Some("Archivist-Test/3"));
        let null_origin = audit_hash_fixture(None, None);

        assert_eq!(
            audit_event_hash_v1(id, created_at, &previous, &with_origin),
            audit_event_hash_v1(id, created_at, &previous, &other_origin)
        );
        let v2 = audit_event_hash_v2(id, created_at, &previous, &with_origin);
        assert_eq!(
            v2,
            "55f41a0611b9a83a4e7abdf657f9ffc463ecf49ffd575699a29c6c113d4073a7"
        );
        assert_ne!(
            v2,
            audit_event_hash_v2(id, created_at, &previous, &other_origin)
        );
        assert!(
            audit_event_hash_for_version(99, id, created_at, &previous, &with_origin).is_none()
        );
        assert_ne!(
            v2,
            audit_event_hash_v2(id, created_at, &previous, &null_origin)
        );
        assert_ne!(
            audit_event_hash_v2(id, created_at, &previous, &null_origin),
            audit_event_hash_v2(
                id,
                created_at,
                &previous,
                &audit_hash_fixture(Some("203.0.113.17"), None)
            )
        );
    }

    #[test]
    fn audit_timestamp_is_canonicalized_to_postgres_precision_before_hashing() {
        let id = Uuid::parse_str("018f0000-0000-7000-8000-000000000003").unwrap();
        let source = "2026-07-17T08:00:00.123456789Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let stored = "2026-07-17T08:00:00.123456Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let previous = Some("previous-event-hash".to_owned());
        let event = audit_hash_fixture(None, None);

        let canonical = postgres_timestamp_precision(source);
        assert_eq!(canonical, stored);
        assert_ne!(
            audit_event_hash_v2(id, source, &previous, &event),
            audit_event_hash_v2(id, stored, &previous, &event),
            "the regression requires a source timestamp PostgreSQL would truncate"
        );
        assert_eq!(
            audit_event_hash_v2(id, canonical, &previous, &event),
            audit_event_hash_v2(id, stored, &previous, &event),
            "the hash input must exactly match the timestamp read back from PostgreSQL"
        );
    }

    #[test]
    fn hashes_tokens_without_returning_raw_value() {
        assert_eq!(hash_token("secret"), hash_token("secret"));
        assert_ne!(hash_token("secret"), "secret");
    }

    #[test]
    fn status_table_names_are_static_known_tables() {
        assert_eq!(StatusTable::Jobs.name(), "jobs");
        assert_eq!(StatusTable::PipelineRuns.name(), "pipeline_runs");
        assert_eq!(StatusTable::ReviewItems.name(), "review_items");
    }

    #[test]
    fn status_column_for_stage_round_trips_every_business_stage() {
        // Every business stage must yield a static column name; orchestration-only stages
        // must surface a typed error so callers never silently fall through to format!.
        for stage in Stage::all_business_stages() {
            let column = status_column_for_stage(stage)
                .unwrap_or_else(|err| panic!("missing column for {stage}: {err}"));
            assert!(
                column.ends_with("_status"),
                "column for {stage} must end with _status, got {column}"
            );
        }
        assert!(status_column_for_stage(Stage::Apply).is_err());
    }

    fn empty_counts(total: i64, complete: i64) -> BacklogCounts {
        BacklogCounts {
            total_documents: total,
            complete,
            missing_ocr: 0,
            waiting_review: 0,
            failed: 0,
            running: 0,
            never_processed: 0,
        }
    }

    fn unrestricted_safety(dry_run: bool) -> WorkflowSafetyStatus {
        WorkflowSafetyStatus {
            paused: false,
            dry_run,
            hourly_document_limit: None,
            daily_document_limit: None,
            hourly_remaining: None,
            daily_remaining: None,
        }
    }

    fn live_failure(failure_kind: &str) -> DashboardLiveFailure {
        DashboardLiveFailure {
            id: Uuid::nil(),
            run_id: Uuid::nil(),
            paperless_document_id: 0,
            stage: Stage::Ocr,
            status: "failed".to_owned(),
            failure_kind: failure_kind.to_owned(),
            attempts: 1,
            error_message: String::new(),
            next_attempt_at: None,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn dashboard_comparison_subtracts_previous_window_and_uses_snapshot_when_present() {
        let counts = empty_counts(120, 80);
        let current = ActivitySummary {
            jobs_created: 50,
            jobs_succeeded: 40,
            jobs_failed: 7,
        };
        let previous = Some(ActivitySummary {
            jobs_created: 30,
            jobs_succeeded: 25,
            jobs_failed: 4,
        });
        let comparison = compute_dashboard_comparison(&counts, current, previous, Some(50));
        assert_eq!(comparison.jobs_created_delta, 20);
        assert_eq!(comparison.jobs_succeeded_delta, 15);
        assert_eq!(comparison.jobs_failed_delta, 3);
        // open_backlog = 120 - 80 = 40; previous_open_backlog = 50; delta = -10.
        assert_eq!(comparison.open_backlog_delta, -10);
    }

    #[test]
    fn dashboard_comparison_falls_back_to_zero_deltas_when_history_is_missing() {
        let counts = empty_counts(120, 80);
        let current = ActivitySummary {
            jobs_created: 5,
            jobs_succeeded: 3,
            jobs_failed: 1,
        };
        // No previous window and no snapshot -> deltas should all be zero
        // because the "previous" defaults to the current values and the
        // historical backlog defaults to the current open backlog.
        let comparison = compute_dashboard_comparison(&counts, current, None, None);
        assert_eq!(comparison.jobs_created_delta, 0);
        assert_eq!(comparison.jobs_succeeded_delta, 0);
        assert_eq!(comparison.jobs_failed_delta, 0);
        assert_eq!(comparison.open_backlog_delta, 0);
    }

    #[test]
    fn backlog_series_empty_state_synthesises_a_single_now_point() {
        let mut points: Vec<DashboardBacklogPoint> = Vec::new();
        let now = Utc::now();
        let counts = BacklogCounts {
            total_documents: 250,
            complete: 200,
            missing_ocr: 0,
            waiting_review: 3,
            failed: 4,
            running: 2,
            never_processed: 0,
        };
        apply_backlog_series_empty_state_fallback(
            &mut points,
            now,
            archivist_core::DashboardGranularity::Hour,
            &counts,
        );
        assert_eq!(points.len(), 1);
        let point = &points[0];
        assert_eq!(point.total_documents, 250);
        assert_eq!(point.complete, 200);
        assert_eq!(point.open_backlog, 50);
        assert_eq!(point.failed, 4);
        assert_eq!(point.waiting_review, 3);
        assert_eq!(point.running, 2);
    }

    #[test]
    fn backlog_series_empty_state_does_not_overwrite_existing_points() {
        let mut points: Vec<DashboardBacklogPoint> = vec![DashboardBacklogPoint {
            bucket: Utc::now(),
            label: "10:00".to_owned(),
            total_documents: 1,
            complete: 1,
            open_backlog: 0,
            failed: 0,
            waiting_review: 0,
            running: 0,
        }];
        apply_backlog_series_empty_state_fallback(
            &mut points,
            Utc::now(),
            archivist_core::DashboardGranularity::Hour,
            &empty_counts(99, 99),
        );
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].total_documents, 1);
    }

    #[test]
    fn needs_attention_items_emit_one_entry_per_kind() {
        let safety = WorkflowSafetyStatus {
            paused: false,
            dry_run: true,
            hourly_document_limit: Some(100),
            daily_document_limit: Some(1000),
            hourly_remaining: Some(2),  // <= ceil(100 * 0.1) = 10
            daily_remaining: Some(900), // 900 > 100 -> not below threshold
        };
        let failures = vec![
            live_failure("failed"),
            live_failure("failed"),
            live_failure("failed"),
            live_failure("retry_scheduled"),
        ];
        let items = compose_needs_attention_items(
            2,
            1,
            &safety,
            &failures,
            &BlockedQueuedCounts::default(),
            &[],
        );
        let kinds: Vec<&str> = items.iter().map(|i| i.kind.as_str()).collect();
        assert!(kinds.contains(&"stuck_runs"));
        assert!(kinds.contains(&"stale_leases"));
        assert!(kinds.contains(&"quota_low"));
        assert!(kinds.contains(&"provider_error"));
        assert!(kinds.contains(&"dry_run_active"));
    }

    #[test]
    fn needs_attention_items_sort_critical_before_warning_before_info() {
        let items = compose_needs_attention_items(
            5,
            5,
            &unrestricted_safety(true),
            &[
                live_failure("failed"),
                live_failure("failed"),
                live_failure("failed"),
            ],
            &BlockedQueuedCounts::default(),
            &[],
        );
        let severities: Vec<&str> = items.iter().map(|i| i.severity.as_str()).collect();
        // stuck_runs (critical) must come before stale_leases (warning),
        // dry_run_active (info) must come last.
        let critical_pos = severities
            .iter()
            .position(|s| *s == "critical")
            .expect("expected at least one critical item");
        let info_pos = severities
            .iter()
            .position(|s| *s == "info")
            .expect("expected at least one info item");
        assert!(
            critical_pos < info_pos,
            "critical severity ({critical_pos}) must sort before info ({info_pos}): {severities:?}"
        );
        for (index, severity) in severities.iter().enumerate().skip(1) {
            let prev = match severities[index - 1] {
                "critical" => 0,
                "warning" => 1,
                "info" => 2,
                _ => 3,
            };
            let curr = match *severity {
                "critical" => 0,
                "warning" => 1,
                "info" => 2,
                _ => 3,
            };
            assert!(prev <= curr, "ordering broken at {index}: {severities:?}");
        }
    }

    #[test]
    fn needs_attention_items_skips_provider_error_when_failures_are_below_threshold() {
        let items = compose_needs_attention_items(
            0,
            0,
            &unrestricted_safety(false),
            &[live_failure("failed"), live_failure("failed")],
            &BlockedQueuedCounts::default(),
            &[],
        );
        let has_provider_error = items.iter().any(|i| i.kind == "provider_error");
        assert!(!has_provider_error);
    }

    #[test]
    fn needs_attention_items_emit_blocked_jobs_when_present() {
        let items = compose_needs_attention_items(
            0,
            0,
            &unrestricted_safety(false),
            &[],
            &BlockedQueuedCounts {
                blocked_by_failed: 69,
                blocked_by_review: 24,
                total: 93,
            },
            &[],
        );
        let blocked = items
            .iter()
            .find(|i| i.kind == "blocked_jobs")
            .expect("expected a blocked_jobs alert when total > 0");
        assert_eq!(blocked.severity, "critical"); // any failed predecessor → critical
        assert_eq!(blocked.count, Some(93));
        assert_eq!(
            blocked.action_key.as_deref(),
            Some("dashboard.alerts.action.unblock_jobs"),
        );
    }

    #[test]
    fn needs_attention_items_emit_provider_cooldown_when_active() {
        let cooldown = AiProviderCooldown {
            provider_name: "ollama".to_owned(),
            cooldown_until: Utc::now() + chrono::Duration::hours(6),
            reason: "weekly usage limit".to_owned(),
            set_at: Utc::now(),
        };
        let items = compose_needs_attention_items(
            0,
            0,
            &unrestricted_safety(false),
            &[],
            &BlockedQueuedCounts::default(),
            &[cooldown],
        );
        let item = items
            .iter()
            .find(|i| i.kind == "provider_cooldown")
            .expect("expected provider_cooldown alert");
        assert_eq!(item.severity, "critical");
        assert!(item.description.contains("ollama"));
    }

    #[test]
    fn quota_below_threshold_uses_ten_percent_floor() {
        // Limit of 100 -> threshold = 10; remaining 10 must trip the alert,
        // remaining 11 must not.
        assert!(quota_below_threshold(Some(10), Some(100)));
        assert!(!quota_below_threshold(Some(11), Some(100)));
        // Limit of 3 -> threshold = max(ceil(0.3), 1) = 1; remaining 0 trips,
        // remaining 2 doesn't.
        assert!(quota_below_threshold(Some(0), Some(3)));
        assert!(!quota_below_threshold(Some(2), Some(3)));
        // Missing remaining or limit means no alert.
        assert!(!quota_below_threshold(None, Some(100)));
        assert!(!quota_below_threshold(Some(10), None));
        assert!(!quota_below_threshold(Some(10), Some(0)));
    }

    #[test]
    fn encrypted_secret_round_trips() {
        let key = SecretString::from("a long local encryption key for tests".to_owned());
        let ciphertext = encrypt_secret(&key, "paperless-token").unwrap();
        assert_ne!(ciphertext, "paperless-token");
        let plaintext = decrypt_secret(&key, &ciphertext).unwrap();
        assert_eq!(plaintext, "paperless-token");
    }

    #[test]
    fn ai_artifact_redaction_removes_prompts_images_and_response_text() {
        let value = json!({
            "model": "example",
            "system_prompt": "secret system prompt",
            "user_prompt": "full document text",
            "messages": [
                { "role": "user", "content": "private content", "images": ["base64-image"] }
            ],
            "usage": { "prompt_tokens": 10 }
        });
        let stored =
            prepare_ai_artifact_value(Some(value), AiArtifactStorageMode::Redacted).unwrap();
        let serialized = stored.to_string();

        assert!(!serialized.contains("secret system prompt"));
        assert!(!serialized.contains("full document text"));
        assert!(!serialized.contains("private content"));
        assert!(!serialized.contains("base64-image"));
        assert!(serialized.contains("redacted"));
        // Usage counters must survive redaction numerically, not as "[REDACTED]".
        assert_eq!(stored["usage"]["prompt_tokens"], 10);
    }

    #[test]
    fn full_storage_mode_keeps_numeric_usage_but_redacts_credentials() {
        let value = json!({
            "api_key": "sk-very-secret",
            "options": { "token": "raw-secret", "num_ctx": 4096 },
            "usage": { "prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14 },
            "prompt_eval_count": 12,
            "eval_count": 7
        });
        let stored = prepare_ai_artifact_value(Some(value), AiArtifactStorageMode::Full).unwrap();

        assert_eq!(stored["usage"]["prompt_tokens"], 10);
        assert_eq!(stored["usage"]["completion_tokens"], 4);
        assert_eq!(stored["usage"]["total_tokens"], 14);
        assert_eq!(stored["prompt_eval_count"], 12);
        assert_eq!(stored["eval_count"], 7);
        assert_eq!(stored["api_key"], "[REDACTED]");
        assert_eq!(stored["options"]["token"], "[REDACTED]");
        assert_eq!(stored["options"]["num_ctx"], 4096);
    }

    #[test]
    fn ai_artifact_metadata_only_keeps_usage_without_raw_content() {
        let value = json!({
            "model": "example",
            "response": "private model text",
            "usage": { "completion_tokens": 4 }
        });
        let stored =
            prepare_ai_artifact_value(Some(value), AiArtifactStorageMode::MetadataOnly).unwrap();
        let serialized = stored.to_string();

        assert!(!serialized.contains("private model text"));
        assert!(serialized.contains("metadata_only"));
        assert_eq!(stored["usage"]["completion_tokens"], 4);
    }

    #[test]
    fn ai_response_token_usage_handles_all_wire_shapes() {
        // OpenAI/Anthropic usage block.
        assert_eq!(
            ai_response_token_usage(Some(
                &json!({ "usage": { "prompt_tokens": 100, "completion_tokens": 40 } })
            )),
            (100, 40)
        );
        // Anthropic-style input_tokens/output_tokens.
        assert_eq!(
            ai_response_token_usage(Some(
                &json!({ "usage": { "input_tokens": 5, "output_tokens": 2 } })
            )),
            (5, 2)
        );
        // Ollama top-level counters.
        assert_eq!(
            ai_response_token_usage(Some(&json!({ "prompt_eval_count": 7, "eval_count": 3 }))),
            (7, 3)
        );
        // OCR pages[] fallback fires only without a top-level usage block...
        assert_eq!(
            ai_response_token_usage(Some(&json!({
                "pages": [
                    { "usage": { "prompt_tokens": 1000, "completion_tokens": 50 } },
                    { "prompt_eval_count": 200, "eval_count": 30 },
                ]
            }))),
            (1200, 80)
        );
        // ...so a post-#259 flattened response is never double counted.
        assert_eq!(
            ai_response_token_usage(Some(&json!({
                "pages": [ { "usage": { "prompt_tokens": 10, "completion_tokens": 5 } } ],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
            }))),
            (10, 5)
        );
        // Redacted strings / objects / negatives contribute 0, like the SQL
        // regexp guard in the 0040 backfill.
        assert_eq!(
            ai_response_token_usage(Some(&json!({
                "usage": { "prompt_tokens": "[REDACTED]", "completion_tokens": { "redacted": true } },
                "prompt_eval_count": -3
            }))),
            (0, 0)
        );
        assert_eq!(ai_response_token_usage(None), (0, 0));
    }

    #[test]
    fn paperless_document_date_parses_dates_and_timestamps_leniently() {
        // Current Paperless reports a plain ISO date; older releases sent a
        // full RFC3339 timestamp. Both must yield the date; junk yields None
        // instead of failing the sync. #315
        let expected = NaiveDate::from_ymd_opt(2026, 6, 1);
        assert_eq!(parse_paperless_document_date(Some("2026-06-01")), expected);
        assert_eq!(
            parse_paperless_document_date(Some("2026-06-01T00:00:00+02:00")),
            expected
        );
        assert_eq!(
            parse_paperless_document_date(Some(" 2026-06-01 ")),
            expected
        );
        assert_eq!(parse_paperless_document_date(Some("01.06.2026")), None);
        assert_eq!(parse_paperless_document_date(Some("")), None);
        assert_eq!(parse_paperless_document_date(None), None);
    }

    #[test]
    fn paperless_modified_timestamp_preserves_the_utc_instant() {
        let expected = DateTime::parse_from_rfc3339("2026-07-18T06:12:34.567890Z")
            .expect("valid expected timestamp")
            .with_timezone(&Utc);
        assert_eq!(
            parse_paperless_modified_at(Some("2026-07-18T08:12:34.567890+02:00")),
            Some(expected)
        );
        assert_eq!(parse_paperless_modified_at(Some("not-a-timestamp")), None);
        assert_eq!(parse_paperless_modified_at(None), None);
    }

    #[test]
    fn live_llm_status_prefers_running_jobs() {
        let now = Utc::now();
        let job = DashboardLiveJob {
            id: Uuid::now_v7(),
            run_id: Uuid::now_v7(),
            trace_id: Uuid::now_v7(),
            paperless_document_id: 42,
            stage: Stage::Metadata,
            status: "running".to_owned(),
            attempts: 1,
            max_attempts: 3,
            lease_owner: Some("worker-1".to_owned()),
            lease_until: Some(now),
            updated_at: now,
            error_message: None,
        };

        let status = llm_processing_status(&[job], &[], &[]);

        assert_eq!(status.state, "running");
        assert!(status.description.contains("42"));
    }

    #[test]
    fn live_paperless_status_reports_failed_audit_event() {
        let now = Utc::now();
        let event = PaperlessAuditEvent {
            event_type: "paperless.sync".to_owned(),
            outcome: "failed".to_owned(),
            created_at: now,
            error_message: Some("Paperless timeout".to_owned()),
        };

        let status = paperless_processing_status(&[], Some(&event), &[]);

        assert_eq!(status.state, "error");
        assert_eq!(status.description, "Paperless timeout");
    }

    #[test]
    fn live_status_ignores_retry_scheduled_failures_as_hard_errors() {
        let now = Utc::now();
        let retry = DashboardLiveFailure {
            id: Uuid::now_v7(),
            run_id: Uuid::now_v7(),
            paperless_document_id: 135,
            stage: Stage::Ocr,
            status: "queued".to_owned(),
            failure_kind: "retry_scheduled".to_owned(),
            attempts: 1,
            error_message: "temporary model runner failure".to_owned(),
            next_attempt_at: Some(now),
            updated_at: now,
        };

        let status = llm_processing_status(&[], &[], &[retry]);

        assert_eq!(status.state, "idle");
        assert_eq!(status.title, "LLM idle");
    }

    #[test]
    fn selector_document_budget_uses_tightest_remaining_limit() {
        let safety = WorkflowSafetyStatus {
            paused: false,
            dry_run: false,
            hourly_document_limit: Some(10),
            daily_document_limit: Some(100),
            hourly_remaining: Some(4),
            daily_remaining: Some(25),
        };

        assert_eq!(selector_document_budget(&safety), Some(4));

        let unlimited = WorkflowSafetyStatus {
            hourly_document_limit: None,
            daily_document_limit: None,
            hourly_remaining: None,
            daily_remaining: None,
            ..safety
        };

        assert_eq!(selector_document_budget(&unlimited), None);
    }

    #[test]
    fn missing_pipeline_stages_skip_completed_documents_and_stage_tags() {
        // v1.4.0 default selector sequence is [Ocr, Metadata]; document with the OCR
        // completion tag but no metadata yet should yield Metadata only.
        let stages = missing_pipeline_stages_for_inventory(
            &Stage::all_business_stages(),
            InventoryStageState {
                ocr_status: "unknown".to_owned(),
                metadata_status: "unknown".to_owned(),
                has_ocr_completion_tag: true,
                // Documents with the tagging-completion tag are considered "metadata done"
                // because the legacy tag was applied after the per-field stages all ran.
                has_tagging_completion_tag: false,
                has_full_completion_tag: false,
            },
        );

        assert!(!stages.contains(&Stage::Ocr));
        assert!(stages.contains(&Stage::Metadata));

        let completed = missing_pipeline_stages_for_inventory(
            &Stage::all_business_stages(),
            InventoryStageState {
                ocr_status: "unknown".to_owned(),
                metadata_status: "unknown".to_owned(),
                has_ocr_completion_tag: false,
                has_tagging_completion_tag: false,
                has_full_completion_tag: true,
            },
        );

        assert!(completed.is_empty());
    }

    #[test]
    fn missing_pipeline_stages_skip_documents_with_succeeded_metadata() {
        // Regression guard for the 0039 cleanup: before the fossil per-field
        // columns were dropped, the Metadata arm OR-ed six always-'unknown'
        // columns and therefore re-enqueued documents whose metadata_status
        // was already 'succeeded'. Only the consolidated column decides now.
        let stages = missing_pipeline_stages_for_inventory(
            &Stage::all_business_stages(),
            InventoryStageState {
                ocr_status: "succeeded".to_owned(),
                metadata_status: "succeeded".to_owned(),
                has_ocr_completion_tag: true,
                has_tagging_completion_tag: false,
                has_full_completion_tag: false,
            },
        );
        assert!(stages.is_empty());

        let needs_metadata = missing_pipeline_stages_for_inventory(
            &Stage::all_business_stages(),
            InventoryStageState {
                ocr_status: "succeeded".to_owned(),
                metadata_status: "failed".to_owned(),
                has_ocr_completion_tag: true,
                has_tagging_completion_tag: false,
                has_full_completion_tag: false,
            },
        );
        assert_eq!(needs_metadata, vec![Stage::Metadata]);
    }

    #[test]
    fn missing_pipeline_stages_skip_rejected_terminal_stage() {
        let stages = missing_pipeline_stages_for_inventory(
            &[Stage::Metadata],
            InventoryStageState {
                ocr_status: "succeeded".to_owned(),
                metadata_status: "rejected".to_owned(),
                has_ocr_completion_tag: true,
                has_tagging_completion_tag: false,
                has_full_completion_tag: false,
            },
        );

        assert!(
            stages.is_empty(),
            "an explicit review rejection is resolved and must not be auto-requeued"
        );
    }
}
