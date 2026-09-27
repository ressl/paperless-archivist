//! Job claiming, completion/failure, leases, recovery, provider cooldowns and blocked jobs.

use super::*;

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
pub(crate) async fn cancel_active_run_jobs_tx(
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
pub(crate) async fn settle_run_after_stage_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
) -> Result<()> {
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

pub(crate) async fn set_inventory_stage_status_tx(
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
pub(crate) fn status_column_for_stage(stage: Stage) -> Result<&'static str> {
    stage
        .inventory_status_column()
        .ok_or_else(|| anyhow!("stage does not map to inventory status: {stage}"))
}

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
