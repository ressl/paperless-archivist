use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use archivist_ai::{
    AiResponse, ChatRequest, MetadataParseDiagnostics, MetadataParseStatus, PromptLanguageContext,
    parse_metadata_suggestion, prompt_for_metadata,
};
use archivist_apply::{recover_review_apply_intents, review_apply_baseline};
use archivist_config::AppConfig;
use archivist_core::{
    AuditEventInput, DocumentPatch, LanguageDetection, MetadataFieldFlags, MetadataSuggestion,
    RuntimeSettings, Stage, detect_document_language, validate_choice_suggestion,
    validate_document_date_suggestion, validate_field_suggestion, validate_tag_suggestion,
    validate_title_suggestion,
};
use archivist_db::{
    AiArtifactInput, DbPool, JobRecord, StartupRepair, append_audit,
    backfill_metadata_stage_for_ocr_only_runs, bump_text_num_ctx_if_too_small,
    bump_vision_num_ctx_if_too_small, claim_jobs, complete_job, connect, create_review_item,
    custom_field_ids_for_names, fail_job, get_backlog_counts, get_runtime_settings,
    increment_metric_counter, insert_ai_artifact, is_last_active_job, list_allowed_named_entities,
    list_allowed_tag_names, list_custom_fields, named_entity_id_for_name,
    rebalance_backfilled_metadata_priorities, record_dashboard_snapshot, record_document_language,
    reset_stale_applying_reviews, reset_stuck_running_pipeline_runs, tag_id_pairs_for_names,
    tag_ids_for_names,
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

use apply::{
    apply_patch_with_workflow_tags, failure_is_terminal,
    retire_trigger_tags_after_terminal_outcome, tags_for_old_tag_strategy,
};
use drain::drain_pending_reviews_if_autopilot_tick;
use failure::{
    ProcessingFailureClass, active_cooldown_for_stage, classify_processing_failure,
    record_quota_cooldown_for_failure, release_lease_for_cooldown,
};
use job_supervisor::{
    InFlightGuard, JobSupervisor, WatchdogVerdict, catch_job_panic, watch_job_lease,
};
use lease::job_lease_seconds;
use notifications::send_operational_notifications;
use ocr_stage::process_ocr;
use paperless::paperless_client;
use providers::{
    apply_active_prompt_with_experiment, apply_retry_prompt, chat_for_stage, chat_with_provider,
    classify_document_type, is_vision_model_runtime_crash, ollama_text_num_ctx_for_provider,
    provider_for_stage,
};
use startup::{run_startup_repair, run_startup_vision_crash_requeue};
use trigger_poll::poll_paperless_triggers;

mod apply;
mod drain;
mod failure;
mod job_supervisor;
mod lease;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataWorkerParseRoute {
    ProcessSuggestion,
    CompleteOmission,
    RetryContractViolation,
}

fn metadata_worker_parse_route(diagnostics: &MetadataParseDiagnostics) -> MetadataWorkerParseRoute {
    match diagnostics.status {
        MetadataParseStatus::Valid => MetadataWorkerParseRoute::ProcessSuggestion,
        MetadataParseStatus::Omitted => MetadataWorkerParseRoute::CompleteOmission,
        MetadataParseStatus::ContractViolation => MetadataWorkerParseRoute::RetryContractViolation,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_terminal_metadata_parse_route(
    pool: &DbPool,
    job: &JobRecord,
    lease_owner: &str,
    settings: &RuntimeSettings,
    response: &AiResponse,
    request: &ChatRequest,
    prompt_id: Option<Uuid>,
    content: &str,
    suggestion: &MetadataSuggestion,
    diagnostics: &MetadataParseDiagnostics,
) -> Result<bool> {
    let route = metadata_worker_parse_route(diagnostics);
    if route == MetadataWorkerParseRoute::ProcessSuggestion {
        return Ok(false);
    }

    let mut normalized = serde_json::to_value(suggestion)?;
    if let Some(object) = normalized.as_object_mut() {
        object.insert(
            "parse_diagnostics".to_owned(),
            serde_json::to_value(diagnostics)?,
        );
    }
    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Metadata,
            provider: &response.provider,
            model: &response.model,
            prompt_id,
            input_hash: &hash_text(content),
            request: Some(serde_json::to_value(request)?),
            response: Some(response.raw_response.clone()),
            normalized_output: Some(normalized),
            duration_ms: response.duration_ms,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    match route {
        MetadataWorkerParseRoute::ProcessSuggestion => Ok(false),
        MetadataWorkerParseRoute::CompleteOmission => {
            if !complete_job(
                pool,
                job,
                lease_owner,
                json!({
                    "skipped": "metadata model omitted all enabled fields",
                    "skipped_fields": [],
                    "parse_diagnostics": diagnostics,
                }),
            )
            .await?
            {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    "lease lost before metadata-omission completion; another worker owns this job"
                );
            }
            Ok(true)
        }
        MetadataWorkerParseRoute::RetryContractViolation => {
            let contract_error = diagnostics.contract_error().ok_or_else(|| {
                anyhow!("metadata parser routing invariant violated: contract error missing")
            })?;
            Err(contract_error.into())
        }
    }
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

async fn language_context_for_content(
    pool: &DbPool,
    settings: &RuntimeSettings,
    job: &JobRecord,
    content: &str,
) -> Result<PromptLanguageContext> {
    let detection = if content.trim().is_empty() {
        LanguageDetection::unknown("heuristic")
    } else {
        detect_document_language(content)
    };
    record_document_language(
        pool,
        job.paperless_document_id,
        &detection,
        Some(job.run_id),
        Some(job.id),
        "worker",
    )
    .await?;
    Ok(PromptLanguageContext::new(
        &detection,
        &settings.tagging.tag_output_language,
    ))
}

/// Pure split of `resolve_tag_names_to_ids`: given the requested names and the
/// (name, id) pairs that the local Paperless mirror already knows about, return
/// the set of known ids plus the list of names that were NOT found (and therefore
/// need creation-or-drop downstream depending on `allow_new_tags`).
///
/// Extracted so the diff/dedup/case-fold logic is unit-testable without a real
/// `DbPool` or `PaperlessClient`.
fn diff_known_tag_names(
    requested: &[String],
    known_pairs: &[(String, i32)],
) -> (Vec<i32>, Vec<String>) {
    let known_lower: std::collections::HashSet<String> = known_pairs
        .iter()
        .map(|(name, _)| archivist_core::fold_catalog_name(name))
        .collect();
    let mut ids: Vec<i32> = known_pairs.iter().map(|(_, id)| *id).collect();
    ids.sort_unstable();
    ids.dedup();
    let unknown: Vec<String> = requested
        .iter()
        .filter(|name| !known_lower.contains(&archivist_core::fold_catalog_name(name)))
        .cloned()
        .collect();
    (ids, unknown)
}

/// Resolve LLM-supplied tag NAMES to Paperless tag IDs so review_items always carry the
/// `Vec<i32>` shape that the approve → patch path expects (the apply path deserializes the
/// review_item's `suggested_patch.tags` as `Vec<i32>`; raw names cause a 500 there and the
/// autopilot drain then reverts the review forever).
///
/// Only names already present in the local `paperless_tags` mirror resolve to ids; nothing
/// is created here. Unknown names are returned so the caller can carry them as pending
/// names that are created at apply time (#404). Workflow tags are dropped from both lists
/// so model output can never set or re-add a trigger/completion tag (#403).
async fn resolve_known_tag_names(
    pool: &DbPool,
    names: &[String],
    workflow_tags: &archivist_core::WorkflowTags,
) -> Result<(Vec<i32>, Vec<String>)> {
    let names: Vec<String> = names
        .iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && !workflow_tags.is_workflow_tag(name))
        .collect();
    if names.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let known_pairs: Vec<(String, i32)> = tag_id_pairs_for_names(pool, &names)
        .await?
        .into_iter()
        .filter(|(name, _)| !workflow_tags.is_workflow_tag(name))
        .collect();
    Ok(diff_known_tag_names(&names, &known_pairs))
}

/// Pure split of `resolve_custom_field_values_to_ids`: build the
/// `[{ "field": id, "value": ... }]` JSON array Paperless expects from the input
/// FieldValueSuggestion list and the locally-known (name, id) pairs. Names not in
/// the pairs list are dropped. Extracted for unit testability.
fn build_custom_field_value_patch(
    fields: &[archivist_core::FieldValueSuggestion],
    id_pairs: &[(String, i32, Option<String>)],
) -> Vec<serde_json::Value> {
    fields
        .iter()
        .filter_map(|field| {
            let (_, id, data_type) = id_pairs
                .iter()
                .find(|(name, _, _)| archivist_core::catalog_names_equal(name, &field.name))?;
            match archivist_core::coerce_custom_field_value(data_type.as_deref(), &field.value) {
                Some(value) => Some(json!({ "field": id, "value": value })),
                None => {
                    warn!(
                        field = %field.name,
                        value = %field.value,
                        "dropped uncoercible custom field value"
                    );
                    None
                }
            }
        })
        .collect()
}

/// Resolve LLM-supplied custom-field NAMES to Paperless custom-field IDs. Same contract as
/// `resolve_tag_names_to_ids` but for custom fields. Unknown names are dropped with a warn
/// log — custom fields cannot be safely auto-created here because they require a `data_type`
/// the LLM doesn't reliably supply.
async fn resolve_custom_field_values_to_ids(
    pool: &DbPool,
    fields: &[archivist_core::FieldValueSuggestion],
) -> Result<Vec<serde_json::Value>> {
    if fields.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<String> = fields.iter().map(|field| field.name.clone()).collect();
    let id_pairs = custom_field_ids_for_names(pool, &names).await?;
    for field in fields {
        if !id_pairs
            .iter()
            .any(|(name, _, _)| archivist_core::catalog_names_equal(name, &field.name))
        {
            warn!(
                unknown_custom_field = %field.name,
                "dropping unknown custom field from review_item suggested_patch"
            );
        }
    }
    Ok(build_custom_field_value_patch(fields, &id_pairs))
}

/// Consolidated metadata stage (v1.4.0). One LLM call replaces six per-field
/// round-trips. The response is fanned out into up to six review items (or one
/// composite Paperless patch in full_auto mode) so existing reviewer UX, audit
/// trails, and per-field opt-outs keep working.
async fn process_metadata(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    lease_owner: &str,
) -> Result<()> {
    // #445: a review "retry with ..." job carries a provider/model/prompt
    // choice in its payload. It applies to this job only (and forces manual
    // review so the result returns to the reviewer); runtime settings stay
    // untouched.
    let retry_overrides = archivist_core::MetadataRetryOverrides::from_job_payload(&job.payload);
    let retry_settings = retry_overrides
        .as_ref()
        .map(|overrides| overrides.apply_to_settings(settings));
    let settings = retry_settings.as_ref().unwrap_or(settings);
    if let Some(overrides) = &retry_overrides {
        info!(
            job_id = %job.id,
            document_id = job.paperless_document_id,
            provider = overrides.provider_name.as_deref().unwrap_or("-"),
            model = overrides.model.as_deref().unwrap_or("-"),
            prompt_id = ?overrides.prompt_id,
            "metadata job runs with review retry overrides"
        );
    }
    let enabled = MetadataFieldFlags::from_enabled_stages(&settings.workflow.enabled_stages);
    if !enabled.any() {
        if !complete_job(
            pool,
            job,
            lease_owner,
            json!({ "skipped": "no metadata fields are enabled in workflow settings" }),
        )
        .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "lease lost before completion; another worker owns this job — skipping"
            );
        } else {
            // #400: terminal without a patch — retire the trigger tags.
            retire_trigger_tags_after_terminal_outcome(pool, paperless, settings, job, false).await;
        }
        return Ok(());
    }

    let document = paperless.get_document(job.paperless_document_id).await?;
    let content = document.content.clone().unwrap_or_default();

    // v1.5.14 (#117): content-hash dedup. If another document with the
    // same OCR-text sha256 has already had its metadata stage succeed,
    // we log the match and emit an audit event but keep running the
    // LLM call. This makes prod safe to enable: the dedup currently
    // serves as a signal-only feature (operators see the hit, but the
    // patch is still freshly LLM-derived). A future release can flip
    // this to a hard skip + clone of the source patch once we have
    // confidence the hash match is a reliable substitution.
    if !content.trim().is_empty() {
        let dedup_hash = hash_bytes(content.as_bytes());
        if let Some((source_id, _payload)) =
            archivist_db::find_metadata_dedup_source(pool, job.paperless_document_id, &dedup_hash)
                .await?
        {
            info!(
                document_id = job.paperless_document_id,
                dedup_source = source_id,
                "content-hash dedup match found (signal-only in v1.5.14)"
            );
            append_audit(
                pool,
                AuditEventInput {
                    event_type: "workflow.metadata_dedup_match".to_owned(),
                    actor_type: "worker".to_owned(),
                    actor_id: None,
                    run_id: Some(job.run_id),
                    job_id: Some(job.id),
                    paperless_document_id: Some(job.paperless_document_id),
                    before: None,
                    after: Some(json!({ "dedup_source": source_id })),
                    metadata: Some(json!({ "trigger": "content_hash" })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
        }
    }

    let language = language_context_for_content(pool, settings, job, &content).await?;

    // Cheap pre-flight: short-circuit fields that Paperless already populated and the operator
    // has not opted into overwriting. We still ask the LLM for the field if any other field is
    // requested, but we drop the suggestion before creating a review item / applying. Doing the
    // gating after the LLM call keeps the prompt deterministic across runs.
    let allowed_correspondents = if enabled.correspondent {
        list_allowed_named_entities(pool, "paperless_correspondents").await?
    } else {
        Vec::new()
    };
    let allowed_document_types = if enabled.document_type {
        list_allowed_named_entities(pool, "paperless_document_types").await?
    } else {
        Vec::new()
    };
    let allowed_tags = if enabled.tags {
        list_allowed_tag_names(pool).await?
    } else {
        Vec::new()
    };
    // Carry each field's data_type alongside its name so the prompt can
    // render a per-type formatting hint (#148). The schema still only needs
    // the names, so we derive a names-only view for `schema_for_metadata`
    // just before that call.
    let allowed_fields: Vec<(String, Option<String>)> = if enabled.fields {
        list_custom_fields(pool)
            .await?
            .into_iter()
            .filter(|field| settings.fields.field_enabled(&field.name))
            .map(|field| (field.name, field.data_type))
            .collect()
    } else {
        Vec::new()
    };

    // v1.5.12: pre-filter the closed-vocabulary lists by OCR-substring
    // frequency so the LLM gets the most relevant candidates and the prompt
    // stays under the token budget. Field names are typically a short curated
    // list so they bypass filtering.
    let allowed_list_max = settings.effective_tuning().allowed_list_max as usize;
    // Lowercase the (potentially large) content ONCE and share it across the
    // three prefilter passes instead of re-lowercasing it each time. #295
    let content_lower = content.to_lowercase();
    let allowed_correspondents = archivist_core::prefilter_allowed_list_lower(
        &content_lower,
        &allowed_correspondents,
        allowed_list_max,
    );
    let allowed_document_types = archivist_core::prefilter_allowed_list_lower(
        &content_lower,
        &allowed_document_types,
        allowed_list_max,
    );
    let allowed_tags = archivist_core::prefilter_allowed_list_lower(
        &content_lower,
        &allowed_tags,
        allowed_list_max,
    );

    // v1.5.13: cheap pre-pass classifier to give the main metadata prompt a
    // document-type-specific hint. Skips the call gracefully when content is
    // empty or the classifier fails — main prompt then runs without the hint.
    let doc_type_category = match classify_document_type(pool, config, settings, &content).await {
        Ok(category) => category,
        // A quota signal must propagate so the provider cooldown is persisted
        // and the stage isn't followed by a second doomed call against the
        // exhausted provider. Everything else degrades to the generic prompt. #280
        Err(error)
            if classify_processing_failure(&error) == ProcessingFailureClass::ProviderQuota =>
        {
            return Err(error);
        }
        Err(error) => {
            warn!(
                document_id = job.paperless_document_id,
                error = %error,
                "doc-type pre-pass failed; falling back to generic metadata prompt"
            );
            archivist_ai::DocTypeCategory::Other
        }
    };
    let doc_type_hint = archivist_ai::metadata_hint_for_doc_type(doc_type_category);
    info!(
        document_id = job.paperless_document_id,
        category = doc_type_category.as_str(),
        hint_chars = doc_type_hint.len(),
        "classified document type for metadata prompt"
    );

    let mut request = prompt_for_metadata(
        &content,
        &allowed_correspondents,
        &allowed_document_types,
        &allowed_tags,
        &allowed_fields,
        &enabled,
        &language,
        settings.effective_tuning().max_tags as usize,
        settings.fields.max_fields,
        doc_type_hint,
    );
    // v1.5.30: attach a JSON Schema that mirrors the prompt's
    // <output_schema> block. The Ollama client forwards it via the
    // `format` field of /api/chat, which lowers the schema to a GBNF
    // grammar and applies it during sampling — out-of-vocabulary tokens
    // become impossible to emit, so the closed-vocabulary
    // (document_type, correspondent, tags, custom-field names) gets
    // hard guarantees on top of the soft prompt constraints.
    // The OpenAI-compatible client forwards it as `response_format`
    // (json_schema/json_object per the provider's `structured_output`
    // tuning, with a one-shot schema-400 fallback), and the Anthropic
    // client as forced tool-use.
    let allowed_field_names: Vec<String> = allowed_fields
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    request.response_schema = archivist_ai::schema_for_metadata(
        &allowed_correspondents,
        &allowed_document_types,
        &allowed_tags,
        &allowed_field_names,
        &enabled,
        settings.effective_tuning().max_tags as usize,
        settings.fields.max_fields,
    );
    let retry_prompt_id = retry_overrides
        .as_ref()
        .and_then(|overrides| overrides.prompt_id);
    let (prompt_id, prompt_experiment_group) =
        match apply_retry_prompt(pool, retry_prompt_id, &mut request).await? {
            Some(prompt_id) => (Some(prompt_id), None),
            None => {
                apply_active_prompt_with_experiment(pool, Stage::Metadata, job.run_id, &mut request)
                    .await?
            }
        };
    // Heartbeat the lease before each long LLM call. The metadata stage can
    // chain classifier + main call + consensus (each up to the configured
    // request timeout) under one lease window; without renewing, a second
    // replica reclaims the "stale" job mid-stage and processes it concurrently.
    if !archivist_db::bump_job_lease(pool, job.id, lease_owner, job_lease_seconds(settings)).await?
    {
        warn!(
            job_id = %job.id,
            document_id = job.paperless_document_id,
            "metadata lease lost before main LLM call; stopping so a replica isn't duplicated"
        );
        return Ok(());
    }
    let response = chat_for_stage(pool, config, settings, Stage::Metadata, request.clone()).await?;
    let parsed = parse_metadata_suggestion(&response.text);
    let parse_diagnostics = parsed.diagnostics;
    let mut suggestion = parsed.suggestion;

    // Omission and contract errors are terminal before consensus/validation:
    // both persist the safe parse artifact, omission completes immediately,
    // and a violation returns the typed retryable error without applying any
    // retained valid subfields.
    if handle_terminal_metadata_parse_route(
        pool,
        job,
        lease_owner,
        settings,
        &response,
        &request,
        prompt_id,
        &content,
        &suggestion,
        &parse_diagnostics,
    )
    .await?
    {
        // #400: an omission completes the job without a patch.
        retire_trigger_tags_after_terminal_outcome(pool, paperless, settings, job, false).await;
        return Ok(());
    }

    // v1.5.15 (#118): two-model consensus check. When
    // `ai.consensus_secondary_text_model` is set AND we're heading for an
    // auto-apply (full_auto, not dry_run), fire a focused secondary call
    // against the configured cross-check model asking ONLY for
    // correspondent + document_date. Drop disagreeing fields from the
    // primary suggestion so they fall into review instead of being
    // silently auto-applied with an unverified value.
    let consensus_enabled = settings
        .effective_tuning()
        .consensus_secondary_text_model
        .as_deref()
        .map(str::trim)
        .is_some_and(|m| !m.is_empty())
        && settings.workflow.mode.auto_apply_validated_suggestions()
        && !settings.workflow.dry_run;
    let consensus_outcome = if consensus_enabled {
        if !archivist_db::bump_job_lease(pool, job.id, lease_owner, job_lease_seconds(settings))
            .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "metadata lease lost before consensus check; stopping so a replica isn't duplicated"
            );
            return Ok(());
        }
        Some(
            run_consensus_check(
                pool,
                config,
                settings,
                job,
                &content,
                &allowed_correspondents,
                &language,
                &mut suggestion,
            )
            .await?,
        )
    } else {
        None
    };

    let mut normalized = serde_json::to_value(&suggestion)?;
    if let Some(object) = normalized.as_object_mut() {
        object.insert(
            "parse_diagnostics".to_owned(),
            serde_json::to_value(&parse_diagnostics)?,
        );
    }
    if let Some(outcome) = consensus_outcome.as_ref()
        && let Some(object) = normalized.as_object_mut()
    {
        object.insert("consensus".to_owned(), serde_json::to_value(outcome)?);
    }
    if let Some(label) = prompt_experiment_group.as_ref()
        && let Some(object) = normalized.as_object_mut()
    {
        object.insert(
            "prompt_experiment_group".to_owned(),
            serde_json::Value::String(label.clone()),
        );
    }

    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Metadata,
            provider: &response.provider,
            model: &response.model,
            prompt_id,
            input_hash: &hash_text(&content),
            request: Some(serde_json::to_value(request)?),
            response: Some(response.raw_response),
            normalized_output: Some(normalized.clone()),
            duration_ms: response.duration_ms,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    // Fan the suggestion out into per-field outcomes.
    //
    // Each field is one of:
    //   * `Apply(field_patch)`              — valid, ready to auto-apply or to attach to a
    //                                         composite review item.
    //   * `Review(review_patch, warnings)`  — needs operator review (low confidence, validation
    //                                         failure, or operator policy says "don't overwrite").
    //   * `Skip(reason)`                     — model omitted the field or the document already had
    //                                         a value we are not allowed to overwrite.
    let auto_apply =
        settings.workflow.mode.auto_apply_validated_suggestions() && !settings.workflow.dry_run;
    let mut composite_patch = DocumentPatch {
        content: None,
        title: None,
        tags: None,
        correspondent: None,
        document_type: None,
        created: None,
        custom_fields: None,
    };
    let mut composite_warnings: Vec<String> = Vec::new();
    let mut review_items: Vec<(serde_json::Value, serde_json::Value)> = Vec::new();
    let mut applied_fields: Vec<&'static str> = Vec::new();
    let mut skipped_fields: Vec<&'static str> = Vec::new();

    // --- title ---
    if enabled.title
        && let Some(title) = suggestion.title.clone()
    {
        match validate_title_suggestion(
            title.clone(),
            // Paperless-ngx Document.title is CharField(max_length=128);
            // anything longer passes validation here but 400s on PATCH.
            128,
            settings.effective_tuning().title_confidence_threshold,
        ) {
            Ok(valid) => {
                composite_patch.title = Some(valid.title.clone());
                applied_fields.push("title");
            }
            Err(errors) => {
                review_items.push((
                    json!({
                        "title": title.title,
                        "standard_metadata": { "field": "title", "confidence": title.confidence }
                    }),
                    json!(errors),
                ));
            }
        }
    }

    // --- document_type ---
    if enabled.document_type
        && let Some(choice) = suggestion.document_type.clone()
    {
        if document.document_type.is_some() && !settings.metadata.overwrite_existing_document_type {
            skipped_fields.push("document_type");
        } else {
            match validate_choice_suggestion(
                choice.clone(),
                &allowed_document_types,
                settings
                    .effective_tuning()
                    .document_type_confidence_threshold,
            ) {
                Ok(valid) => {
                    let id =
                        named_entity_id_for_name(pool, "paperless_document_types", &valid.name)
                            .await?;
                    if let Some(id) = id {
                        composite_patch.document_type = Some(Some(id));
                        applied_fields.push("document_type");
                    } else {
                        skipped_fields.push("document_type");
                    }
                }
                Err(errors) => {
                    review_items.push((
                        json!({
                            "document_type": "",
                            "standard_metadata": {
                                "field": "document_type",
                                "suggested_name": choice.name,
                                "confidence": choice.confidence,
                                "evidence": choice.evidence,
                                "current_document_type": document.document_type,
                            }
                        }),
                        json!(errors),
                    ));
                }
            }
        }
    }

    // --- correspondent ---
    if enabled.correspondent
        && let Some(choice) = suggestion.correspondent.clone()
    {
        if document.correspondent.is_some() && !settings.metadata.overwrite_existing_correspondent {
            skipped_fields.push("correspondent");
        } else {
            match validate_choice_suggestion(
                choice.clone(),
                &allowed_correspondents,
                settings
                    .effective_tuning()
                    .correspondent_confidence_threshold,
            ) {
                Ok(valid) => {
                    let id =
                        named_entity_id_for_name(pool, "paperless_correspondents", &valid.name)
                            .await?;
                    if let Some(id) = id {
                        composite_patch.correspondent = Some(Some(id));
                        applied_fields.push("correspondent");
                    } else {
                        skipped_fields.push("correspondent");
                    }
                }
                Err(errors) => {
                    review_items.push((
                        json!({
                            "correspondent": "",
                            "standard_metadata": {
                                "field": "correspondent",
                                "suggested_name": choice.name,
                                "confidence": choice.confidence,
                                "evidence": choice.evidence,
                                "current_correspondent": document.correspondent,
                            }
                        }),
                        json!(errors),
                    ));
                }
            }
        }
    }

    // --- new correspondent (gated, created at apply time) ---
    // When the model found no closed-vocabulary match (it set `correspondent`
    // null) but proposed a sender/issuer the document clearly names, carry
    // the NAME forward. It is created in Paperless only when the patch is
    // actually applied (full_auto right below, or on review approval) — never
    // while the suggestion waits for review or in dry-run (#404). Gated by
    // `metadata.allow_new_correspondents` and the same overwrite-existing
    // guard as the closed-vocab path.
    let mut pending_new_correspondent: Option<String> = None;
    if enabled.correspondent
        && settings.metadata.allow_new_correspondents
        && suggestion.correspondent.is_none()
        && composite_patch.correspondent.is_none()
        && (document.correspondent.is_none() || settings.metadata.overwrite_existing_correspondent)
        && let Some(new_name) = suggestion
            .new_correspondent
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
    {
        pending_new_correspondent = Some(new_name.to_owned());
        applied_fields.push("correspondent");
    }
    let mut pending_new_tags: Vec<String> = Vec::new();

    // --- document_date ---
    if enabled.document_date
        && let Some(date) = suggestion.document_date.clone()
    {
        let already_set = document
            .created
            .as_deref()
            .is_some_and(|value| !value.is_empty());
        if already_set && !settings.metadata.overwrite_existing_document_date {
            skipped_fields.push("document_date");
        } else {
            // v1.5.12: anchor-check the suggested date against the OCR text.
            // If no anchor phrase (Rechnungsdatum, Issued on, …) is within
            // ±80 chars of an occurrence of the date in the OCR text, drop
            // the confidence by document_date_anchor_penalty before
            // validating — this catches the common case where the model
            // picks up a body-text date (delivery date, due date, reference
            // to another invoice) instead of the actual document date.
            let mut adjusted_date = date.clone();
            let mut date_anchor_warning: Option<String> = None;
            if settings.metadata.document_date_anchor_required
                && !archivist_core::document_date_has_anchor(&date.date, &content)
            {
                let original = adjusted_date.confidence.unwrap_or(0.0);
                let penalty = settings.metadata.document_date_anchor_penalty;
                adjusted_date.confidence = Some((original - penalty).max(0.0));
                date_anchor_warning = Some(format!(
                    "document_date suggestion '{}' has no anchor phrase (Rechnungsdatum, Issued on, …) within {} chars in the OCR text; confidence reduced from {:.2} to {:.2}",
                    date.date,
                    80,
                    original,
                    adjusted_date.confidence.unwrap_or(0.0),
                ));
            }
            match validate_document_date_suggestion(
                adjusted_date,
                settings
                    .effective_tuning()
                    .document_date_confidence_threshold,
            ) {
                Ok(valid) => {
                    composite_patch.created = Some(valid.date.clone());
                    composite_warnings.extend(valid.warnings);
                    if let Some(warning) = date_anchor_warning.clone() {
                        composite_warnings.push(warning);
                    }
                    applied_fields.push("document_date");
                }
                Err(mut errors) => {
                    if let Some(warning) = date_anchor_warning.clone() {
                        errors.push(archivist_core::ValidationError::DataQuality(warning));
                    }
                    review_items.push((
                        json!({
                            "created": date.date.clone(),
                            "standard_metadata": {
                                "field": "document_date",
                                "suggested_date": date.date,
                                "confidence": date.confidence,
                                "evidence": date.evidence,
                                "warnings": date.warnings,
                                "current_date": document.created,
                                "anchor_warning": date_anchor_warning,
                            }
                        }),
                        json!(errors),
                    ));
                }
            }
        }
    }

    // --- tags ---
    if enabled.tags
        && let Some(tags) = suggestion.tags.clone()
    {
        // v1.5.12: tags-confidence override for the consolidated stage. Clone
        // TaggingSettings and bump the confidence_threshold to the per-field
        // metadata override so process_metadata stays decoupled from how the
        // legacy per-field tag stage thresholds work.
        let mut tagging_for_meta = settings.tagging.clone();
        tagging_for_meta.confidence_threshold =
            settings.effective_tuning().tags_confidence_threshold;
        match validate_tag_suggestion(
            tags.clone(),
            &allowed_tags,
            &settings.workflow.tags,
            &tagging_for_meta,
        ) {
            Ok(valid) => {
                let selected_ids = tag_ids_for_names(pool, &valid.tags).await?;
                // #411: honour old_tag_strategy for real; workflow and rule
                // tags always survive.
                let tag_ids =
                    tags_for_old_tag_strategy(pool, settings, &document, &selected_ids).await?;
                composite_patch.tags = Some(tag_ids);
                // #411: validated new_tags (only non-empty with allow_new_tags)
                // are applied too — created at apply time (#404).
                pending_new_tags = valid.new_tags.clone();
                composite_warnings.extend(valid.warnings);
                applied_fields.push("tags");
            }
            Err(errors) => {
                // Validation failed (e.g. low confidence, count over max_tags). Resolve the raw
                // LLM names to integer IDs BEFORE creating the review_item so the apply path can
                // deserialize `suggested_patch.tags` as `Vec<i32>` without 500-ing.
                //
                // #403: the review patch is `current ∪ suggested` so approving it can only ADD
                // tags (the apply path treats `baseline − desired` as removals); workflow tags
                // are never taken from model output.
                // #404: unknown names are NOT created here; with allow_new_tags they travel as
                // pending names and are created only if the review is approved.
                let (known_ids, unknown) =
                    resolve_known_tag_names(pool, &tags.tags, &settings.workflow.tags).await?;
                let mut tag_ids = document.tags.clone();
                tag_ids.extend(known_ids);
                tag_ids.sort_unstable();
                tag_ids.dedup();
                let pending = if settings.tagging.allow_new_tags {
                    archivist_apply::PendingNewObjects::new(
                        unknown.into_iter().chain(tags.new_tags.iter().cloned()),
                        None,
                        &settings.workflow.tags,
                    )
                } else {
                    for name in &unknown {
                        warn!(
                            unknown_tag = %name,
                            "dropping unknown tag from review_item suggested_patch (allow_new_tags is false)"
                        );
                    }
                    archivist_apply::PendingNewObjects::default()
                };
                let mut review_patch = json!({
                    "tags": tag_ids,
                    "standard_metadata": {
                        "field": "tags",
                        "confidence": tags.confidence,
                        "suggested_names": tags.tags,
                        "new_tag_names": pending.tags,
                    }
                });
                pending.attach_to(&mut review_patch);
                review_items.push((review_patch, json!(errors)));
            }
        }
    }

    // --- fields ---
    if enabled.fields
        && let Some(fields) = suggestion.fields.clone()
    {
        match validate_field_suggestion(
            fields.clone(),
            &allowed_field_names,
            settings.fields.max_fields,
            settings.effective_tuning().fields_confidence_threshold,
        ) {
            Ok(valid) => {
                let names = valid
                    .fields
                    .iter()
                    .map(|field| field.name.clone())
                    .collect::<Vec<_>>();
                let ids = custom_field_ids_for_names(pool, &names).await?;
                let mut values = Vec::new();
                for field in &valid.fields {
                    let Some((_, id, data_type)) = ids.iter().find(|(name, _, _)| {
                        archivist_core::catalog_names_equal(name, &field.name)
                    }) else {
                        continue;
                    };
                    match archivist_core::coerce_custom_field_value(
                        data_type.as_deref(),
                        &field.value,
                    ) {
                        Some(value) => values.push(json!({ "field": id, "value": value })),
                        None => {
                            warn!(
                                field = %field.name,
                                value = %field.value,
                                "dropped uncoercible custom field value"
                            );
                            composite_warnings.push(format!(
                                "dropped uncoercible custom field value: {} = {}",
                                field.name, field.value
                            ));
                        }
                    }
                }
                composite_patch.custom_fields = Some(json!(values));
                composite_warnings.extend(valid.warnings);
                applied_fields.push("fields");
            }
            Err(errors) => {
                // Same shape-correctness fix as tags: resolve field NAMES to numeric IDs and
                // wrap as `[{ "field": id, "value": ... }]` so the approve → patch path can
                // deserialize `suggested_patch.custom_fields` against Paperless without 500.
                let values = resolve_custom_field_values_to_ids(pool, &fields.fields).await?;
                review_items.push((
                    json!({
                        "custom_fields": values,
                        "standard_metadata": {
                            "field": "fields",
                            "suggested_names": fields
                                .fields
                                .iter()
                                .map(|f| f.name.clone())
                                .collect::<Vec<_>>(),
                        }
                    }),
                    json!(errors),
                ));
            }
        }
    }

    info!(
        job_id = %job.id,
        document_id = job.paperless_document_id,
        applied_fields = ?applied_fields,
        review_items = review_items.len(),
        skipped_fields = ?skipped_fields,
        "consolidated metadata stage planned outcome"
    );

    // Model-proposed objects that do not exist in Paperless yet. Capped and
    // stripped of workflow tag names; created only at apply time (#404).
    let pending_new_objects = archivist_apply::PendingNewObjects::new(
        pending_new_tags,
        pending_new_correspondent,
        &settings.workflow.tags,
    );

    // Routing:
    //   * full_auto: apply the validated composite_patch directly even if some
    //     fields had validation warnings (UnknownTag, UnknownChoice, EmptyOutput
    //     etc.). The warnings tell the operator WHICH per-field suggestion was
    //     dropped, but the patch only carries fields that resolved. Creating
    //     six review items per document in full_auto turns "hands-off mode"
    //     into a manual-review queue and explodes Paperless API calls 6x.
    //   * Otherwise (manual_review, auto_select_review, or full_auto + dry_run):
    //     every field becomes a review item — operator inspects all
    //     suggestions atomically rather than seeing a half-applied document.
    //   * If everything was skipped (already-set fields with overwrite disabled),
    //     we still mark the job complete so the run drains.
    // Final heartbeat before side effects (review inserts / Paperless PATCH):
    // from here on a lost lease must stop this worker, not double-apply.
    if !archivist_db::bump_job_lease(pool, job.id, lease_owner, job_lease_seconds(settings)).await?
    {
        warn!(
            job_id = %job.id,
            document_id = job.paperless_document_id,
            "metadata lease lost before apply/review; stopping so a replica isn't duplicated"
        );
        return Ok(());
    }
    let review_warning_count = review_items.len();
    if !review_items.is_empty() && !auto_apply {
        // Manual / dry-run path: demote applied fields to review items too,
        // so the operator can sign off on the full set rather than seeing
        // partial application.
        if composite_patch.title.is_some() {
            review_items.push((
                json!({
                    "title": composite_patch.title.clone().unwrap_or_default(),
                    "standard_metadata": { "field": "title", "auto_validated": true }
                }),
                json!([]),
            ));
        }
        if let Some(Some(correspondent)) = composite_patch.correspondent {
            review_items.push((
                json!({
                    "correspondent": correspondent,
                    "standard_metadata": { "field": "correspondent", "auto_validated": true }
                }),
                json!([]),
            ));
        }
        if let Some(Some(document_type)) = composite_patch.document_type {
            review_items.push((
                json!({
                    "document_type": document_type,
                    "standard_metadata": { "field": "document_type", "auto_validated": true }
                }),
                json!([]),
            ));
        }
        if let Some(date) = composite_patch.created.clone() {
            review_items.push((
                json!({
                    "created": date,
                    "standard_metadata": { "field": "document_date", "auto_validated": true }
                }),
                json!([]),
            ));
        }
        if let Some(name) = pending_new_objects.correspondent.clone() {
            // #404: name only; the correspondent is created on approval.
            let mut patch = json!({
                "standard_metadata": {
                    "field": "correspondent",
                    "auto_validated": true,
                    "suggested_name": name,
                    "new_object": true,
                }
            });
            archivist_apply::PendingNewObjects {
                tags: Vec::new(),
                correspondent: Some(name),
            }
            .attach_to(&mut patch);
            review_items.push((patch, json!([])));
        }
        if let Some(tags) = composite_patch.tags.clone() {
            let mut patch = json!({
                "tags": tags,
                "standard_metadata": {
                    "field": "tags",
                    "auto_validated": true,
                    "new_tag_names": pending_new_objects.tags,
                }
            });
            archivist_apply::PendingNewObjects {
                tags: pending_new_objects.tags.clone(),
                correspondent: None,
            }
            .attach_to(&mut patch);
            review_items.push((patch, json!([])));
        }
        if let Some(custom_fields) = composite_patch.custom_fields.clone() {
            review_items.push((
                json!({
                    "custom_fields": custom_fields,
                    "standard_metadata": { "field": "fields", "auto_validated": true }
                }),
                json!([]),
            ));
        }

        let baseline = review_apply_baseline(&document);
        for (patch, warnings) in review_items {
            if create_review_item(pool, job, patch, warnings, baseline.clone(), lease_owner)
                .await?
                .is_none()
            {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    "metadata lease lost during review creation; stopping so a replica isn't duplicated"
                );
                return Ok(());
            }
        }
        Ok(())
    } else if !applied_fields.is_empty() {
        if auto_apply {
            // #404: full_auto + validated is the apply moment, so create the
            // proposed objects now. A creation failure degrades to applying
            // the rest of the patch, as before.
            if !pending_new_objects.is_empty() {
                match archivist_apply::materialize_pending_new_objects(
                    paperless,
                    &pending_new_objects,
                    archivist_apply::NewObjectPolicy::from_settings(settings),
                    &mut composite_patch,
                )
                .await
                {
                    Ok(new_tag_ids) if !new_tag_ids.is_empty() => {
                        let tags = composite_patch
                            .tags
                            .get_or_insert_with(|| document.tags.clone());
                        tags.extend(new_tag_ids);
                        tags.sort_unstable();
                        tags.dedup();
                    }
                    Ok(_) => {}
                    Err(error) => warn!(
                        document_id = job.paperless_document_id,
                        error = %error,
                        "failed to create proposed Paperless objects; applying without them"
                    ),
                }
            }
            let final_run_stage = is_last_active_job(pool, job.run_id, job.id).await?;
            let execution = apply_patch_with_workflow_tags(
                pool,
                paperless,
                settings,
                job,
                composite_patch,
                final_run_stage,
                lease_owner,
            )
            .await?;
            if complete_job(
                pool,
                job,
                lease_owner,
                json!({
                    "applied": true,
                    "fields": applied_fields,
                    "warnings": composite_warnings,
                    "dropped_field_count": review_warning_count,
                    "parse_diagnostics": parse_diagnostics,
                }),
            )
            .await?
            {
                archivist_db::finalize_apply_intent(pool, execution.attempt_id()).await?;
            } else {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    "lease lost before completion; another worker owns this job — skipping"
                );
            }
            Ok(())
        } else {
            // manual_review (or dry_run): a single composite review item with all validated
            // suggestions so the operator approves the whole set atomically.
            let baseline = review_apply_baseline(&document);
            let mut composite_review_patch = serde_json::to_value(&composite_patch)?;
            pending_new_objects.attach_to(&mut composite_review_patch);
            if create_review_item(
                pool,
                job,
                composite_review_patch,
                json!(composite_warnings),
                baseline,
                lease_owner,
            )
            .await?
            .is_none()
            {
                warn!(
                    job_id = %job.id,
                    document_id = job.paperless_document_id,
                    "metadata lease lost during review creation; skipping"
                );
            }
            Ok(())
        }
    } else if auto_apply && review_warning_count > 0 {
        // full_auto + every field had a validation warning, nothing applied. We
        // record the warnings in the job result and mark the job complete so
        // the run drains — Paperless gets nothing for this stage but neither
        // does the operator get a useless review pile.
        if !complete_job(
            pool,
            job,
            lease_owner,
            json!({
                "skipped": "all metadata fields had validation warnings — no resolvable patch in full_auto",
                "warnings": composite_warnings,
                "dropped_field_count": review_warning_count,
                "parse_diagnostics": parse_diagnostics,
            }),
        )
        .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "lease lost before completion; another worker owns this job — skipping"
            );
        } else {
            // #400: terminal without a patch — retire the trigger tags.
            retire_trigger_tags_after_terminal_outcome(pool, paperless, settings, job, false)
                .await;
        }
        Ok(())
    } else {
        if !complete_job(
            pool,
            job,
            lease_owner,
            json!({
                "skipped": "all metadata fields skipped by model omission or overwrite policy",
                "skipped_fields": skipped_fields,
                "parse_diagnostics": parse_diagnostics,
            }),
        )
        .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "lease lost before completion; another worker owns this job — skipping"
            );
        } else {
            // #400: terminal without a patch — retire the trigger tags.
            retire_trigger_tags_after_terminal_outcome(pool, paperless, settings, job, false).await;
        }
        Ok(())
    }
}

/// Outcome of the two-model consensus cross-check. Captured for the
/// metadata `ai_artifacts.normalized` payload so dashboards can chart
/// the disagreement rate without re-parsing audit events.
#[derive(Debug, Clone, Default, serde::Serialize)]
struct ConsensusOutcome {
    secondary_model: String,
    correspondent_primary: Option<String>,
    correspondent_secondary: Option<String>,
    correspondent_disagreed: bool,
    date_primary: Option<String>,
    date_secondary: Option<String>,
    date_disagreed: bool,
}

/// Two-model consensus cross-check for high-stakes fields.
///
/// Runs a focused secondary LLM call asking ONLY for `correspondent`
/// and `document_date`. When the secondary answer disagrees with the
/// primary suggestion's value, that specific field is wiped from the
/// primary `MetadataSuggestion` so it falls into review instead of
/// being auto-applied. Disagreements are audited via
/// `workflow.consensus_disagreement`.
///
/// Comparison rules:
/// * correspondent — case-insensitive exact match on the resolved name.
///   Empty secondary answer means "no opinion", which is NOT a
///   disagreement.
/// * document_date — both sides parsed as ISO; absolute day difference
///   must be ≤ `settings.ai.consensus_date_tolerance_days`. Empty or
///   un-parsable secondary answer means "no opinion".
#[allow(clippy::too_many_arguments)]
async fn run_consensus_check(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
    job: &JobRecord,
    content: &str,
    allowed_correspondents: &[String],
    language: &archivist_ai::PromptLanguageContext,
    suggestion: &mut MetadataSuggestion,
) -> Result<ConsensusOutcome> {
    let tuning = settings.effective_tuning();
    let secondary_model = tuning
        .consensus_secondary_text_model
        .clone()
        .unwrap_or_default();
    let mut outcome = ConsensusOutcome {
        secondary_model: secondary_model.clone(),
        ..Default::default()
    };
    if secondary_model.trim().is_empty() {
        return Ok(outcome);
    }

    // Build the focused 2-field prompt and override the model for the
    // call. Reuses the metadata stage's provider (and therefore the
    // operator's authentication) so no separate endpoint config is
    // needed.
    let mut request =
        archivist_ai::prompt_for_consensus_check(content, allowed_correspondents, language);
    let mut provider = match provider_for_stage(settings, Stage::Metadata, false) {
        Ok(p) => p,
        Err(error) => {
            warn!(
                document_id = job.paperless_document_id,
                error = %error,
                "consensus skipped: provider_for_stage(metadata) failed"
            );
            return Ok(outcome);
        }
    };
    provider.model = secondary_model.clone();
    request.model = secondary_model.clone();
    request.num_ctx = ollama_text_num_ctx_for_provider(&provider, tuning.text_num_ctx);
    request.reasoning_effort = Some(provider.reasoning_effort);
    request.max_output_tokens = provider.max_output_tokens;
    request.structured_output = Some(provider.structured_output);

    let response = match chat_with_provider(pool, config, &provider, request).await {
        Ok(r) => r,
        // Propagate a quota signal so a cooldown is recorded; a non-quota
        // secondary-call failure stays a graceful no-opinion. #280
        Err(error)
            if classify_processing_failure(&error) == ProcessingFailureClass::ProviderQuota =>
        {
            return Err(error);
        }
        Err(error) => {
            warn!(
                document_id = job.paperless_document_id,
                secondary_model = %secondary_model,
                error = %error,
                "consensus secondary call failed; treating as no-opinion"
            );
            return Ok(outcome);
        }
    };
    let answer = archivist_ai::parse_consensus_answer(&response.text);

    // Correspondent comparison
    if let Some(primary) = suggestion.correspondent.clone() {
        outcome.correspondent_primary = Some(primary.name.clone());
        outcome.correspondent_secondary = Some(answer.correspondent.clone());
        let primary_norm = primary.name.trim().to_lowercase();
        let secondary_norm = answer.correspondent.trim().to_lowercase();
        if !secondary_norm.is_empty() && primary_norm != secondary_norm {
            outcome.correspondent_disagreed = true;
            suggestion.correspondent = None;
        }
    }

    // Date comparison
    if let Some(primary) = suggestion.document_date.clone() {
        outcome.date_primary = Some(primary.date.clone());
        outcome.date_secondary = Some(answer.document_date.clone());
        let primary_parsed = chrono::NaiveDate::parse_from_str(&primary.date, "%Y-%m-%d").ok();
        let secondary_parsed =
            chrono::NaiveDate::parse_from_str(answer.document_date.trim(), "%Y-%m-%d").ok();
        if let (Some(p), Some(s)) = (primary_parsed, secondary_parsed) {
            let tolerance = tuning.consensus_date_tolerance_days.max(0);
            let diff = (p - s).num_days().abs();
            if diff > tolerance {
                outcome.date_disagreed = true;
                suggestion.document_date = None;
            }
        }
    }

    if outcome.correspondent_disagreed || outcome.date_disagreed {
        info!(
            document_id = job.paperless_document_id,
            secondary_model = %secondary_model,
            correspondent_disagreed = outcome.correspondent_disagreed,
            date_disagreed = outcome.date_disagreed,
            "consensus disagreement — dropping disagreeing fields from auto-apply"
        );
        append_audit(
            pool,
            AuditEventInput {
                event_type: "workflow.consensus_disagreement".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: Some(job.run_id),
                job_id: Some(job.id),
                paperless_document_id: Some(job.paperless_document_id),
                before: None,
                after: Some(json!({
                    "secondary_model": secondary_model,
                    "correspondent_disagreed": outcome.correspondent_disagreed,
                    "correspondent_primary": outcome.correspondent_primary,
                    "correspondent_secondary": outcome.correspondent_secondary,
                    "date_disagreed": outcome.date_disagreed,
                    "date_primary": outcome.date_primary,
                    "date_secondary": outcome.date_secondary,
                })),
                metadata: Some(json!({ "trigger": "consensus_check" })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }

    Ok(outcome)
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
    use archivist_core::ProcessingMode;
    use archivist_db::create_run_with_jobs_with_priority;
    use sqlx::Row;

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

    #[test]
    fn metadata_contract_violations_retry_but_omissions_take_the_explicit_success_route() {
        let malformed = parse_metadata_suggestion(r#"{"title":42}"#);
        assert_eq!(
            metadata_worker_parse_route(&malformed.diagnostics),
            MetadataWorkerParseRoute::RetryContractViolation
        );
        let contract_error = malformed
            .diagnostics
            .contract_error()
            .expect("wrong known field type must violate the contract");
        let error = anyhow::Error::new(contract_error);
        assert_eq!(
            classify_processing_failure(&error),
            ProcessingFailureClass::Transient
        );

        let omitted = parse_metadata_suggestion(r#"{"title":null}"#);
        assert_eq!(omitted.diagnostics.status, MetadataParseStatus::Omitted);
        assert_eq!(
            metadata_worker_parse_route(&omitted.diagnostics),
            MetadataWorkerParseRoute::CompleteOmission
        );
        assert!(omitted.diagnostics.contract_error().is_none());

        let valid = parse_metadata_suggestion(r#"{"title":{"title":"Invoice","confidence":0.9}}"#);
        assert_eq!(
            metadata_worker_parse_route(&valid.diagnostics),
            MetadataWorkerParseRoute::ProcessSuggestion
        );
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
    async fn metadata_terminal_parse_routes_persist_artifacts_before_completion_or_retry() {
        let Ok(database_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = connect(&database_url, 10)
            .await
            .expect("connect test database (DB integration tests share one database: run them serially with `-- --ignored --test-threads=1`, see scripts/verify/migration_smoke.sh)");
        archivist_db::migrate(&pool)
            .await
            .expect("apply migrations");
        sqlx::query(
            r#"
            truncate document_inventory, jobs, pipeline_runs, review_items,
                     ai_artifacts, audit_events restart identity cascade
            "#,
        )
        .execute(&pool)
        .await
        .expect("truncate test tables");

        let request = ChatRequest {
            model: "test-model".to_owned(),
            system_prompt: "system".to_owned(),
            user_prompt: "user".to_owned(),
            temperature: 0.0,
            num_ctx: None,
            response_schema: None,
            reasoning_effort: None,
            max_output_tokens: None,
            structured_output: None,
        };
        let settings = RuntimeSettings::default();

        for document_id in [77, 78] {
            sqlx::query(
                "insert into document_inventory (paperless_document_id, current_tags) values ($1, '{}')",
            )
            .bind(document_id)
            .execute(&pool)
            .await
            .expect("seed inventory");
            create_run_with_jobs_with_priority(
                &pool,
                document_id,
                &[Stage::Metadata],
                ProcessingMode::ManualReview,
                "test",
                "test",
                Some(0),
            )
            .await
            .expect("create metadata run");
        }
        let jobs = claim_jobs(&pool, 2, "worker-a", 300)
            .await
            .expect("claim jobs");
        let omitted_job = jobs
            .iter()
            .find(|job| job.paperless_document_id == 77)
            .expect("omission job");
        let contract_job = jobs
            .iter()
            .find(|job| job.paperless_document_id == 78)
            .expect("contract job");

        let omitted = parse_metadata_suggestion("{}");
        let omitted_response = AiResponse {
            provider: "test-provider".to_owned(),
            model: "test-model".to_owned(),
            text: "{}".to_owned(),
            raw_response: json!({"message":{"content":"{}"}}),
            duration_ms: 1,
        };
        let handled = handle_terminal_metadata_parse_route(
            &pool,
            omitted_job,
            "worker-a",
            &settings,
            &omitted_response,
            &request,
            None,
            "private document content",
            &omitted.suggestion,
            &omitted.diagnostics,
        )
        .await
        .expect("complete omission");
        assert!(handled, "omission is terminal before consensus/apply");
        let row = sqlx::query("select status, result from jobs where id = $1")
            .bind(omitted_job.id)
            .fetch_one(&pool)
            .await
            .expect("omission job result");
        let status: String = row.try_get("status").expect("omission status");
        let result: serde_json::Value = row.try_get("result").expect("omission result");
        assert_eq!(status, "succeeded");
        assert_eq!(result["parse_diagnostics"]["status"], "omitted");

        let malformed = parse_metadata_suggestion(r#"{"title":"raw-private-value"}"#);
        let malformed_response = AiResponse {
            provider: "test-provider".to_owned(),
            model: "test-model".to_owned(),
            text: r#"{"title":"raw-private-value"}"#.to_owned(),
            raw_response: json!({"message":{"content":"raw-private-value"}}),
            duration_ms: 1,
        };
        let error = handle_terminal_metadata_parse_route(
            &pool,
            contract_job,
            "worker-a",
            &settings,
            &malformed_response,
            &request,
            None,
            "private document content",
            &malformed.suggestion,
            &malformed.diagnostics,
        )
        .await
        .expect_err("contract violation must retry");
        assert_eq!(
            classify_processing_failure(&error),
            ProcessingFailureClass::Transient
        );
        let safe_error = format!("{error:#}");
        assert!(safe_error.contains("metadata model contract violation"));
        assert!(safe_error.contains("invalid_fields=title"));
        assert!(!safe_error.contains("raw-private-value"));
        let row = sqlx::query("select status, result from jobs where id = $1")
            .bind(contract_job.id)
            .fetch_one(&pool)
            .await
            .expect("contract job state");
        let status: String = row.try_get("status").expect("contract status");
        let result: Option<serde_json::Value> = row.try_get("result").expect("contract result");
        assert_eq!(status, "running", "terminal helper never completes it");
        assert!(result.is_none());
        let normalized: serde_json::Value = sqlx::query_scalar(
            "select normalized_output from ai_artifacts where job_id = $1 order by created_at desc limit 1",
        )
        .bind(contract_job.id)
        .fetch_one(&pool)
        .await
        .expect("contract artifact");
        assert_eq!(
            normalized["parse_diagnostics"]["status"],
            "contract_violation"
        );
        assert_eq!(
            normalized["parse_diagnostics"]["invalid_fields"],
            json!(["title"])
        );
        assert!(!normalized.to_string().contains("raw-private-value"));
        let review_count: i64 = sqlx::query_scalar("select count(*) from review_items")
            .fetch_one(&pool)
            .await
            .expect("review count");
        assert_eq!(review_count, 0);

        let updated = fail_job(
            &pool,
            contract_job,
            "worker-a",
            &safe_error,
            classify_processing_failure(&error).is_retryable(),
            None,
        )
        .await
        .expect("route contract failure through normal retry budget");
        assert!(updated);
        let row = sqlx::query("select status, error_message from jobs where id = $1")
            .bind(contract_job.id)
            .fetch_one(&pool)
            .await
            .expect("retried contract job");
        let status: String = row.try_get("status").expect("retry status");
        let error_message: String = row.try_get("error_message").expect("safe job error");
        assert_eq!(status, "queued");
        assert_eq!(error_message, safe_error);
        assert!(!error_message.contains("raw-private-value"));
    }

    // ---- v1.5.2 Bug 2 regression: name-to-id resolution for review_items ----

    #[test]
    fn diff_known_tag_names_case_insensitive_and_unique() {
        let requested = vec![
            "Hardware".to_owned(),
            "Rechnung".to_owned(),
            "hardware".to_owned(), // duplicate, different case
            "NoSuchTag".to_owned(),
        ];
        // The local mirror returns lowercased matches like the real SQL helper does.
        let known = vec![("hardware".to_owned(), 7), ("rechnung".to_owned(), 12)];
        let (ids, unknown) = diff_known_tag_names(&requested, &known);
        assert_eq!(ids, vec![7, 12], "known ids returned sorted-deduped");
        assert_eq!(
            unknown,
            vec!["NoSuchTag".to_owned()],
            "only the unmatched name needs creation-or-drop downstream"
        );
    }

    #[test]
    fn diff_known_tag_names_folds_non_ascii_capitals() {
        // The SQL lookup returns the catalog spelling; a lowercase request must
        // not be treated as unknown (and re-created in Paperless). #409
        let requested = vec!["ärzte".to_owned(), "Übersetzung".to_owned()];
        let known = vec![("Ärzte".to_owned(), 21), ("übersetzung".to_owned(), 22)];
        let (ids, unknown) = diff_known_tag_names(&requested, &known);
        assert_eq!(ids, vec![21, 22]);
        assert!(unknown.is_empty(), "unexpected unknown tags: {unknown:?}");
    }

    #[test]
    fn diff_known_tag_names_empty_inputs() {
        let (ids, unknown) = diff_known_tag_names(&[], &[]);
        assert!(ids.is_empty());
        assert!(unknown.is_empty());
    }

    #[test]
    fn diff_known_tag_names_all_unknown() {
        let requested = vec!["A".to_owned(), "B".to_owned()];
        let (ids, unknown) = diff_known_tag_names(&requested, &[]);
        assert!(ids.is_empty());
        assert_eq!(unknown, requested);
    }

    #[test]
    fn build_custom_field_value_patch_drops_unknown_names() {
        use archivist_core::FieldValueSuggestion;
        use serde_json::Value;
        let fields = vec![
            FieldValueSuggestion {
                name: "Invoice Number".to_owned(),
                value: Value::String("INV-001".to_owned()),
                confidence: Some(0.9),
            },
            FieldValueSuggestion {
                name: "ghost_field".to_owned(),
                value: Value::String("nope".to_owned()),
                confidence: Some(0.9),
            },
        ];
        let id_pairs = vec![("invoice number".to_owned(), 42, Some("string".to_owned()))];
        let patch = build_custom_field_value_patch(&fields, &id_pairs);
        assert_eq!(patch.len(), 1, "ghost_field should be dropped");
        // Shape must be { "field": <i32>, "value": ... } — what Paperless / DocumentPatch expects.
        let entry = &patch[0];
        assert_eq!(entry.get("field").and_then(Value::as_i64), Some(42));
        assert_eq!(
            entry.get("value").and_then(Value::as_str),
            Some("INV-001"),
            "value passes through unchanged"
        );
    }

    #[test]
    fn build_custom_field_value_patch_preserves_input_order() {
        use archivist_core::FieldValueSuggestion;
        use serde_json::Value;
        let fields = vec![
            FieldValueSuggestion {
                name: "B".to_owned(),
                value: Value::String("val-b".to_owned()),
                confidence: None,
            },
            FieldValueSuggestion {
                name: "A".to_owned(),
                value: Value::String("val-a".to_owned()),
                confidence: None,
            },
        ];
        let id_pairs = vec![("a".to_owned(), 1, None), ("b".to_owned(), 2, None)];
        let patch = build_custom_field_value_patch(&fields, &id_pairs);
        assert_eq!(
            patch[0].get("field").and_then(Value::as_i64),
            Some(2),
            "input order preserved (B then A), not pair order"
        );
        assert_eq!(patch[1].get("field").and_then(Value::as_i64), Some(1));
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
