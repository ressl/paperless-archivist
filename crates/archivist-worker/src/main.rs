use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use archivist_apply::recover_review_apply_intents;
use archivist_config::AppConfig;
use archivist_db::{
    DbPool, StartupRepair, backfill_metadata_stage_for_ocr_only_runs,
    bump_text_num_ctx_if_too_small, bump_vision_num_ctx_if_too_small, connect, get_backlog_counts,
    get_runtime_settings, rebalance_backfilled_metadata_priorities, record_dashboard_snapshot,
    reset_stale_applying_reviews, reset_stuck_running_pipeline_runs,
};
use chrono::Utc;
use secrecy::ExposeSecret;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::signal;
use tokio::time::{sleep, timeout};
use tracing::{Instrument, error, info, info_span, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use claim_loop::{CLAIM_LOOP_STALL_LIMIT_SECONDS, process_available_jobs};
use drain::drain_pending_reviews_if_autopilot_tick;
use job_supervisor::JobSupervisor;
use notifications::send_operational_notifications;
use paperless::paperless_client;
use startup::{run_startup_repair, run_startup_vision_crash_requeue};
use trigger_poll::poll_paperless_triggers;

mod apply;
mod claim_loop;
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
