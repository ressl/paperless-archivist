//! Idempotent one-shot startup repairs.

use super::*;

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
