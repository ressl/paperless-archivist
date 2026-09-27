//! Dashboard, statistics, provider usage/costs and metric counters.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub jobs_queued: i64,
    pub jobs_running: i64,
    pub jobs_failed: i64,
    pub jobs_succeeded: i64,
    pub reviews_pending: i64,
    pub runs_active: i64,
    pub audit_events: i64,
    pub selector_runs_total: i64,
    pub selector_documents_queued_total: i64,
    pub job_retries_scheduled_total: i64,
    pub model_errors_total: i64,
    pub apply_success_total: i64,
    pub apply_failure_total: i64,
    pub apply_latency_ms_count: i64,
    pub apply_latency_ms_sum: i64,
    pub apply_latency_ms_p95: i64,
    pub ocr_latency_ms_count: i64,
    pub ocr_latency_ms_p95: i64,
    pub metadata_latency_ms_count: i64,
    pub metadata_latency_ms_p95: i64,
    pub oldest_queued_age_seconds: i64,
}

pub async fn get_backlog_counts(pool: &DbPool) -> Result<BacklogCounts> {
    // The `failed` KPI covers the two live stage-status columns. The six
    // fossil per-field columns the OR-chain used to include were dropped in
    // migration 0039 (constant 'unknown' since the v1.4.0 consolidation);
    // metadata_status is their consolidated successor.
    let row = sqlx::query(
        r#"
        select
          count(*)::bigint as total_documents,
          count(*) filter (where complete)::bigint as complete,
          count(*) filter (where ocr_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_ocr,
          count(*) filter (where needs_review or current_run_status = 'waiting_review')::bigint as waiting_review,
          count(*) filter (where ocr_status = 'failed' or metadata_status = 'failed')::bigint as failed,
          count(*) filter (where current_run_status in ('queued', 'running', 'applying'))::bigint as running,
          count(*) filter (where last_run_id is null)::bigint as never_processed
        from document_inventory
        "#,
    )
    .fetch_one(pool)
    .await?;

    Ok(BacklogCounts {
        total_documents: row.try_get("total_documents")?,
        complete: row.try_get("complete")?,
        missing_ocr: row.try_get("missing_ocr")?,
        waiting_review: row.try_get("waiting_review")?,
        failed: row.try_get("failed")?,
        running: row.try_get("running")?,
        never_processed: row.try_get("never_processed")?,
    })
}

pub async fn record_dashboard_snapshot(pool: &DbPool, counts: &BacklogCounts) -> Result<()> {
    sqlx::query(
        r#"
        insert into dashboard_snapshots (
          total_documents, complete, missing_ocr, waiting_review,
          failed, running, never_processed
        )
        select $1, $2, $3, $4, $5, $6, $7
        where not exists (
          select 1 from dashboard_snapshots
           where captured_at >= now() - interval '5 minutes'
        )
        "#,
    )
    .bind(counts.total_documents)
    .bind(counts.complete)
    .bind(counts.missing_ocr)
    .bind(counts.waiting_review)
    .bind(counts.failed)
    .bind(counts.running)
    .bind(counts.never_processed)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ActivitySummary {
    pub(crate) jobs_created: i64,
    pub(crate) jobs_succeeded: i64,
    pub(crate) jobs_failed: i64,
}

pub async fn get_dashboard_stats(
    pool: &DbPool,
    range: DashboardRange,
    counts: &BacklogCounts,
    now: DateTime<Utc>,
    start: DateTime<Utc>,
) -> Result<DashboardStats> {
    // Snapshots are written by the worker tick loop (see archivist-worker::run_worker) so the
    // /dashboard read path no longer fires a write per poll. The 5-minute existence guard inside
    // `record_dashboard_snapshot` keeps the table de-duplicated regardless.
    let activity = activity_summary(pool, start, now).await?;
    let previous = if let Some(duration) = range.duration() {
        Some(activity_summary(pool, start - duration, start).await?)
    } else {
        None
    };
    let comparison = dashboard_comparison(pool, start, counts, activity, previous).await?;
    let job_status = status_counts(pool, StatusTable::Jobs).await?;
    let running_jobs = job_status
        .iter()
        .find(|item| item.status == "running")
        .map(|item| item.count)
        .unwrap_or_default();
    let completed_or_failed = activity.jobs_succeeded + activity.jobs_failed;
    let failure_rate = if completed_or_failed == 0 {
        0.0
    } else {
        activity.jobs_failed as f64 / completed_or_failed as f64
    };
    let completion_rate = if counts.total_documents == 0 {
        0.0
    } else {
        counts.complete as f64 / counts.total_documents as f64
    };
    let mttc_seconds = mttc_seconds_value(pool, start, now).await?;
    let p95_stage_duration_ms = p95_stage_duration_value(pool, start, now).await?;
    let cost_series = cost_series_tokens(pool, start, now, range).await?;

    Ok(DashboardStats {
        generated_at: now,
        selected_range: range.key().to_owned(),
        available_ranges: DashboardRange::options(),
        kpis: archivist_core::DashboardKpis {
            completion_rate,
            open_backlog: counts.total_documents - counts.complete,
            failure_rate,
            review_load: counts.waiting_review,
            running_jobs,
            throughput: activity.jobs_succeeded,
            cost_in_range_usd: None,
            mttc_seconds,
            p95_stage_duration_ms,
        },
        comparison,
        stage_status: stage_status(pool).await?,
        throughput_series: throughput_series(pool, start, now, range).await?,
        backlog_series: backlog_series(pool, start, now, range, counts).await?,
        job_status,
        run_status: status_counts(pool, StatusTable::PipelineRuns).await?,
        review_status: status_counts(pool, StatusTable::ReviewItems).await?,
        provider_usage: provider_usage(pool, start).await?,
        quality: quality_stats(pool, start).await?,
        cost_series,
        cost_breakdown_by_provider: Vec::new(),
    })
}

pub async fn get_dashboard_live_status(
    pool: &DbPool,
    settings: &RuntimeSettings,
) -> Result<DashboardLiveStatus> {
    let now = Utc::now();
    let active_runs = dashboard_live_runs(pool).await?;
    let active_jobs = dashboard_live_jobs(pool).await?;
    let recent_llm_events = dashboard_live_llm_events(pool).await?;
    let recent_failures = dashboard_live_failures(pool).await?;
    let latest_paperless_event = latest_paperless_audit_event(pool).await?;
    let workflow_safety = get_workflow_safety_status(pool, settings).await?;
    let selector_ready = settings.workflow.mode.auto_select_documents()
        && !workflow_safety.paused
        && workflow_safety
            .hourly_remaining
            .is_none_or(|remaining| remaining > 0)
        && workflow_safety
            .daily_remaining
            .is_none_or(|remaining| remaining > 0);
    let needs_attention = needs_attention_items(pool, &workflow_safety, &recent_failures).await?;

    Ok(DashboardLiveStatus {
        generated_at: now,
        workflow_mode: settings.workflow.mode,
        autopilot_enabled: selector_ready,
        workflow_safety: workflow_safety.clone(),
        selector: selector_processing_status(settings, &workflow_safety),
        next_selector_scan_at: selector_ready.then_some(now + chrono::Duration::seconds(60)),
        llm: llm_processing_status(&active_jobs, &recent_llm_events, &recent_failures),
        paperless: paperless_processing_status(
            &active_jobs,
            latest_paperless_event.as_ref(),
            &recent_failures,
        ),
        active_runs,
        active_jobs,
        recent_llm_events,
        recent_failures,
        needs_attention,
    })
}

pub async fn get_workflow_safety_status(
    pool: &DbPool,
    settings: &RuntimeSettings,
) -> Result<WorkflowSafetyStatus> {
    let hourly_used = auto_selector_runs_since(pool, "1 hour").await?;
    let daily_used = auto_selector_runs_since(pool, "1 day").await?;
    // Throughput caps follow the active provider's tuning when set, falling
    // back to the global workflow.* values otherwise. See
    // `RuntimeSettings::effective_tuning`.
    let tuning = settings.effective_tuning();
    Ok(WorkflowSafetyStatus {
        paused: settings.workflow.paused,
        dry_run: settings.workflow.dry_run,
        hourly_document_limit: tuning.hourly_document_limit,
        daily_document_limit: tuning.daily_document_limit,
        hourly_remaining: remaining_budget(tuning.hourly_document_limit, hourly_used),
        daily_remaining: remaining_budget(tuning.daily_document_limit, daily_used),
    })
}

async fn auto_selector_runs_since(pool: &DbPool, interval: &str) -> Result<i64> {
    sqlx::query_scalar(
        r#"
        select count(distinct paperless_document_id)::bigint
          from pipeline_runs
         where trigger_tag = 'auto-selector'
           and created_at >= now() - $1::interval
        "#,
    )
    .bind(interval)
    .fetch_one(pool)
    .await
    .context("count auto-selector runs")
}

fn remaining_budget(limit: Option<i64>, used: i64) -> Option<i64> {
    limit.map(|limit| (limit - used).max(0))
}

pub fn selector_document_budget(safety: &WorkflowSafetyStatus) -> Option<i64> {
    [safety.hourly_remaining, safety.daily_remaining]
        .into_iter()
        .flatten()
        .min()
}

fn selector_processing_status(
    settings: &RuntimeSettings,
    safety: &WorkflowSafetyStatus,
) -> ServiceProcessingStatus {
    if safety.paused {
        return ServiceProcessingStatus {
            state: "paused".to_owned(),
            title: "Auto selector paused".to_owned(),
            description: "Automatic document selection is paused. Manual queues remain available."
                .to_owned(),
            last_event_at: None,
        };
    }
    if !settings.workflow.mode.auto_select_documents() {
        return ServiceProcessingStatus {
            state: "idle".to_owned(),
            title: "Manual mode".to_owned(),
            description:
                "The selector is disabled because the workflow mode requires manual triggers."
                    .to_owned(),
            last_event_at: None,
        };
    }
    if selector_document_budget(safety).is_some_and(|remaining| remaining <= 0) {
        return ServiceProcessingStatus {
            state: "limited".to_owned(),
            title: "Auto selector limit reached".to_owned(),
            description: "Hourly or daily document limits are exhausted for the current window."
                .to_owned(),
            last_event_at: None,
        };
    }
    ServiceProcessingStatus {
        state: if safety.dry_run { "dry_run" } else { "running" }.to_owned(),
        title: if safety.dry_run {
            "Auto selector dry-run".to_owned()
        } else {
            "Auto selector ready".to_owned()
        },
        description: if safety.dry_run {
            "Documents can be selected and evaluated, but validated patches are not auto-applied."
                .to_owned()
        } else {
            "Automatic document selection is enabled and within configured safety limits."
                .to_owned()
        },
        last_event_at: None,
    }
}

async fn dashboard_live_runs(pool: &DbPool) -> Result<Vec<DashboardLiveRun>> {
    let rows = sqlx::query(concat!(
        r#"
        select id, paperless_document_id, mode, status, trigger_tag, stages,
               started_at, created_at, updated_at
          from pipeline_runs
         where status in ("#,
        sql_active_run_statuses!(),
        r#")
         order by updated_at desc
         limit 8
        "#
    ))
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let mode: String = row.try_get("mode")?;
            let stages: Value = row.try_get("stages")?;
            let id: Uuid = row.try_get("id")?;
            Ok(DashboardLiveRun {
                id,
                trace_id: id,
                paperless_document_id: row.try_get("paperless_document_id")?,
                mode: mode.parse()?,
                status: row.try_get("status")?,
                trigger_tag: row.try_get("trigger_tag")?,
                stages: serde_json::from_value(stages).unwrap_or_default(),
                started_at: row.try_get("started_at")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect()
}

async fn dashboard_live_jobs(pool: &DbPool) -> Result<Vec<DashboardLiveJob>> {
    let rows = sqlx::query(concat!(
        r#"
        select id, run_id, paperless_document_id, stage, status, attempts,
               max_attempts, lease_owner, lease_until, updated_at, error_message
          from jobs
         where status in ("#,
        sql_active_job_statuses!(),
        r#")
         order by case status when 'running' then 0 when 'queued' then 1 else 2 end,
                  updated_at desc
         limit 16
        "#
    ))
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: String = row.try_get("stage")?;
            let run_id: Uuid = row.try_get("run_id")?;
            Ok(DashboardLiveJob {
                id: row.try_get("id")?,
                run_id,
                trace_id: run_id,
                paperless_document_id: row.try_get("paperless_document_id")?,
                stage: stage.parse()?,
                status: row.try_get("status")?,
                attempts: row.try_get("attempts")?,
                max_attempts: row.try_get("max_attempts")?,
                lease_owner: row.try_get("lease_owner")?,
                lease_until: row.try_get("lease_until")?,
                updated_at: row.try_get("updated_at")?,
                error_message: row.try_get("error_message")?,
            })
        })
        .collect()
}

async fn dashboard_live_llm_events(pool: &DbPool) -> Result<Vec<DashboardLiveLlmEvent>> {
    let rows = sqlx::query(
        r#"
        select id, run_id, job_id, stage, provider, model, duration_ms, created_at
          from ai_artifacts
         order by created_at desc
         limit 8
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: String = row.try_get("stage")?;
            Ok(DashboardLiveLlmEvent {
                id: row.try_get("id")?,
                run_id: row.try_get("run_id")?,
                job_id: row.try_get("job_id")?,
                stage: stage.parse()?,
                provider: row.try_get("provider")?,
                model: row.try_get("model")?,
                duration_ms: row.try_get("duration_ms")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

async fn dashboard_live_failures(pool: &DbPool) -> Result<Vec<DashboardLiveFailure>> {
    // Only failures from each document's CURRENT run (`last_run_id`). The `jobs`
    // table keeps every historical failed row forever, so an unfiltered "last 8
    // failed jobs" perpetually shows old failures even after the documents were
    // re-run and succeeded — the live panel then disagrees with the failed KPI
    // (which reads the live document_inventory status). Binding to `last_run_id`
    // drops superseded failures: a doc re-run to success points last_run_id at
    // the new run, so its old failed job no longer matches.
    let rows = sqlx::query(
        r#"
        select j.id, j.run_id, j.paperless_document_id, j.stage, j.status, j.attempts,
               case
                 when j.status = 'queued' and j.run_after > now() then 'retry_scheduled'
                 when j.status = 'queued' then 'retry_ready'
                 else 'failed'
               end as failure_kind,
               coalesce(j.error_message, 'Job failed without details') as error_message,
               case when j.status = 'queued' then j.run_after else null end as next_attempt_at,
               j.updated_at
          from jobs j
          join document_inventory di
            on di.paperless_document_id = j.paperless_document_id
           and di.last_run_id = j.run_id
         where j.status = 'failed' or (j.status = 'queued' and j.error_message is not null)
         order by j.updated_at desc
         limit 8
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: String = row.try_get("stage")?;
            Ok(DashboardLiveFailure {
                id: row.try_get("id")?,
                run_id: row.try_get("run_id")?,
                paperless_document_id: row.try_get("paperless_document_id")?,
                stage: stage.parse()?,
                status: row.try_get("status")?,
                failure_kind: row.try_get("failure_kind")?,
                attempts: row.try_get("attempts")?,
                error_message: row.try_get("error_message")?,
                next_attempt_at: row.try_get("next_attempt_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub(crate) struct PaperlessAuditEvent {
    pub(crate) event_type: String,
    pub(crate) outcome: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) error_message: Option<String>,
}

async fn latest_paperless_audit_event(pool: &DbPool) -> Result<Option<PaperlessAuditEvent>> {
    let row = sqlx::query(
        r#"
        select event_type, outcome, created_at, error_message
          from audit_events
         where event_type in ('paperless.sync', 'document.patch_applied')
         order by created_at desc
         limit 1
        "#,
    )
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        Ok(PaperlessAuditEvent {
            event_type: row.try_get("event_type")?,
            outcome: row.try_get("outcome")?,
            created_at: row.try_get("created_at")?,
            error_message: row.try_get("error_message")?,
        })
    })
    .transpose()
}

pub(crate) fn llm_processing_status(
    active_jobs: &[DashboardLiveJob],
    recent_llm_events: &[DashboardLiveLlmEvent],
    recent_failures: &[DashboardLiveFailure],
) -> ServiceProcessingStatus {
    if let Some(job) = active_jobs.iter().find(|job| job.status == "running") {
        return ServiceProcessingStatus {
            state: "running".to_owned(),
            title: "LLM processing active".to_owned(),
            description: format!(
                "{} job for Paperless document {} is running.",
                job.stage, job.paperless_document_id
            ),
            last_event_at: Some(job.updated_at),
        };
    }

    if let Some(failure) = latest_hard_failure(recent_failures) {
        return ServiceProcessingStatus {
            state: "error".to_owned(),
            title: "Recent processing failure".to_owned(),
            description: failure.error_message.clone(),
            last_event_at: Some(failure.updated_at),
        };
    }

    if let Some(event) = recent_llm_events.first() {
        return ServiceProcessingStatus {
            state: "idle".to_owned(),
            title: "LLM idle".to_owned(),
            description: format!(
                "Last model call: {} / {} for {}.",
                event.provider, event.model, event.stage
            ),
            last_event_at: Some(event.created_at),
        };
    }

    ServiceProcessingStatus {
        state: "idle".to_owned(),
        title: "LLM idle".to_owned(),
        description: "No model activity recorded yet.".to_owned(),
        last_event_at: None,
    }
}

pub(crate) fn paperless_processing_status(
    active_jobs: &[DashboardLiveJob],
    latest_event: Option<&PaperlessAuditEvent>,
    recent_failures: &[DashboardLiveFailure],
) -> ServiceProcessingStatus {
    if let Some(job) = active_jobs.iter().find(|job| job.status == "running") {
        return ServiceProcessingStatus {
            state: "running".to_owned(),
            title: "Paperless processing active".to_owned(),
            description: format!(
                "Document {} is being read or updated for {}.",
                job.paperless_document_id, job.stage
            ),
            last_event_at: Some(job.updated_at),
        };
    }

    if let Some(event) = latest_event
        && event.outcome != "success"
    {
        return ServiceProcessingStatus {
            state: "error".to_owned(),
            title: "Recent Paperless action failed".to_owned(),
            description: event
                .error_message
                .clone()
                .unwrap_or_else(|| format!("{} ended with {}", event.event_type, event.outcome)),
            last_event_at: Some(event.created_at),
        };
    }

    if let Some(failure) = latest_hard_failure(recent_failures) {
        return ServiceProcessingStatus {
            state: "error".to_owned(),
            title: "Recent document processing failure".to_owned(),
            description: failure.error_message.clone(),
            last_event_at: Some(failure.updated_at),
        };
    }

    if let Some(event) = latest_event {
        return ServiceProcessingStatus {
            state: "idle".to_owned(),
            title: "Paperless idle".to_owned(),
            description: format!("Last Paperless action: {}.", event.event_type),
            last_event_at: Some(event.created_at),
        };
    }

    ServiceProcessingStatus {
        state: "idle".to_owned(),
        title: "Paperless idle".to_owned(),
        description: "No Paperless sync or patch activity recorded yet.".to_owned(),
        last_event_at: None,
    }
}

fn latest_hard_failure(recent_failures: &[DashboardLiveFailure]) -> Option<&DashboardLiveFailure> {
    recent_failures
        .iter()
        .find(|failure| failure.status == "failed" || failure.failure_kind == "failed")
}

pub async fn provider_usage(
    pool: &DbPool,
    start: DateTime<Utc>,
) -> Result<Vec<ProviderUsageStats>> {
    let rows = sqlx::query(
        r#"
        with artifacts as (
          select provider, model, stage, job_id, duration_ms, input_tokens, output_tokens
            from ai_artifacts
           where created_at >= $1
        ),
        -- Aggregate artifacts WITHOUT joining feedback, so request/token/
        -- latency stats are not multiplied by the number of feedback events
        -- per job (the fan-out bug). #260.
        --
        -- Token counters are the typed columns filled at insert time and
        -- backfilled by migration 0040 — no jsonb parsing on the read path.
        usage as (
          select provider,
                 model,
                 stage,
                 count(*)::bigint as request_count,
                 coalesce(avg(duration_ms), 0)::double precision as avg_duration_ms,
                 coalesce(percentile_cont(0.95) within group (order by duration_ms), 0)::bigint as p95_duration_ms,
                 coalesce(sum(input_tokens), 0)::bigint as input_tokens,
                 coalesce(sum(output_tokens), 0)::bigint as output_tokens
            from artifacts
           group by provider, model, stage
        ),
        -- Feedback aggregated separately, keyed by the distinct artifact
        -- (provider, model, stage, job_id) tuples so each feedback event is
        -- counted once per cell. Bounded to the same range as the artifacts.
        feedback as (
          select a.provider, a.model, a.stage,
                 count(distinct f.id)::bigint as feedback_count,
                 count(distinct f.id) filter (
                   where f.event_type in ('review.approved', 'review.edited')
                 )::bigint as positive_feedback,
                 count(distinct f.id) filter (
                   where f.event_type = 'review.rejected'
                 )::bigint as negative_feedback
            from (select distinct provider, model, stage, job_id from artifacts) a
            join audit_events f
              on f.job_id = a.job_id
             and f.event_type in ('review.approved', 'review.edited', 'review.rejected')
             and f.created_at >= $1
           group by a.provider, a.model, a.stage
        )
        select u.provider,
               u.model,
               u.stage,
               u.request_count,
               u.avg_duration_ms,
               u.p95_duration_ms,
               u.input_tokens,
               u.output_tokens,
               coalesce(fb.feedback_count, 0)::bigint as feedback_count,
               coalesce(fb.positive_feedback, 0)::bigint as positive_feedback,
               coalesce(fb.negative_feedback, 0)::bigint as negative_feedback
          from usage u
          left join feedback fb
            on fb.provider = u.provider and fb.model = u.model and fb.stage = u.stage
         order by u.request_count desc, u.provider, u.model, u.stage
         limit 50
        "#,
    )
    .bind(start)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(ProviderUsageStats {
                provider: row.try_get("provider")?,
                model: row.try_get("model")?,
                stage: row.try_get("stage")?,
                request_count: row.try_get("request_count")?,
                avg_duration_ms: row.try_get("avg_duration_ms")?,
                p95_duration_ms: row.try_get("p95_duration_ms")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                estimated_cost_usd: None,
                feedback_count: row.try_get("feedback_count")?,
                positive_feedback: row.try_get("positive_feedback")?,
                negative_feedback: row.try_get("negative_feedback")?,
                acceptance_rate: feedback_rate(
                    row.try_get("positive_feedback")?,
                    row.try_get("negative_feedback")?,
                ),
                latency_history: Vec::new(),
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ProviderBucketEntry {
    pub bucket: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub stage: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub avg_duration_ms: Option<f64>,
    pub request_count: i64,
}

pub async fn provider_bucket_entries(
    pool: &DbPool,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
) -> Result<Vec<ProviderBucketEntry>> {
    let granularity = range.granularity();
    let rows = sqlx::query(
        r#"
        select
          date_trunc($4, ai.created_at) as bucket,
          ai.provider,
          ai.model,
          ai.stage,
          -- Typed counters (insert-time + 0040 backfill); no jsonb parsing.
          coalesce(sum(ai.input_tokens), 0)::bigint as input_tokens,
          coalesce(sum(ai.output_tokens), 0)::bigint as output_tokens,
          avg(duration_ms)::double precision as avg_duration_ms,
          count(*)::bigint as request_count
        from ai_artifacts ai
        where ai.created_at >= $1
          and ai.created_at < $2
        group by 1, 2, 3, 4
        order by 1, 2, 3, 4
        "#,
    )
    .bind(start)
    .bind(now)
    .bind(granularity.interval())
    .bind(granularity.date_trunc())
    .fetch_all(pool)
    .await
    .context("query provider bucket entries")?;

    rows.into_iter()
        .map(|row| {
            Ok(ProviderBucketEntry {
                bucket: row.try_get("bucket")?,
                provider: row.try_get("provider")?,
                model: row.try_get("model")?,
                stage: row.try_get("stage")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                avg_duration_ms: row.try_get("avg_duration_ms")?,
                request_count: row.try_get("request_count")?,
            })
        })
        .collect()
}

// --- Statistics page aggregations -------------------------------------------
// Fine-grained, custom-range aggregations powering the dedicated Statistics
// page. The API handler assembles summary / by-provider / by-model / by-stage /
// time-series views from these rows in Rust (one DB round-trip each).

/// One (bucket × provider × model × stage) AI-usage cell.
///
/// No p95 here on purpose: percentiles cannot be re-aggregated across the
/// per-cell grain this query returns, so a per-cell p95 was computed but
/// never surfaced. Dropped rather than exposing a misleading number (#312);
/// the dashboard's `provider_usage` computes its p95 over the raw rows.
#[derive(Debug, Clone)]
pub struct StatisticsUsageRow {
    pub bucket: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub stage: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub avg_duration_ms: Option<f64>,
}

/// AI-usage cells over a custom `[from, to)` range, bucketed by `trunc`
/// (a validated date_trunc unit: hour/day/week/month). Token counters are the
/// typed `input_tokens`/`output_tokens` columns written at insert time and
/// backfilled for historical rows by migration 0040.
pub async fn statistics_usage_rows(
    pool: &DbPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    trunc: &str,
) -> Result<Vec<StatisticsUsageRow>> {
    let rows = sqlx::query(
        r#"
        select
          date_trunc($3, ai.created_at) as bucket,
          ai.provider,
          ai.model,
          ai.stage,
          count(*)::bigint as request_count,
          coalesce(sum(ai.input_tokens), 0)::bigint as input_tokens,
          coalesce(sum(ai.output_tokens), 0)::bigint as output_tokens,
          avg(duration_ms)::double precision as avg_duration_ms
        from ai_artifacts ai
        where ai.created_at >= $1
          and ai.created_at < $2
        group by 1, 2, 3, 4
        order by 1, 2, 3, 4
        "#,
    )
    .bind(from)
    .bind(to)
    .bind(trunc)
    .fetch_all(pool)
    .await
    .context("query statistics usage rows")?;

    rows.into_iter()
        .map(|row| {
            Ok(StatisticsUsageRow {
                bucket: row.try_get("bucket")?,
                provider: row.try_get("provider")?,
                model: row.try_get("model")?,
                stage: row.try_get("stage")?,
                request_count: row.try_get("request_count")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                avg_duration_ms: row.try_get("avg_duration_ms")?,
            })
        })
        .collect()
}

/// One (bucket × stage × status) pipeline-throughput cell from `jobs`.
#[derive(Debug, Clone)]
pub struct StatisticsThroughputRow {
    pub bucket: DateTime<Utc>,
    pub stage: String,
    pub status: String,
    pub job_count: i64,
}

/// Pipeline throughput over a custom `[from, to)` range, bucketed by `trunc`.
/// Buckets on `updated_at` (when the job reached its terminal status).
pub async fn statistics_throughput_rows(
    pool: &DbPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    trunc: &str,
) -> Result<Vec<StatisticsThroughputRow>> {
    let rows = sqlx::query(
        r#"
        select
          date_trunc($3, updated_at) as bucket,
          stage,
          status,
          count(*)::bigint as job_count
        from jobs
        where updated_at >= $1
          and updated_at < $2
          and status in ('succeeded', 'failed', 'cancelled')
        group by 1, 2, 3
        order by 1, 2, 3
        "#,
    )
    .bind(from)
    .bind(to)
    .bind(trunc)
    .fetch_all(pool)
    .await
    .context("query statistics throughput rows")?;

    rows.into_iter()
        .map(|row| {
            Ok(StatisticsThroughputRow {
                bucket: row.try_get("bucket")?,
                stage: row.try_get("stage")?,
                status: row.try_get("status")?,
                job_count: row.try_get("job_count")?,
            })
        })
        .collect()
}

pub fn dashboard_bucket_labels(
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
) -> Vec<(DateTime<Utc>, String)> {
    use chrono::TimeZone;
    let granularity = range.granularity();
    let start_trunc = truncate_to_granularity(start, granularity);
    let mut buckets = Vec::new();
    let mut cursor = start_trunc;
    while cursor < now {
        buckets.push((cursor, bucket_label(cursor, granularity)));
        cursor = match granularity {
            archivist_core::DashboardGranularity::Hour => cursor + ChronoDuration::hours(1),
            archivist_core::DashboardGranularity::Day => cursor + ChronoDuration::days(1),
            archivist_core::DashboardGranularity::Month => {
                let next_month = if cursor.month() == 12 {
                    Utc.with_ymd_and_hms(cursor.year() + 1, 1, 1, 0, 0, 0)
                        .single()
                } else {
                    Utc.with_ymd_and_hms(cursor.year(), cursor.month() + 1, 1, 0, 0, 0)
                        .single()
                };
                match next_month {
                    Some(value) => value,
                    None => break,
                }
            }
        };
    }
    buckets
}

fn truncate_to_granularity(
    timestamp: DateTime<Utc>,
    granularity: archivist_core::DashboardGranularity,
) -> DateTime<Utc> {
    use chrono::TimeZone;
    match granularity {
        archivist_core::DashboardGranularity::Hour => Utc
            .with_ymd_and_hms(
                timestamp.year(),
                timestamp.month(),
                timestamp.day(),
                timestamp.hour(),
                0,
                0,
            )
            .single()
            .unwrap_or(timestamp),
        archivist_core::DashboardGranularity::Day => Utc
            .with_ymd_and_hms(
                timestamp.year(),
                timestamp.month(),
                timestamp.day(),
                0,
                0,
                0,
            )
            .single()
            .unwrap_or(timestamp),
        archivist_core::DashboardGranularity::Month => Utc
            .with_ymd_and_hms(timestamp.year(), timestamp.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(timestamp),
    }
}

fn feedback_rate(positive: i64, negative: i64) -> Option<f64> {
    let total = positive + negative;
    (total > 0).then_some(positive as f64 / total as f64)
}

async fn quality_stats(pool: &DbPool, start: DateTime<Utc>) -> Result<QualityStats> {
    let row = sqlx::query(
        r#"
        select
          count(*) filter (where event_type in ('review.approved', 'review.edited', 'review.rejected'))::bigint as review_decisions,
          count(*) filter (where event_type = 'review.approved')::bigint as review_approved,
          count(*) filter (where event_type = 'review.edited')::bigint as review_edited,
          count(*) filter (where event_type = 'review.rejected')::bigint as review_rejected
          from audit_events
         where created_at >= $1
        "#,
    )
    .bind(start)
    .fetch_one(pool)
    .await?;
    let review_approved: i64 = row.try_get("review_approved")?;
    let review_edited: i64 = row.try_get("review_edited")?;
    let review_rejected: i64 = row.try_get("review_rejected")?;
    let warning_row = sqlx::query(
        r#"
        select
          count(*) filter (
            where validation_warnings::text ilike '%LowConfidence%'
               or validation_warnings::text ilike '%low confidence%'
               or validation_warnings::text ilike '%below threshold%'
          )::bigint as uncertainty_reviews,
          count(*) filter (
            where validation_warnings is not null
              and validation_warnings <> '[]'::jsonb
          )::bigint as validation_warning_reviews
          from review_items
         where created_at >= $1
        "#,
    )
    .bind(start)
    .fetch_one(pool)
    .await?;
    Ok(QualityStats {
        review_decisions: row.try_get("review_decisions")?,
        review_approved,
        review_edited,
        review_rejected,
        acceptance_rate: feedback_rate(review_approved + review_edited, review_rejected),
        uncertainty_reviews: warning_row.try_get("uncertainty_reviews")?,
        validation_warning_reviews: warning_row.try_get("validation_warning_reviews")?,
    })
}

pub async fn dashboard_range_start(
    pool: &DbPool,
    range: DashboardRange,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    if let Some(duration) = range.duration() {
        return Ok(now - duration);
    }
    let row = sqlx::query(
        r#"
        select least(
          coalesce((select min(created_at) from jobs), now()),
          coalesce((select min(created_at) from pipeline_runs), now()),
          coalesce((select min(captured_at) from dashboard_snapshots), now())
        ) as started_at
        "#,
    )
    .fetch_one(pool)
    .await?;
    row.try_get("started_at").context("dashboard range start")
}

async fn activity_summary(
    pool: &DbPool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<ActivitySummary> {
    let row = sqlx::query(
        r#"
        select
          count(*) filter (where created_at >= $1 and created_at < $2)::bigint as jobs_created,
          count(*) filter (where status = 'succeeded' and updated_at >= $1 and updated_at < $2)::bigint as jobs_succeeded,
          count(*) filter (where status = 'failed' and updated_at >= $1 and updated_at < $2)::bigint as jobs_failed
        from jobs
        "#,
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await?;
    Ok(ActivitySummary {
        jobs_created: row.try_get("jobs_created")?,
        jobs_succeeded: row.try_get("jobs_succeeded")?,
        jobs_failed: row.try_get("jobs_failed")?,
    })
}

async fn dashboard_comparison(
    pool: &DbPool,
    start: DateTime<Utc>,
    counts: &BacklogCounts,
    current: ActivitySummary,
    previous: Option<ActivitySummary>,
) -> Result<DashboardComparison> {
    let previous_open_backlog: Option<i64> = sqlx::query(
        r#"
        select total_documents - complete as open_backlog
          from dashboard_snapshots
         where captured_at < $1
         order by captured_at desc
         limit 1
        "#,
    )
    .bind(start)
    .fetch_optional(pool)
    .await?
    .map(|row| row.try_get::<i64, _>("open_backlog"))
    .transpose()?;
    Ok(compute_dashboard_comparison(
        counts,
        current,
        previous,
        previous_open_backlog,
    ))
}

/// Pure half of `dashboard_comparison` — composes a `DashboardComparison`
/// from the current activity summary, the optional previous-period summary
/// and an optional historical open-backlog snapshot. Extracted so the math
/// can be unit-tested without a pool.
pub(crate) fn compute_dashboard_comparison(
    counts: &BacklogCounts,
    current: ActivitySummary,
    previous: Option<ActivitySummary>,
    previous_open_backlog: Option<i64>,
) -> DashboardComparison {
    let previous_open_backlog =
        previous_open_backlog.unwrap_or(counts.total_documents - counts.complete);
    let previous = previous.unwrap_or(ActivitySummary {
        jobs_created: current.jobs_created,
        jobs_succeeded: current.jobs_succeeded,
        jobs_failed: current.jobs_failed,
    });
    DashboardComparison {
        jobs_created_delta: current.jobs_created - previous.jobs_created,
        jobs_succeeded_delta: current.jobs_succeeded - previous.jobs_succeeded,
        jobs_failed_delta: current.jobs_failed - previous.jobs_failed,
        open_backlog_delta: counts.total_documents - counts.complete - previous_open_backlog,
    }
}

/// Per-stage rollup for the dashboard Stage-Matrix.
///
/// Emits exactly the `ocr` and `metadata` rows (the consolidated v1.4.0 stage
/// set; `metadata_status` is the column added in migration 0019). The legacy
/// per-field UNION arms (title/document_type/correspondent/document_date/
/// tags/fields) were removed with their columns in migration 0039; they had
/// reported the constant 'unknown' since v1.4.0 and were suppressed from the
/// matrix by the old `touched > 0` clause anyway.
async fn stage_status(pool: &DbPool) -> Result<Vec<DashboardStageStatus>> {
    let rows = sqlx::query(
        concat!(r#"
        with stage_rows as (
          select 'ocr' as stage,
                 case when has_full_completion_tag then 'succeeded' else ocr_status end as status,
                 current_run_status
            from document_inventory
          union all
          select 'metadata',
                 case when has_full_completion_tag then 'succeeded' else metadata_status end,
                 current_run_status
            from document_inventory
        ),
        counted as (
          select
            stage,
            count(*)::bigint as total,
            count(*) filter (where status in ("#,
sql_terminal_stage_statuses!(),
r#"))::bigint as complete,
            count(*) filter (where status = 'failed')::bigint as failed,
            count(*) filter (where status = 'waiting_review' or current_run_status = 'waiting_review')::bigint as waiting_review,
            count(*) filter (where current_run_status in ('queued', 'running', 'applying') and status not in ("#,
sql_terminal_stage_statuses!(),
r#", 'failed'))::bigint as running
          from stage_rows
          group by stage
        )
        select stage, complete, failed, waiting_review, running,
               greatest(total - complete - failed - waiting_review - running, 0)::bigint as pending
          from counted
         order by case stage when 'ocr' then 1 else 2 end
        "#),
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(DashboardStageStatus {
                stage: row.try_get("stage")?,
                complete: row.try_get("complete")?,
                pending: row.try_get("pending")?,
                failed: row.try_get("failed")?,
                waiting_review: row.try_get("waiting_review")?,
                running: row.try_get("running")?,
            })
        })
        .collect()
}

async fn throughput_series(
    pool: &DbPool,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
) -> Result<Vec<DashboardTimeBucket>> {
    let granularity = range.granularity();
    let rows = sqlx::query(
        r#"
        with buckets as (
          select generate_series(date_trunc($4, $1), $2, $3::interval) as bucket
        )
        select
          b.bucket,
          (select count(*)::bigint from jobs where created_at >= b.bucket and created_at < b.bucket + $3::interval) as jobs_created,
          (select count(*)::bigint from jobs where status = 'succeeded' and updated_at >= b.bucket and updated_at < b.bucket + $3::interval) as jobs_succeeded,
          (select count(*)::bigint from jobs where status = 'failed' and updated_at >= b.bucket and updated_at < b.bucket + $3::interval) as jobs_failed,
          (select count(*)::bigint from pipeline_runs where created_at >= b.bucket and created_at < b.bucket + $3::interval) as runs_created,
          (select count(*)::bigint from pipeline_runs where status = 'succeeded' and finished_at >= b.bucket and finished_at < b.bucket + $3::interval) as runs_succeeded,
          (select count(*)::bigint from pipeline_runs where status = 'failed' and finished_at >= b.bucket and finished_at < b.bucket + $3::interval) as runs_failed
        from buckets b
        order by b.bucket
        "#,
    )
    .bind(start)
    .bind(now)
    .bind(granularity.interval())
    .bind(granularity.date_trunc())
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let bucket: DateTime<Utc> = row.try_get("bucket")?;
            Ok(DashboardTimeBucket {
                label: bucket_label(bucket, granularity),
                bucket,
                jobs_created: row.try_get("jobs_created")?,
                jobs_succeeded: row.try_get("jobs_succeeded")?,
                jobs_failed: row.try_get("jobs_failed")?,
                runs_created: row.try_get("runs_created")?,
                runs_succeeded: row.try_get("runs_succeeded")?,
                runs_failed: row.try_get("runs_failed")?,
            })
        })
        .collect()
}

async fn backlog_series(
    pool: &DbPool,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
    counts: &BacklogCounts,
) -> Result<Vec<DashboardBacklogPoint>> {
    let granularity = range.granularity();
    let rows = sqlx::query(
        r#"
        with buckets as (
          select generate_series(date_trunc($4, $1), $2, $3::interval) as bucket
        )
        select b.bucket, s.total_documents, s.complete, s.failed, s.waiting_review, s.running
          from buckets b
          join lateral (
            select total_documents, complete, failed, waiting_review, running
              from dashboard_snapshots
             where captured_at >= b.bucket
               and captured_at < b.bucket + $3::interval
             order by captured_at desc
             limit 1
          ) s on true
         order by b.bucket
        "#,
    )
    .bind(start)
    .bind(now)
    .bind(granularity.interval())
    .bind(granularity.date_trunc())
    .fetch_all(pool)
    .await?;

    let mut points = rows
        .into_iter()
        .map(|row| {
            let bucket: DateTime<Utc> = row.try_get("bucket")?;
            let total_documents: i64 = row.try_get("total_documents")?;
            let complete: i64 = row.try_get("complete")?;
            Ok(DashboardBacklogPoint {
                label: bucket_label(bucket, granularity),
                bucket,
                total_documents,
                complete,
                open_backlog: total_documents - complete,
                failed: row.try_get("failed")?,
                waiting_review: row.try_get("waiting_review")?,
                running: row.try_get("running")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    apply_backlog_series_empty_state_fallback(&mut points, now, granularity, counts);

    Ok(points)
}

/// Pure helper that synthesises a single "now" backlog point from the live
/// `counts` snapshot when no `dashboard_snapshots` rows fall inside the
/// requested range. Extracted so the empty-state behaviour can be unit-tested.
pub(crate) fn apply_backlog_series_empty_state_fallback(
    points: &mut Vec<DashboardBacklogPoint>,
    now: DateTime<Utc>,
    granularity: archivist_core::DashboardGranularity,
    counts: &BacklogCounts,
) {
    if points.is_empty() {
        points.push(DashboardBacklogPoint {
            bucket: now,
            label: bucket_label(now, granularity),
            total_documents: counts.total_documents,
            complete: counts.complete,
            open_backlog: counts.total_documents - counts.complete,
            failed: counts.failed,
            waiting_review: counts.waiting_review,
            running: counts.running,
        });
    }
}

/// Tables that the dashboard groups by `status`. The variants are the only valid
/// inputs to [`status_counts`] — we use a closed Rust enum instead of a free-form
/// string so the table name can never originate from caller-controlled data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusTable {
    Jobs,
    PipelineRuns,
    ReviewItems,
}

impl StatusTable {
    /// Static SQL identifier for this table. The returned value is a compile-time
    /// constant — safe to interpolate into queries.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Jobs => "jobs",
            Self::PipelineRuns => "pipeline_runs",
            Self::ReviewItems => "review_items",
        }
    }
}

async fn status_counts(pool: &DbPool, table: StatusTable) -> Result<Vec<DashboardStatusCount>> {
    // SAFETY: `table.name()` is a compile-time constant chosen from a closed enum.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        select status, count(*)::bigint as count
          from {table}
         group by status
         order by count desc, status
        "#,
        table = table.name(),
    )))
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(DashboardStatusCount {
                status: row.try_get("status")?,
                count: row.try_get("count")?,
            })
        })
        .collect()
}

async fn mttc_seconds_value(
    pool: &DbPool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Option<f64>> {
    // Previously this was `avg(finished_at - started_at)` over
    // pipeline_runs, which dwarfed every other dashboard signal: a run can
    // sit in `waiting_review`, in `applying` between user clicks, or pause
    // entirely if the worker is offline, and that wall-clock latency has
    // nothing to do with how long the system spent computing the answer.
    // Real-world deployments saw values like "123 h 53 m" — accurate for
    // wall clock, useless as a processing-time KPI.
    //
    // The honest measurement is the AI compute time per run: sum of
    // `ai_artifacts.duration_ms` across the run, averaged across runs that
    // finished in the window. This is what the user means by "how long
    // does processing a document take" — it ignores human-paced gaps and
    // tracks the actual work.
    let row = sqlx::query(
        r#"
        with per_run as (
          select a.run_id, sum(a.duration_ms)::double precision / 1000.0 as seconds
            from ai_artifacts a
            join pipeline_runs r on r.id = a.run_id
           where r.status = 'succeeded'
             and r.finished_at is not null
             and r.finished_at >= $1
             and r.finished_at < $2
             and a.duration_ms is not null
           group by a.run_id
        )
        select avg(seconds)::double precision as mttc from per_run
        "#,
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
    .context("query mttc")?;
    Ok(row.try_get::<Option<f64>, _>("mttc")?)
}

async fn p95_stage_duration_value(
    pool: &DbPool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Option<i64>> {
    let row = sqlx::query(
        r#"
        select percentile_cont(0.95) within group (order by duration_ms)::bigint as p95
          from ai_artifacts
         where duration_ms is not null
           and created_at >= $1
           and created_at < $2
        "#,
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
    .context("query p95 stage duration")?;
    Ok(row.try_get::<Option<i64>, _>("p95")?)
}

async fn cost_series_tokens(
    pool: &DbPool,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
) -> Result<Vec<DashboardCostBucket>> {
    let granularity = range.granularity();
    let rows = sqlx::query(
        r#"
        with buckets as (
          select generate_series(date_trunc($4, $1), $2, $3::interval) as bucket
        )
        select
          b.bucket,
          -- Typed counters (insert-time + 0040 backfill); no jsonb parsing.
          coalesce((
            select sum(input_tokens)::bigint
              from ai_artifacts
             where created_at >= b.bucket and created_at < b.bucket + $3::interval
          ), 0)::bigint as input_tokens,
          coalesce((
            select sum(output_tokens)::bigint
              from ai_artifacts
             where created_at >= b.bucket and created_at < b.bucket + $3::interval
          ), 0)::bigint as output_tokens,
          coalesce((
            select count(*)::bigint
              from ai_artifacts
             where created_at >= b.bucket and created_at < b.bucket + $3::interval
          ), 0)::bigint as request_count
        from buckets b
        order by b.bucket
        "#,
    )
    .bind(start)
    .bind(now)
    .bind(granularity.interval())
    .bind(granularity.date_trunc())
    .fetch_all(pool)
    .await
    .context("query cost series")?;

    rows.into_iter()
        .map(|row| {
            let bucket: DateTime<Utc> = row.try_get("bucket")?;
            Ok(DashboardCostBucket {
                label: bucket_label(bucket, granularity),
                bucket,
                cost_usd: None,
                request_count: row.try_get("request_count")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
            })
        })
        .collect()
}

async fn needs_attention_items(
    pool: &DbPool,
    safety: &WorkflowSafetyStatus,
    recent_failures: &[DashboardLiveFailure],
) -> Result<Vec<NeedsAttentionItem>> {
    // Pull the two pool-dependent counts first; the rest of the composition
    // is pure and lives in `compose_needs_attention_items` so it can be
    // unit-tested without a database. See the tests module.
    let stuck_runs: i64 = sqlx::query_scalar(
        r#"
        select count(*)::bigint
          from pipeline_runs
         where status = 'running'
           and updated_at < now() - interval '10 minutes'
        "#,
    )
    .fetch_one(pool)
    .await
    .context("count stuck runs")?;
    let stale_leases: i64 = sqlx::query_scalar(
        r#"
        select count(*)::bigint
          from jobs
         where status = 'running'
           and lease_until is not null
           and lease_until < now()
        "#,
    )
    .fetch_one(pool)
    .await
    .context("count stale leases")?;
    let blocked = count_blocked_queued_jobs(pool).await?;
    let active_cooldowns = list_active_provider_cooldowns(pool).await?;

    Ok(compose_needs_attention_items(
        stuck_runs,
        stale_leases,
        safety,
        recent_failures,
        &blocked,
        &active_cooldowns,
    ))
}

/// Pure composition of `NeedsAttentionItem`s from a snapshot of the inputs
/// `needs_attention_items` would otherwise gather from the database. Extracted
/// so the ordering and threshold logic can be unit-tested without a pool.
pub(crate) fn compose_needs_attention_items(
    stuck_runs: i64,
    stale_leases: i64,
    safety: &WorkflowSafetyStatus,
    recent_failures: &[DashboardLiveFailure],
    blocked: &BlockedQueuedCounts,
    active_cooldowns: &[AiProviderCooldown],
) -> Vec<NeedsAttentionItem> {
    let mut items = Vec::new();

    if stuck_runs > 0 {
        items.push(NeedsAttentionItem {
            kind: "stuck_runs".to_owned(),
            severity: "critical".to_owned(),
            title: format!("{stuck_runs} stuck run(s)"),
            description: "Pipeline runs have not progressed in the last 10 minutes.".to_owned(),
            action_key: Some("dashboard.alerts.action.recover_runs".to_owned()),
            count: Some(stuck_runs),
        });
    }

    if stale_leases > 0 {
        items.push(NeedsAttentionItem {
            kind: "stale_leases".to_owned(),
            severity: "warning".to_owned(),
            title: format!("{stale_leases} stale lease(s)"),
            description:
                "Workers hold expired leases. Requeue to let healthy workers pick them up."
                    .to_owned(),
            action_key: Some("dashboard.alerts.action.requeue_leases".to_owned()),
            count: Some(stale_leases),
        });
    }

    // Blocked queued jobs: the claim_jobs filter refuses these because
    // an earlier-stage job in the same run is failed or waiting_review.
    // Surface count + offer the operator-side "unblock" action, which
    // re-queues failed predecessors with attempts=0 so subsequent ticks
    // can pick them up again.
    if blocked.total > 0 {
        items.push(NeedsAttentionItem {
            kind: "blocked_jobs".to_owned(),
            severity: if blocked.blocked_by_failed > 0 {
                "critical".to_owned()
            } else {
                "warning".to_owned()
            },
            title: format!("{} blockierte Job(s)", blocked.total),
            description: format!(
                "{} durch fehlgeschlagene Vorgänger-Stages, {} durch laufende Reviews. \
                 Entsperren stellt die Vorgänger zurück in die Queue, ohne den Run zu verwerfen.",
                blocked.blocked_by_failed, blocked.blocked_by_review
            ),
            action_key: Some("dashboard.alerts.action.unblock_jobs".to_owned()),
            count: Some(blocked.total),
        });
    }

    // Active provider cooldown: the worker hit a usage-cap 429 and
    // suspended the provider. While the cooldown holds, claims for jobs
    // routed to that provider release the lease without burning a retry
    // — so this is mostly informational, unless the operator just
    // upgraded the plan and wants to lift the cooldown manually.
    if !active_cooldowns.is_empty() {
        let provider_list = active_cooldowns
            .iter()
            .map(|c| c.provider_name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        items.push(NeedsAttentionItem {
            kind: "provider_cooldown".to_owned(),
            severity: "critical".to_owned(),
            title: format!("{} provider quota cooldown", active_cooldowns.len()),
            description: format!(
                "AI providers paused due to usage-cap 429: {provider_list}. \
                 Worker will skip jobs routed to these providers until cooldown expires."
            ),
            action_key: Some("dashboard.alerts.action.clear_provider_cooldown".to_owned()),
            count: Some(active_cooldowns.len() as i64),
        });
    }

    if quota_below_threshold(safety.hourly_remaining, safety.hourly_document_limit) {
        items.push(NeedsAttentionItem {
            kind: "quota_low".to_owned(),
            severity: "warning".to_owned(),
            title: "Hourly quota almost exhausted".to_owned(),
            description: "Automatic selection will pause when the hourly limit is reached."
                .to_owned(),
            action_key: Some("dashboard.alerts.action.adjust_limits".to_owned()),
            count: safety.hourly_remaining,
        });
    }
    if quota_below_threshold(safety.daily_remaining, safety.daily_document_limit) {
        items.push(NeedsAttentionItem {
            kind: "quota_low".to_owned(),
            severity: "warning".to_owned(),
            title: "Daily quota almost exhausted".to_owned(),
            description: "Automatic selection will pause when the daily limit is reached."
                .to_owned(),
            action_key: Some("dashboard.alerts.action.adjust_limits".to_owned()),
            count: safety.daily_remaining,
        });
    }

    let hard_failure_count = recent_failures
        .iter()
        .filter(|item| item.failure_kind == "failed")
        .count() as i64;
    if hard_failure_count >= 3 {
        items.push(NeedsAttentionItem {
            kind: "provider_error".to_owned(),
            severity: "warning".to_owned(),
            title: format!("{hard_failure_count} recent failure(s)"),
            description: "Multiple jobs failed recently. Inspect logs or provider availability."
                .to_owned(),
            action_key: Some("dashboard.alerts.action.inspect_failures".to_owned()),
            count: Some(hard_failure_count),
        });
    }

    if safety.dry_run {
        items.push(NeedsAttentionItem {
            kind: "dry_run_active".to_owned(),
            severity: "info".to_owned(),
            title: "Dry-run mode is active".to_owned(),
            description:
                "Validated patches will not be applied to Paperless until dry-run is disabled."
                    .to_owned(),
            action_key: Some("dashboard.alerts.action.disable_dry_run".to_owned()),
            count: None,
        });
    }

    items.sort_by_key(|item| match item.severity.as_str() {
        "critical" => 0,
        "warning" => 1,
        "info" => 2,
        _ => 3,
    });

    items
}

pub(crate) fn quota_below_threshold(remaining: Option<i64>, limit: Option<i64>) -> bool {
    match (remaining, limit) {
        (Some(remaining), Some(limit)) if limit > 0 => {
            let threshold = (limit as f64 * 0.1).ceil() as i64;
            remaining <= threshold.max(1)
        }
        _ => false,
    }
}

fn bucket_label(
    bucket: DateTime<Utc>,
    granularity: archivist_core::DashboardGranularity,
) -> String {
    match granularity {
        archivist_core::DashboardGranularity::Hour => bucket.format("%H:%M").to_string(),
        archivist_core::DashboardGranularity::Day => bucket.format("%d.%m.").to_string(),
        archivist_core::DashboardGranularity::Month => bucket.format("%Y-%m").to_string(),
    }
}

/// Atomically bump a monotone metric counter, creating the row on first use.
///
/// Unlike the `audit_events`-derived gauges in [`metrics_snapshot`], these
/// counters live in their own table and are therefore unaffected by audit
/// retention pruning, keeping the `/metrics` series monotone and `rate()`-safe.
/// Increment exactly once at the source event so the counter never double-counts.
pub async fn increment_metric_counter(pool: &DbPool, name: &str, by: i64) -> Result<()> {
    sqlx::query(
        r#"
        insert into metrics_counters (name, value, updated_at)
        values ($1, $2, now())
        on conflict (name) do update
           set value = metrics_counters.value + excluded.value,
               updated_at = now()
        "#,
    )
    .bind(name)
    .bind(by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Transaction-scoped variant of [`increment_metric_counter`] so a counter bump
/// can be made atomic with the source event written in the same transaction.
pub(crate) async fn increment_metric_counter_tx(
    tx: &mut Transaction<'_, Postgres>,
    name: &str,
    by: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into metrics_counters (name, value, updated_at)
        values ($1, $2, now())
        on conflict (name) do update
           set value = metrics_counters.value + excluded.value,
               updated_at = now()
        "#,
    )
    .bind(name)
    .bind(by)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Read all monotone metric counters as a `name -> value` map.
pub async fn read_metric_counters(pool: &DbPool) -> Result<HashMap<String, i64>> {
    let rows = sqlx::query("select name, value from metrics_counters")
        .fetch_all(pool)
        .await?;
    let mut counters = HashMap::with_capacity(rows.len());
    for row in rows {
        let name: String = row.try_get("name")?;
        let value: i64 = row.try_get("value")?;
        counters.insert(name, value);
    }
    Ok(counters)
}

pub async fn metrics_snapshot(pool: &DbPool) -> Result<MetricsSnapshot> {
    let row = sqlx::query(
        concat!(r#"
        select
          (select count(*)::bigint from jobs where status = 'queued') as jobs_queued,
          (select count(*)::bigint from jobs where status = 'running') as jobs_running,
          (select count(*)::bigint from jobs where status = 'failed') as jobs_failed,
          (select count(*)::bigint from jobs where status = 'succeeded') as jobs_succeeded,
          (select count(*)::bigint from review_items where status = 'pending') as reviews_pending,
          (select count(*)::bigint from pipeline_runs where status in ("#,
sql_active_run_statuses!(),
r#")) as runs_active,
          -- Total event count keeps an exact count(*); switch to a pg_class
          -- reltuples estimate if this scan ever becomes a hot spot.
          -- Planner estimate instead of a full count(*): audit_events is
          -- unbounded and this query runs on every /metrics scrape.
          (select greatest(reltuples, 0)::bigint from pg_class where oid = 'audit_events'::regclass) as audit_events,
          (select count(*)::bigint from audit_events where event_type = 'workflow.selector_ran') as selector_runs_total,
          coalesce((
            select sum(coalesce((after ->> 'queued')::bigint, 0))::bigint
              from audit_events
             where event_type = 'workflow.selector_ran'
          ), 0) as selector_documents_queued_total,
          (select count(*)::bigint from audit_events where event_type = 'job.retry_scheduled') as job_retries_scheduled_total,
          -- LLM stages only. Pre-v1.4 used six per-field stages; the live
          -- Stage enum is ocr / metadata / apply, and metadata (the main LLM
          -- stage) was missing here so its errors were invisible to the
          -- metric. apply is a Paperless PATCH, not a model call. #262.
          (select count(*)::bigint
             from jobs
            where error_message is not null
              and stage in ('ocr', 'metadata')
          ) as model_errors_total,
          (select count(*)::bigint from audit_events where event_type = 'document.patch_applied' and outcome = 'success') as apply_success_total,
          (select count(*)::bigint from audit_events where event_type = 'document.patch_apply_failed' and outcome = 'failed') as apply_failure_total,
          -- Latency aggregates are scoped to a recent window so they no longer
          -- scan the entire unbounded audit_events table; the
          -- (event_type, created_at) index covers this access path.
          coalesce((
            select count(*)::bigint
              from audit_events
             where event_type in ('document.patch_applied', 'document.patch_apply_failed')
               and created_at > now() - interval '24 hours'
               and metadata ? 'duration_ms'
          ), 0) as apply_latency_ms_count,
          coalesce((
            select sum((metadata ->> 'duration_ms')::bigint)::bigint
              from audit_events
             where event_type in ('document.patch_applied', 'document.patch_apply_failed')
               and created_at > now() - interval '24 hours'
               and metadata ? 'duration_ms'
          ), 0) as apply_latency_ms_sum,
          coalesce((
            select (percentile_disc(0.95) within group (order by (metadata ->> 'duration_ms')::bigint))::bigint
              from audit_events
             where event_type in ('document.patch_applied', 'document.patch_apply_failed')
               and created_at > now() - interval '24 hours'
               and metadata ? 'duration_ms'
          ), 0) as apply_latency_ms_p95,
          -- Per-stage latency is sourced from ai_artifacts.duration_ms (the
          -- recorded job timing for each stage round-trip), scoped to the same
          -- 24h window as the apply latency aggregates above.
          coalesce((
            select count(*)::bigint
              from ai_artifacts
             where stage = 'ocr'
               and duration_ms is not null
               and created_at > now() - interval '24 hours'
          ), 0) as ocr_latency_ms_count,
          coalesce((
            select (percentile_disc(0.95) within group (order by duration_ms))::bigint
              from ai_artifacts
             where stage = 'ocr'
               and duration_ms is not null
               and created_at > now() - interval '24 hours'
          ), 0) as ocr_latency_ms_p95,
          coalesce((
            select count(*)::bigint
              from ai_artifacts
             where stage = 'metadata'
               and duration_ms is not null
               and created_at > now() - interval '24 hours'
          ), 0) as metadata_latency_ms_count,
          coalesce((
            select (percentile_disc(0.95) within group (order by duration_ms))::bigint
              from ai_artifacts
             where stage = 'metadata'
               and duration_ms is not null
               and created_at > now() - interval '24 hours'
          ), 0) as metadata_latency_ms_p95,
          -- Oldest backlog age: how long the earliest queued job has been
          -- waiting (now() - min(run_after) over status='queued'). Null when
          -- the queue is empty, coalesced to 0.
          coalesce((
            select extract(epoch from (now() - min(run_after)))::bigint
              from jobs
             where status = 'queued'
          ), 0) as oldest_queued_age_seconds
        "#),
    )
    .fetch_one(pool)
    .await?;
    Ok(MetricsSnapshot {
        jobs_queued: row.try_get("jobs_queued")?,
        jobs_running: row.try_get("jobs_running")?,
        jobs_failed: row.try_get("jobs_failed")?,
        jobs_succeeded: row.try_get("jobs_succeeded")?,
        reviews_pending: row.try_get("reviews_pending")?,
        runs_active: row.try_get("runs_active")?,
        audit_events: row.try_get("audit_events")?,
        selector_runs_total: row.try_get("selector_runs_total")?,
        selector_documents_queued_total: row.try_get("selector_documents_queued_total")?,
        job_retries_scheduled_total: row.try_get("job_retries_scheduled_total")?,
        model_errors_total: row.try_get("model_errors_total")?,
        apply_success_total: row.try_get("apply_success_total")?,
        apply_failure_total: row.try_get("apply_failure_total")?,
        apply_latency_ms_count: row.try_get("apply_latency_ms_count")?,
        apply_latency_ms_sum: row.try_get("apply_latency_ms_sum")?,
        apply_latency_ms_p95: row.try_get("apply_latency_ms_p95")?,
        ocr_latency_ms_count: row.try_get("ocr_latency_ms_count")?,
        ocr_latency_ms_p95: row.try_get("ocr_latency_ms_p95")?,
        metadata_latency_ms_count: row.try_get("metadata_latency_ms_count")?,
        metadata_latency_ms_p95: row.try_get("metadata_latency_ms_p95")?,
        oldest_queued_age_seconds: row.try_get("oldest_queued_age_seconds")?,
    })
}
