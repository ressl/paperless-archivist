//! Pipeline run creation, missing-stage queueing, completion tags and metadata run inspection.

use super::*;

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

pub(crate) struct PreparedRunCreation {
    pub(crate) run_id: Uuid,
    pub(crate) audit_event: Option<AuditEventInput>,
}

/// Resolve or create one active run and materialise its jobs/inventory state,
/// but leave the `run.created` audit append to the caller. Batch callers first
/// prepare every document, then take the global audit-chain lock; this keeps
/// all unique-index conflict waits ahead of audit serialization.
pub(crate) async fn prepare_run_with_jobs_on_tx(
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
pub(crate) async fn lock_active_run_document_tx(
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
pub(crate) async fn lock_active_run_documents_tx(
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

pub(crate) struct InventoryStageState {
    pub(crate) ocr_status: String,
    pub(crate) metadata_status: String,
    pub(crate) has_ocr_completion_tag: bool,
    pub(crate) has_tagging_completion_tag: bool,
    pub(crate) has_full_completion_tag: bool,
}

pub(crate) fn missing_pipeline_stages_for_inventory(
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
