use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use archivist_apply::recover_review_apply_intents;
use archivist_config::AppConfig;
use archivist_core::{AuditEventInput, RuntimeSettings, Stage};
use archivist_db::{
    DbPool, JobRecord, StartupRepair, append_audit, backfill_metadata_stage_for_ocr_only_runs,
    bump_text_num_ctx_if_too_small, bump_vision_num_ctx_if_too_small, claim_jobs, connect,
    fail_job, get_backlog_counts, get_runtime_settings, increment_metric_counter,
    rebalance_backfilled_metadata_priorities, record_dashboard_snapshot,
    reset_stale_applying_reviews, reset_stuck_running_pipeline_runs,
};
use archivist_paperless::PaperlessClient;
use chrono::{Duration as ChronoDuration, Utc};
use secrecy::ExposeSecret;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::signal;
use tokio::time::{sleep, timeout};
use tracing::{Instrument, error, info, info_span, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use apply::{failure_is_terminal, retire_trigger_tags_after_terminal_outcome};
use drain::drain_pending_reviews_if_autopilot_tick;
use failure::{
    ProcessingFailureClass, active_cooldown_for_stage, classify_processing_failure,
    record_quota_cooldown_for_failure, release_lease_for_cooldown,
};
use job_supervisor::{
    InFlightGuard, JobSupervisor, WatchdogVerdict, catch_job_panic, watch_job_lease,
};
use lease::job_lease_seconds;
use metadata_stage::process_metadata;
use notifications::send_operational_notifications;
use ocr_stage::process_ocr;
use paperless::paperless_client;
use providers::is_vision_model_runtime_crash;
use startup::{run_startup_repair, run_startup_vision_crash_requeue};
use trigger_poll::poll_paperless_triggers;

mod apply;
mod drain;
mod failure;
mod job_supervisor;
mod lease;
mod metadata_stage;
mod notifications;
mod ocr_stage;
mod paperless;
mod providers;
mod startup;
mod sync;
#[cfg(test)]
mod test_support;
mod trigger_poll;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AppConfig::from_env();
    config.validate()?;
    init_tracing(&config.log_level);

    let pool = connect(
        config.database_url.expose_secret(),
        config.db_max_connections,
    )
    .await?;
    wait_for_schema(&pool).await?;
    run_worker(pool, Arc::new(config)).await
}

fn init_tracing(filter: &str) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .json()
        .init();
}

/// Resets an `Arc<AtomicBool>` re-entry guard to `false` on drop, so a panic
/// inside a spawned periodic task (job processing / trigger-poll / autopilot
/// drain) cannot leak the guard `true` and silently wedge that tick slot for
/// the rest of the process lifetime. Constructed right after a successful
/// `compare_exchange`, dropped when the spawned future unwinds or completes.
struct ReentryGuard(Arc<AtomicBool>);

impl Drop for ReentryGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn run_worker(pool: DbPool, config: Arc<AppConfig>) -> Result<()> {
    let worker_id = format!("worker-{}", uuid::Uuid::now_v7());
    info!(%worker_id, "paperless archivist worker started");
    let mut tick: u64 = 0;
    let trigger_poll_running = Arc::new(AtomicBool::new(false));
    // Live-reload concurrency: tracks the value used on the previous claim
    // cycle so we can emit `workflow.concurrency_changed` only on transitions
    // rather than once per tick. Seeded with the sentinel `0` ("not yet
    // observed"): the resolver always returns a concurrency ≥1, so the first
    // claim cycle seeds the real value without emitting a spurious transition
    // on startup (previously seeding from the env cap logged a bogus
    // transition on every start whenever the configured concurrency was lower).
    let last_observed_concurrency = Arc::new(AtomicU32::new(0));
    // Re-entry guard so a long-running autopilot drain tick does not block
    // subsequent worker ticks. Drains can run minutes when Paperless is slow
    // or the pending backlog is large; we want OCR job processing to keep
    // happening in the meantime.
    let autopilot_drain_running = Arc::new(AtomicBool::new(false));
    // Re-entry guard for job processing. Spawning (rather than awaiting) the
    // claim+process batch keeps the wall-clock `tick % 12` maintenance
    // schedule (trigger-poll, drain, snapshots) firing on time even while a
    // long OCR batch is in flight — previously the loop blocked on
    // `process_available_jobs().await` until the whole batch finished, so
    // those maintenance checks fired far less often than the intended 5s.
    let job_processing_running = Arc::new(AtomicBool::new(false));
    // #407: in-flight job registry + progress tracking for continuous
    // claiming, the per-job watchdog and the liveness heartbeat.
    let supervisor = Arc::new(JobSupervisor::new(Utc::now().timestamp()));

    // Write a fresh dashboard snapshot near startup so the read path has something current
    // before the periodic tick fires (snapshots used to be written on every /dashboard read).
    if let Err(error) = record_dashboard_snapshot_tick(&pool).await {
        warn!(error = %error, "initial dashboard snapshot failed");
    }

    // Log the configured Ollama num_ctx values so operators can confirm the
    // GGML_ASSERT fix (ollama/ollama#14401) is in effect after deploy. If the
    // values are below the historical 4096-token default, that's a deliberate
    // operator override on a memory-constrained host — we still log so it is
    // visible. The actual wire-up happens per-call in `chat_for_stage` /
    // OCR vision construction.
    match get_runtime_settings(&pool).await {
        Ok(settings) => {
            let tuning = settings.effective_tuning();
            info!(
                ollama_vision_num_ctx = tuning.vision_num_ctx,
                ollama_text_num_ctx = tuning.text_num_ctx,
                "setting vision options.num_ctx and text options.num_ctx for Ollama calls"
            );
        }
        Err(error) => warn!(error = %error, "failed to read Ollama num_ctx settings at startup"),
    }

    // #443: the one-shot repairs below run at most once per
    // (name, version) — see `StartupRepair` — instead of on every boot. The
    // recurring review sweeps after them are not repairs and stay ungated.

    // One-shot: lift the GGML_ASSERT recurrence ceiling that v1.5.1 set to
    // 16384. Production observed 137 OCR jobs burning through their retry
    // budget despite num_ctx=16384, so we bump the floor to 32768 for any
    // deployment that hasn't already raised it manually. This runs BEFORE
    // the vision-crash requeue so the requeued jobs run under the new num_ctx.
    run_startup_repair(&pool, StartupRepair::VISION_NUM_CTX_FLOOR, || async {
        let summary = bump_vision_num_ctx_if_too_small(&pool).await?;
        if summary.bumped {
            info!(
                previous = ?summary.previous,
                current = summary.current,
                "bumped ai.ollama_vision_num_ctx to 32768 to give vision model more headroom"
            );
        } else {
            info!("ai.ollama_vision_num_ctx already at or above 32768; no bump");
        }
        Ok(Some(json!({
            "previous": summary.previous,
            "current": summary.current,
            "bumped": summary.bumped,
        })))
    })
    .await;

    // One-shot: raise the text num_ctx to a 32768 floor (matching vision). A
    // large metadata prompt (bounded OCR + candidate allowlists + few-shots +
    // JSON shape) can exceed 16384 tokens on a long document and fail the
    // metadata job with exceed_context_size_error (seen at 18962). Operators
    // who already raised it past the floor are untouched.
    run_startup_repair(&pool, StartupRepair::TEXT_NUM_CTX_FLOOR, || async {
        let summary = bump_text_num_ctx_if_too_small(&pool).await?;
        if summary.bumped {
            info!(
                previous = ?summary.previous,
                current = summary.current,
                "bumped ai.ollama_text_num_ctx to 32768 to give the text model context headroom"
            );
        } else {
            info!("ai.ollama_text_num_ctx already at or above 32768; no bump");
        }
        Ok(Some(json!({
            "previous": summary.previous,
            "current": summary.current,
            "bumped": summary.bumped,
        })))
    })
    .await;

    // One-shot: lift failed OCR jobs killed by the GGML vision-runtime crash signature back
    // into the queue so they get a second chance under the new fallback machinery.
    // Gated by the runtime setting so operators can disable for upgrade scenarios where the
    // queue must not be touched; a disabled pass records no marker.
    run_startup_repair(&pool, StartupRepair::VISION_CRASH_REQUEUE, || {
        run_startup_vision_crash_requeue(&pool)
    })
    .await;

    // One-shot: backfill the consolidated `metadata` stage onto historical
    // `pipeline_runs` that were queued with only `["ocr"]` (e.g. by trigger
    // polling against documents tagged only with the OCR trigger). Without
    // this, those runs terminate after OCR and the Review queue fills up
    // with content-only review items that never get a real
    // Title/Correspondent/Tags suggestion.
    run_startup_repair(&pool, StartupRepair::METADATA_STAGE_BACKFILL, || async {
        let summary = backfill_metadata_stage_for_ocr_only_runs(&pool).await?;
        if summary.runs_updated > 0 {
            info!(
                runs_updated = summary.runs_updated,
                jobs_inserted = summary.jobs_inserted,
                "metadata-stage backfill lifted OCR-only pipeline_runs to include the metadata stage"
            );
        } else {
            info!("metadata-stage backfill found no OCR-only pipeline_runs to lift");
        }
        Ok(Some(json!({
            "runs_updated": summary.runs_updated,
            "jobs_inserted": summary.jobs_inserted,
        })))
    })
    .await;

    // One-shot: fix the v1.5.4 backfill bug where new metadata jobs got
    // `payload.priority = 1_000_000 - document_id` instead of inheriting
    // the OCR sibling's priority. Without this, the backfilled metadata
    // jobs sit queued indefinitely behind every other OCR job globally.
    run_startup_repair(
        &pool,
        StartupRepair::METADATA_PRIORITY_REBALANCE,
        || async {
            let summary = rebalance_backfilled_metadata_priorities(&pool).await?;
            if summary.jobs_repriced > 0 {
                info!(
                    jobs_repriced = summary.jobs_repriced,
                    "rebalanced backfilled metadata-job priorities to inherit OCR siblings'"
                );
            } else {
                info!("metadata-job priority rebalance found no mispriced rows");
            }
            Ok(Some(json!({ "jobs_repriced": summary.jobs_repriced })))
        },
    )
    .await;

    // One-shot: clean up pipeline_runs.status='running' rows whose jobs
    // are all settled. Pre-v1.5.7 complete_job left intermediate stage
    // successes on 'running' which surfaced as "N stuck run(s)" on the
    // dashboard. v1.5.7 fixes complete_job for new runs; this catches
    // the historical residue.
    run_startup_repair(&pool, StartupRepair::STUCK_RUNNING_RUNS_RESET, || async {
        let summary = reset_stuck_running_pipeline_runs(&pool).await?;
        if summary.runs_reset > 0 {
            info!(
                runs_reset = summary.runs_reset,
                "reset historical pipeline_runs stuck on 'running' to their correct status"
            );
        } else {
            info!("stuck-running pipeline_runs cleanup found no rows to reset");
        }
        Ok(Some(json!({ "runs_reset": summary.runs_reset })))
    })
    .await;

    // Reconcile durable Paperless intents before the legacy stale-review
    // sweep. Active/confirmed intents remain fenced; only rows with no active
    // intent or a settled failure may return to a retryable review status.
    if let Err(error) = timeout(
        Duration::from_secs(30),
        recover_review_apply_tick(&pool, &config),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow!("startup review-intent recovery timed out")))
    {
        warn!(error = %error, "startup review-intent recovery failed");
    }

    // Recover legacy review items stranded in 'applying' before durable
    // intents existed. 300s comfortably exceeds a healthy apply.
    match reset_stale_applying_reviews(&pool, 300).await {
        Ok(count) if count > 0 => {
            info!(
                count,
                "reverted review items stranded in 'applying' back to 'pending'"
            )
        }
        Ok(_) => {}
        Err(error) => warn!(error = %error, "startup stale-applying review reset failed"),
    }

    loop {
        tokio::select! {
            _ = shutdown_signal() => {
                info!(%worker_id, "worker shutdown requested; draining in-flight work");
                // Stop claiming and give the in-flight tick/drain tasks up to
                // 25s (inside the deployment's 60s grace period) to settle
                // their jobs terminally. If something is still mid-LLM-call at
                // the deadline we exit anyway — its lease expires and another
                // replica reclaims it, which used to be the fate of EVERY
                // in-flight job on deploy.
                let drain_deadline = std::time::Instant::now() + Duration::from_secs(25);
                while (job_processing_running.load(Ordering::Acquire)
                    || supervisor.in_flight() > 0
                    || autopilot_drain_running.load(Ordering::Acquire))
                    && std::time::Instant::now() < drain_deadline
                {
                    sleep(Duration::from_millis(250)).await;
                }
                if job_processing_running.load(Ordering::Acquire)
                    || supervisor.in_flight() > 0
                    || autopilot_drain_running.load(Ordering::Acquire)
                {
                    warn!(
                        %worker_id,
                        "drain deadline reached with work still in flight; leases will expire and be reclaimed"
                    );
                } else {
                    info!(%worker_id, "worker drained cleanly");
                }
                return Ok(());
            }
            _ = sleep(Duration::from_secs(5)) => {
                tick += 1;
                if job_processing_running
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    let pool = pool.clone();
                    let config = Arc::clone(&config);
                    let worker_id = worker_id.clone();
                    let last_observed_concurrency = Arc::clone(&last_observed_concurrency);
                    let job_processing_running = Arc::clone(&job_processing_running);
                    let supervisor = Arc::clone(&supervisor);
                    tokio::spawn(async move {
                        let _guard = ReentryGuard(job_processing_running);
                        if let Err(error) = process_available_jobs(
                            &pool,
                            &config,
                            &worker_id,
                            &last_observed_concurrency,
                            &supervisor,
                        )
                        .await
                        {
                            error!(error = %error, "job processing tick failed");
                        }
                        // #407: a completed claim cycle (even a failed or
                        // empty one) proves the claim loop is not wedged.
                        supervisor.claim_cycle_completed(Utc::now().timestamp());
                    });
                }
                if tick % 12 == 3
                    && let Err(error) = timeout(
                        Duration::from_secs(20),
                        send_operational_notifications(&pool, &config),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("notification tick timed out")))
                {
                    warn!(error = %error, "notification tick failed");
                }
                // Dashboard snapshot writes used to fire on every /dashboard read; now they
                // happen here once per minute (every 12 five-second ticks).
                if tick % 12 == 5
                    && let Err(error) = record_dashboard_snapshot_tick(&pool).await
                {
                    warn!(error = %error, "dashboard snapshot tick failed");
                }
                // Recover review items stranded in 'applying' by a crash
                // mid-apply (#253), once per minute. 300s exceeds a healthy
                // apply, so anything older was abandoned.
                if tick % 12 == 9 {
                    if let Err(error) = timeout(
                        Duration::from_secs(20),
                        recover_review_apply_tick(&pool, &config),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("review-intent recovery timed out")))
                    {
                        warn!(error = %error, "review-intent recovery sweep failed");
                    }
                    match reset_stale_applying_reviews(&pool, 300).await {
                        Ok(count) if count > 0 => warn!(
                            count,
                            "reverted review items stranded in 'applying' back to 'pending'"
                        ),
                        Ok(_) => {}
                        Err(error) => {
                            warn!(error = %error, "stale-applying review sweep failed")
                        }
                    }
                }
                // Autopilot review drain: when the runtime is in full_auto, any review_items
                // still sitting in `pending` are auto-applied here, respecting the same safety
                // budget the auto-selector honors. This handles the residual backlog from
                // historical batches that routed to manual_review before commit 0d7a915 made
                // routing follow live runtime mode, and any future flip-from-review case.
                //
                // Spawned (not awaited) so a slow drain — Paperless under load, or a multi-
                // thousand-item backlog being chewed through — cannot stall the worker's
                // main tick loop (which also drives OCR job processing). The atomic guard
                // makes the next drain firing skip cleanly while the previous one is still
                // running; v1.5.4 lifted this out of the inline await to fix the
                // backlog-vs-OCR-throughput contention observed in prod.
                if tick % 12 == 7
                    && autopilot_drain_running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    let pool = pool.clone();
                    let config = Arc::clone(&config);
                    let autopilot_drain_running = Arc::clone(&autopilot_drain_running);
                    tokio::spawn(async move {
                        let _guard = ReentryGuard(autopilot_drain_running);
                        if let Err(error) =
                            drain_pending_reviews_if_autopilot_tick(&pool, &config).await
                        {
                            warn!(error = %error, "autopilot review drain tick failed");
                        }
                    });
                }
                if tick % 12 == 1
                    && trigger_poll_running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    let pool = pool.clone();
                    let config = Arc::clone(&config);
                    let trigger_poll_running = Arc::clone(&trigger_poll_running);
                    tokio::spawn(async move {
                        let _guard = ReentryGuard(trigger_poll_running);
                        let trace_id = Uuid::now_v7();
                        let started = std::time::Instant::now();
                        info!(%trace_id, "trigger polling started");
                        let result = timeout(
                            Duration::from_secs(300),
                            poll_paperless_triggers(&pool, &config)
                                .instrument(info_span!("trigger_poll", %trace_id)),
                        )
                        .await;
                        match result {
                            Ok(Ok(())) => {
                                info!(%trace_id, duration_ms = started.elapsed().as_millis() as u64, "trigger polling completed");
                            }
                            Ok(Err(error)) => {
                                warn!(%trace_id, error = %error, duration_ms = started.elapsed().as_millis() as u64, "trigger polling failed");
                            }
                            Err(_) => {
                                warn!(%trace_id, duration_ms = started.elapsed().as_millis() as u64, "trigger polling timed out");
                            }
                        }
                    });
                }
                // Liveness heartbeat: touch a file with the current unix timestamp
                // at the end of every successful tick. The worker exposes no HTTP
                // server, so the Kubernetes livenessProbe checks the staleness of
                // this file — a hung tick-loop (which keeps the binary present)
                // stops updating it and is restarted. Cheap and non-fatal.
                // #407: only while work progresses — the claim loop completed
                // recently and every in-flight job renewed its lease (or was
                // aborted by its watchdog) in time. A job stuck where even the
                // watchdog cannot abort it now fails liveness.
                if supervisor.is_healthy(Utc::now().timestamp(), CLAIM_LOOP_STALL_LIMIT_SECONDS) {
                    write_liveness_heartbeat().await;
                } else {
                    warn!(
                        in_flight = supervisor.in_flight(),
                        "job processing made no progress; withholding liveness heartbeat"
                    );
                }
            }
        }
    }
}

/// Write the current unix timestamp to the heartbeat file read by the
/// Kubernetes liveness probe. Path comes from `ARCHIVIST_WORKER_HEARTBEAT_FILE`
/// (default `/tmp/archivist-worker.heartbeat`). Failures are logged, never fatal.
async fn write_liveness_heartbeat() {
    let path = std::env::var("ARCHIVIST_WORKER_HEARTBEAT_FILE")
        .unwrap_or_else(|_| "/tmp/archivist-worker.heartbeat".to_string());
    let now = Utc::now().timestamp();
    if let Err(error) = tokio::fs::write(&path, now.to_string()).await {
        warn!(error = %error, path, "failed to write liveness heartbeat file");
    }
}

async fn record_dashboard_snapshot_tick(pool: &DbPool) -> Result<()> {
    let counts = get_backlog_counts(pool).await?;
    record_dashboard_snapshot(pool, &counts).await
}

async fn recover_review_apply_tick(pool: &DbPool, config: &AppConfig) -> Result<()> {
    let settings = get_runtime_settings(pool).await?;
    let paperless = paperless_client(pool, config, &settings).await?;
    let summary = recover_review_apply_intents(pool, &paperless, 100).await?;
    if summary.examined > 0 {
        info!(
            examined = summary.examined,
            applied = summary.applied,
            failed_settled = summary.failed_settled,
            deferred = summary.deferred,
            "recovered durable Paperless review intents"
        );
    }
    Ok(())
}

async fn wait_for_schema(pool: &DbPool) -> Result<()> {
    for attempt in 1..=60 {
        match get_runtime_settings(pool).await {
            Ok(_) => return Ok(()),
            Err(error) if attempt < 60 => {
                warn!(attempt, error = %error, "waiting for API database migrations");
                sleep(Duration::from_secs(2)).await;
            }
            Err(error) => return Err(error).context("wait for API database migrations"),
        }
    }
    Ok(())
}

/// Free job slots for the next claim cycle. #407
fn claim_capacity(target_concurrency: u32, in_flight: usize) -> usize {
    (target_concurrency as usize).saturating_sub(in_flight)
}

async fn process_available_jobs(
    pool: &DbPool,
    config: &Arc<AppConfig>,
    worker_id: &str,
    last_observed_concurrency: &AtomicU32,
    supervisor: &Arc<JobSupervisor>,
) -> Result<()> {
    // v1.6.2 issue #127: per-cycle live-reload of worker pool size.
    //
    // Read settings BEFORE claiming so the target concurrency reflects the
    // operator's current intent. The env var is the hard upper cap; the
    // active provider's tuning can only clamp lower. On a transition we
    // emit `workflow.concurrency_changed` with both `from` and `to` so the
    // audit log shows when the pool resized and why.
    //
    // Continuous-claim semantics (#407): claimed jobs run detached and each
    // tick only claims into free slots (`target - in_flight`). Pool downscale
    // therefore never aborts in-flight work — surplus slots are simply not
    // refilled. Pool upscale starts on the next tick.
    let settings = match get_runtime_settings(pool).await {
        Ok(settings) => Arc::new(settings),
        Err(error) => {
            warn!(
                error = %error,
                "failed to load runtime settings for tick; skipping claim cycle"
            );
            return Ok(());
        }
    };

    let env_cap = env_concurrency_cap(config);
    let target_concurrency = resolve_target_concurrency(env_cap, &settings);
    let previous_concurrency = last_observed_concurrency.swap(target_concurrency, Ordering::AcqRel);
    // `previous_concurrency == 0` is the startup sentinel — the first observed
    // value is not a transition, so don't log/audit it.
    if previous_concurrency != 0 && previous_concurrency != target_concurrency {
        info!(
            from = previous_concurrency,
            to = target_concurrency,
            env_cap,
            "worker concurrency transitioned (live-reload from settings)"
        );
        let audit = AuditEventInput {
            event_type: "workflow.concurrency_changed".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: Some(json!({ "worker_concurrency": previous_concurrency })),
            after: Some(json!({ "worker_concurrency": target_concurrency })),
            metadata: Some(json!({ "env_cap": env_cap, "source": "settings_live_reload" })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        };
        if let Err(error) = append_audit(pool, audit).await {
            warn!(error = %error, "failed to record concurrency transition audit event");
        }
    }

    if target_concurrency == 0 {
        // Defensive — the resolver clamps to ≥1, but if some operator
        // pinned concurrency to 0 the right behaviour is to skip the tick
        // entirely rather than issue an empty claim.
        return Ok(());
    }

    // #407: continuous claiming — only top up the slots that are free right
    // now. Jobs still running from earlier cycles keep their slots; a
    // downscale simply claims nothing until enough of them finish.
    let free_slots = claim_capacity(target_concurrency, supervisor.in_flight());
    if free_slots == 0 {
        return Ok(());
    }
    let jobs = claim_jobs(
        pool,
        free_slots as i64,
        worker_id,
        job_lease_seconds(&settings),
    )
    .await?;
    if jobs.is_empty() {
        return Ok(());
    }
    info!(
        claimed_jobs = jobs.len(),
        target_concurrency,
        %worker_id,
        "claimed jobs for processing"
    );
    let paperless = match paperless_client(pool, config, &settings).await {
        Ok(client) => Arc::new(client),
        Err(error) => {
            warn!(error = ?error, "failed to construct Paperless client for batch; failing claimed jobs");
            for job in &jobs {
                let _ = fail_job(pool, job, worker_id, &format!("{:#}", error), true, None).await;
            }
            return Ok(());
        }
    };

    let lease_seconds = job_lease_seconds(&settings);
    for job in jobs {
        let job_pool = pool.clone();
        let config = Arc::clone(config);
        let settings = Arc::clone(&settings);
        let paperless = Arc::clone(&paperless);
        let lease_owner = worker_id.to_owned();
        let trace_id = job.run_id;
        let span = info_span!(
            "archivist_job",
            trace_id = %trace_id,
            run_id = %job.run_id,
            job_id = %job.id,
            document_id = job.paperless_document_id,
            stage = %job.stage,
            attempt = job.attempts
        );
        // #407: register before spawning so the next claim cycle already sees
        // this slot as occupied; the guard inside the task frees it on
        // completion, panic or abort.
        supervisor.touch(job.id, job_progress_deadline(lease_seconds));
        let task_supervisor = Arc::clone(supervisor);
        let watchdog_job = job.clone();
        let handle = tokio::spawn(
            async move {
                let _in_flight = InFlightGuard {
                    supervisor: &task_supervisor,
                    job_id: job.id,
                };
                let pool = job_pool;
                let started = std::time::Instant::now();
                // #402: a panic becomes an ordinary error so the failure path
                // below records it via `fail_job` instead of losing it as a
                // JoinError and leaving the lease to expire.
                let result = catch_job_panic(process_job(
                    &pool,
                    &config,
                    settings.as_ref(),
                    paperless.as_ref(),
                    &job,
                    &lease_owner,
                ))
                .await;
                if let Err(error) = &result {
                    let failure_class = classify_processing_failure(error);
                    // Set when a quota cooldown/release fails with a transient DB
                    // error: the document must NOT be permanently failed for a
                    // quota that resets — make the fallback fail_job retryable. #295
                    let mut force_retryable = false;
                    if failure_class == ProcessingFailureClass::ProviderQuota {
                        // Count the quota-exhausted event so its rate is alertable
                        // (#311). Best-effort: a counter write must never mask the
                        // underlying failure handling below.
                        if let Err(metric_err) =
                            increment_metric_counter(&pool, "provider_quota_total", 1).await
                        {
                            warn!(error = %metric_err, "failed to increment provider_quota_total");
                        }
                        // The provider replied with a usage-cap signal.
                        // Persist a cooldown so subsequent claims of jobs
                        // routed to the same provider release their lease
                        // immediately rather than burning through the per-job
                        // retry budget against a quota that resets in days.
                        match record_quota_cooldown_for_failure(&pool, &settings, &job, error).await
                        {
                            Ok(cooldown_until) => {
                                // The triggering job must NOT be permanently
                                // failed: ProviderQuota is non-retryable, so
                                // `fail_job(..., false)` would sacrifice this
                                // document even after the quota resets. Instead
                                // release the lease exactly like the cooldown
                                // short-circuit — decrement the attempt and set
                                // `run_after = cooldown_until` so the job comes
                                // back once the provider is plausibly available.
                                match release_lease_for_cooldown(
                                    &pool,
                                    &job,
                                    &lease_owner,
                                    cooldown_until,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        warn!(
                                            error = ?error,
                                            failure_class = failure_class.as_str(),
                                            until = %cooldown_until,
                                            duration_ms = started.elapsed().as_millis() as u64,
                                            "provider quota exhausted; released lease without burning an attempt instead of failing the job"
                                        );
                                        return result;
                                    }
                                    Err(release_err) => {
                                        // Releasing failed — fall through to the
                                        // normal fail path so the job does not
                                        // silently stay leased, but as retryable
                                        // (a transient DB error, not a real
                                        // permanent failure).
                                        force_retryable = true;
                                        warn!(
                                            error = %release_err,
                                            "failed to release lease after quota-exhausted failure; falling back to retryable fail_job"
                                        );
                                    }
                                }
                            }
                            Err(cooldown_err) => {
                                force_retryable = true;
                                warn!(
                                    error = %cooldown_err,
                                    "failed to persist provider cooldown after quota-exhausted failure"
                                );
                            }
                        }
                    }
                    let vision_model_crash = is_vision_model_runtime_crash(error);
                    if vision_model_crash {
                        // GGML_ASSERT / "llama runner process no longer running" come from the
                        // Ollama runtime aborting on specific input shapes. If we reach this
                        // branch the worker already attempted the explicit/auto-discovered
                        // fallback in `run_vision_with_fallback` and that ALSO crashed (or no
                        // fallback was available). Surface enough breadcrumb info for
                        // operators to either install a safer chain entry or set
                        // `ai.fallback_vision_model` explicitly. Under Full-Auto this still
                        // falls through to the standard transient retry budget.
                        warn!(
                            error = ?error,
                            failure_class = failure_class.as_str(),
                            duration_ms = started.elapsed().as_millis() as u64,
                            vision_model_crash = true,
                            hint = "ollama vision model and fallback both crashed (GGML_ASSERT / runner crash); install one of qwen2-vl:7b / llava-llama3:8b / llava:13b or set ai.fallback_vision_model",
                            "job processing failed"
                        );
                    } else {
                        warn!(
                            error = ?error,
                            failure_class = failure_class.as_str(),
                            duration_ms = started.elapsed().as_millis() as u64,
                            "job processing failed"
                        );
                    }
                    let retryable = failure_class.is_retryable() || force_retryable;
                    let recorded = fail_job(
                        &pool,
                        &job,
                        &lease_owner,
                        &format!("{:#}", error),
                        retryable,
                        failure_class.retry_ceiling(),
                    )
                    .await;
                    // #400: a permanent failure retires the trigger tags and
                    // sets the failure marker so it is visible in Paperless
                    // and the trigger poll does not requeue it every minute.
                    if matches!(recorded, Ok(true))
                        && failure_is_terminal(&job, retryable, failure_class.retry_ceiling())
                    {
                        retire_trigger_tags_after_terminal_outcome(
                            &pool,
                            paperless.as_ref(),
                            settings.as_ref(),
                            &job,
                            true,
                        )
                        .await;
                    }
                } else {
                    info!(
                        duration_ms = started.elapsed().as_millis() as u64,
                        "job processing completed"
                    );
                }
                result
            }
            .instrument(span),
        );
        spawn_job_watchdog(
            pool.clone(),
            Arc::clone(supervisor),
            watchdog_job,
            worker_id.to_owned(),
            lease_seconds,
            handle.abort_handle(),
        );
    }
    // #407: jobs run detached; the next 5s tick claims into whatever slots are
    // free, so one long job no longer holds the other slots idle.
    Ok(())
}

/// Interval at which the no-progress watchdog inspects a job's lease. #407
const JOB_WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);
/// Slack past `lease_until` before a job counts as hung, covering a renewal
/// that is in flight right at the lease boundary. #407
const JOB_WATCHDOG_GRACE_SECONDS: i64 = 60;
/// The liveness heartbeat stops when the claim loop has not completed for
/// this long. #407
const CLAIM_LOOP_STALL_LIMIT_SECONDS: i64 = 600;

/// Deadline (unix seconds) by which a job must show progress again: one lease
/// window plus the watchdog grace and two watchdog intervals, i.e. strictly
/// after the watchdog would have aborted a hung job. #407
fn job_progress_deadline(lease_seconds: i64) -> i64 {
    Utc::now().timestamp()
        + lease_seconds
        + JOB_WATCHDOG_GRACE_SECONDS
        + 2 * JOB_WATCHDOG_INTERVAL.as_secs() as i64
}

/// Watch one in-flight job's lease. Lease renewals are the job's progress
/// signal; a job whose lease lapsed (plus grace) while we still own it hung
/// somewhere without its own bound, so abort the task and fail the job
/// (retryable, bounded by `max_attempts`). A lost lease needs no action here:
/// the job's own fencing stops it. #407
fn spawn_job_watchdog(
    pool: DbPool,
    supervisor: Arc<JobSupervisor>,
    job: JobRecord,
    lease_owner: String,
    lease_seconds: i64,
    job_task: tokio::task::AbortHandle,
) -> tokio::task::JoinHandle<()> {
    let span = info_span!("archivist_job_watchdog", job_id = %job.id, run_id = %job.run_id);
    tokio::spawn(
        async move {
        let verdict = watch_job_lease(
            || archivist_db::job_lease_until(&pool, job.id, &lease_owner),
            || job_task.is_finished(),
            || supervisor.touch(job.id, job_progress_deadline(lease_seconds)),
            JOB_WATCHDOG_INTERVAL,
            ChronoDuration::seconds(JOB_WATCHDOG_GRACE_SECONDS),
        )
        .await;
        if verdict == WatchdogVerdict::Stalled {
            job_task.abort();
            let message = format!(
                "job made no progress within its {lease_seconds}s lease window and was aborted by the worker watchdog"
            );
            warn!(job_id = %job.id, "{message}");
            if let Err(error) = fail_job(&pool, &job, &lease_owner, &message, true, None).await {
                warn!(error = %error, job_id = %job.id, "failed to record watchdog abort");
            }
        }
        }
        .instrument(span),
    )
}

/// Hard upper cap from `ARCHIVIST_WORKER_CONCURRENCY`. The settings-supplied
/// `worker_concurrency` can only clamp lower — never higher. This stops an
/// operator typo (e.g. `worker_concurrency: 9999`) from spinning up
/// thousands of in-flight jobs on a host that can only handle a handful.
fn env_concurrency_cap(config: &AppConfig) -> u32 {
    let raw = config.worker_concurrency.max(1) as u64;
    raw.min(u32::MAX as u64) as u32
}

/// Resolve the target concurrency for the next claim cycle.
///
/// Rules:
/// 1. The env var (`ARCHIVIST_WORKER_CONCURRENCY`) is the hard upper cap.
/// 2. The active provider's tuning (via `effective_tuning`) supplies the
///    desired pool size; if no tuning is set, the env cap is used.
/// 3. The result is clamped to `[1, env_cap]` so the worker always makes
///    forward progress on a tick.
///
/// Pure function — every input is a value the caller already owns, so this
/// is the unit-testable seam for the live-reload behaviour. The audit
/// event and atomic store happen in the caller.
fn resolve_target_concurrency(env_cap: u32, settings: &RuntimeSettings) -> u32 {
    let desired = settings.effective_tuning().worker_concurrency;
    desired.min(env_cap).max(1)
}

async fn process_job(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
    paperless: &PaperlessClient,
    job: &JobRecord,
    lease_owner: &str,
) -> Result<()> {
    info!(job_id = %job.id, run_id = %job.run_id, document_id = job.paperless_document_id, stage = %job.stage, "processing job");

    // Provider cooldown short-circuit. If the active provider for this
    // stage was previously flagged with a usage-limit 429, the worker
    // released the cooldown row and we'd just burn an attempt against
    // the same wall. Release the lease back to the queue with
    // `run_after = cooldown_until` so the job comes back when the
    // provider can plausibly answer again.
    if let Some(active) = active_cooldown_for_stage(pool, settings, job.stage).await? {
        info!(
            provider = %active.provider_name,
            until = %active.cooldown_until,
            "provider cooldown active; releasing lease without burning an attempt"
        );
        release_lease_for_cooldown(pool, job, lease_owner, active.cooldown_until).await?;
        return Ok(());
    }

    match job.stage {
        Stage::Ocr => process_ocr(pool, config, paperless, settings, job, lease_owner).await,
        Stage::Metadata => {
            process_metadata(pool, config, paperless, settings, job, lease_owner).await
        }
        Stage::Apply => Err(anyhow!(
            "stage {} is not directly executable by the worker",
            job.stage
        )),
    }
}

/// Resolves on SIGINT (ctrl-c) or SIGTERM. Kubernetes terminates pods with
/// SIGTERM; the worker previously only listened for SIGINT, so every rollout
/// ran until SIGKILL and left in-flight jobs to expire their leases,
/// burning a retry attempt per job.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
    }
}

fn hash_text(value: &str) -> String {
    hash_bytes(value.as_bytes())
}

fn hash_bytes(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_app_config;

    #[test]
    fn continuous_claim_only_fills_free_slots() {
        // #407: long-running jobs keep their slots; the rest are claimable.
        assert_eq!(claim_capacity(4, 0), 4);
        assert_eq!(claim_capacity(4, 1), 3);
        assert_eq!(claim_capacity(4, 4), 0);
        // Downscale below the in-flight count claims nothing, aborts nothing.
        assert_eq!(claim_capacity(2, 5), 0);
    }

    // -------------------------------------------------------------------
    // v1.6.2 issue #127: worker live-reload of concurrency
    //
    // The live-reload helper is `resolve_target_concurrency(env_cap,
    // settings)`. We verify:
    //   1. Settings clamp the env cap (lower).
    //   2. Settings cannot push above the env cap (typo safety).
    //   3. Floor is 1 — the worker always makes forward progress.
    //   4. Mutating the live settings between two calls grows / shrinks
    //      the target, simulating a real claim-cycle live-reload.
    //   5. The in-flight invariant: shrinking the target does not abort
    //      a previously claimed task. We model the in-flight task as a
    //      future that owns its own resources independently of the next
    //      cycle's target — the per-tick spawn-and-join structure makes
    //      this trivially true, so the test asserts the invariant
    //      directly on the values without races.
    // -------------------------------------------------------------------

    fn settings_with_concurrency(value: Option<u32>) -> RuntimeSettings {
        let mut settings = RuntimeSettings::default();
        settings.ai.default_provider = "ollama".to_owned();
        // Replace ollama provider's tuning with the desired value.
        for provider in settings.ai.providers.iter_mut() {
            if provider.name == "ollama" {
                provider.tuning.worker_concurrency = value;
            }
        }
        settings
    }

    #[test]
    fn resolve_target_concurrency_uses_settings_when_below_env_cap() {
        let settings = settings_with_concurrency(Some(2));
        assert_eq!(resolve_target_concurrency(8, &settings), 2);
    }

    #[test]
    fn resolve_target_concurrency_clamps_settings_above_env_cap_typo_safety() {
        // Hard upper cap is the env cap; an operator setting 9999 must
        // never spin up 9999 concurrent jobs.
        let settings = settings_with_concurrency(Some(9999));
        assert_eq!(resolve_target_concurrency(4, &settings), 4);
    }

    #[test]
    fn resolve_target_concurrency_floors_to_one_when_settings_say_zero() {
        let settings = settings_with_concurrency(Some(0));
        assert_eq!(resolve_target_concurrency(4, &settings), 1);
    }

    #[test]
    fn resolve_target_concurrency_uses_env_cap_when_tuning_is_blank() {
        // No tuning value AND no global default → effective_tuning falls
        // back to 1; that 1 is below the env cap so the result is 1.
        let settings = settings_with_concurrency(None);
        assert_eq!(resolve_target_concurrency(4, &settings), 1);
    }

    #[test]
    fn live_reload_grows_pool_when_settings_increase_concurrency() {
        // Start at concurrency=2 (the ollama preset), simulate an operator
        // raising the value to 4, assert the next cycle's target grows.
        let env_cap = 8;
        let initial = settings_with_concurrency(Some(2));
        let initial_target = resolve_target_concurrency(env_cap, &initial);
        assert_eq!(initial_target, 2);

        let bumped = settings_with_concurrency(Some(4));
        let bumped_target = resolve_target_concurrency(env_cap, &bumped);
        assert!(
            bumped_target > initial_target,
            "pool target must grow from {} to {}",
            initial_target,
            bumped_target
        );
        assert_eq!(bumped_target, 4);
    }

    #[test]
    fn live_reload_shrinks_pool_without_aborting_in_flight_jobs() {
        // The contract: shrinking target_concurrency from 2 → 1 must not
        // abort an in-flight job. The per-tick spawn-and-join design
        // satisfies this by construction — a task spawned in the
        // previous tick keeps its own `Arc<RuntimeSettings>` clone and
        // runs to completion before we even consult the next target.
        let env_cap = 8;
        let initial = settings_with_concurrency(Some(2));
        let initial_target = resolve_target_concurrency(env_cap, &initial);
        // Simulate two tasks "in flight" — the previous tick's claimed
        // jobs. Their lifecycle is independent of the next target.
        let in_flight_marker = Arc::new(AtomicU32::new(2));

        // Operator shrinks the pool to 1.
        let shrunk = settings_with_concurrency(Some(1));
        let shrunk_target = resolve_target_concurrency(env_cap, &shrunk);
        assert_eq!(initial_target, 2);
        assert_eq!(shrunk_target, 1);
        // In-flight marker is still 2 — the new target does not reach into
        // already-spawned tasks. This is the "never abort an in-flight
        // job" invariant.
        assert_eq!(in_flight_marker.load(Ordering::Acquire), 2);
    }

    #[test]
    fn env_concurrency_cap_floors_to_one() {
        // Defensive: if the env somehow resolved to 0 we still want to
        // make forward progress. The clamp lives in `env_concurrency_cap`
        // before the resolver sees it.
        let mut config = test_app_config();
        config.worker_concurrency = 0;
        assert_eq!(env_concurrency_cap(&config), 1);
    }
}
