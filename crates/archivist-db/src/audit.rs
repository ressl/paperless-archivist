//! Audit chain append, hashing, integrity verification, filters and security retention.

use super::*;

static AUDIT_INTEGRITY_VERIFY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

pub async fn append_audit(pool: &DbPool, event: AuditEventInput) -> Result<()> {
    let mut tx = pool.begin().await?;
    append_audit_tx(&mut tx, event).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn append_audit_tx(
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
pub(crate) fn postgres_timestamp_precision(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    timestamp
        .with_nanosecond(timestamp.nanosecond() / 1_000 * 1_000)
        .expect("a truncated nanosecond value is always valid")
}

pub(crate) fn audit_event_hash_v1(
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

pub(crate) fn audit_event_hash_v2(
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

pub(crate) fn audit_event_hash_for_version(
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
