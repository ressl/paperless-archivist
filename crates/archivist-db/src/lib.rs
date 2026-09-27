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
mod jobs;
mod pool;
mod reviews;
mod runs;
mod settings;
mod stats;
mod users;

pub use chat::*;
pub use inventory::*;
pub use jobs::*;
pub use pool::*;
pub use reviews::*;
pub use runs::*;
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
