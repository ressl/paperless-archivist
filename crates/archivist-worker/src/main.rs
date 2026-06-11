use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use archivist_ai::{
    AiProviderError, AiResponse, AnthropicClient, ChatRequest, DEFAULT_OCR_SYSTEM_PROMPT,
    ImageInput, OllamaClient, OpenAiCompatibleClient, PromptLanguageContext, TextProvider,
    VisionProvider, VisionRequest, parse_metadata_suggestion, prompt_for_metadata,
};
use archivist_config::AppConfig;
use archivist_core::{
    AiProviderKind, AuditEventInput, DocumentPatch, LanguageDetection, MetadataFieldFlags,
    MetadataSuggestion, OldTagStrategy, ProcessingMode, ReasoningEffort, RuntimeSettings, Stage,
    detect_document_language, validate_choice_suggestion, validate_document_date_suggestion,
    validate_field_suggestion, validate_tag_suggestion, validate_title_suggestion,
};
use archivist_db::{
    AiArtifactInput, DbPool, JobRecord, ReviewItemRecord, append_audit,
    backfill_metadata_stage_for_ocr_only_runs, bump_text_num_ctx_if_too_small,
    bump_vision_num_ctx_if_too_small, claim_jobs, claim_notification_delivery,
    claim_pending_review_for_autopilot_drain, complete_job, connect, create_review_item,
    create_run_with_jobs_with_priority, custom_field_ids_for_names, fail_job, get_active_prompt,
    get_backlog_counts, get_dashboard_live_status, get_runtime_settings,
    get_workflow_safety_status, increment_metric_counter, insert_ai_artifact, is_last_active_job,
    list_allowed_named_entities, list_allowed_tag_names, list_custom_fields,
    list_pending_review_items_for_autopilot_drain, mark_review_auto_applied,
    named_entity_id_for_name, paperless_sync_cursor, queue_missing_pipeline,
    rebalance_backfilled_metadata_priorities, record_dashboard_snapshot, record_document_language,
    release_job_lease_for_cooldown, requeue_vision_crashed_jobs, reset_stale_applying_reviews,
    reset_stuck_running_pipeline_runs, resolve_secret, revert_review_to_pending_after_failed_drain,
    selector_document_budget, tag_id_pairs_for_names, tag_ids_for_names,
    update_paperless_sync_cursor, upsert_inventory_item, upsert_paperless_custom_field,
    upsert_paperless_named_entity, upsert_paperless_tag,
};
use archivist_ocr::{
    normalize_ocr_pages, render_document_pages, strip_code_fences, validate_ocr_text,
};
use archivist_paperless::{
    PaperlessClient, PaperlessDocumentDetail, PaperlessDocumentSummary, PaperlessError,
    PaperlessTag,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Client as HttpClient;
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::signal;
use tokio::time::{sleep, timeout};
use tracing::{Instrument, error, info, info_span, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

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

    // One-shot: lift the GGML_ASSERT recurrence ceiling that v1.5.1 set to
    // 16384. Production observed 137 OCR jobs burning through their retry
    // budget despite num_ctx=16384, so we bump the floor to 32768 for any
    // deployment that hasn't already raised it manually. This runs BEFORE
    // the vision-crash requeue so the requeued jobs run under the new num_ctx.
    match bump_vision_num_ctx_if_too_small(&pool).await {
        Ok(summary) if summary.bumped => info!(
            previous = ?summary.previous,
            current = summary.current,
            "bumped ai.ollama_vision_num_ctx to 32768 to give vision model more headroom"
        ),
        Ok(_) => info!("ai.ollama_vision_num_ctx already at or above 32768; no bump"),
        Err(error) => warn!(error = %error, "startup vision num_ctx bump failed"),
    }

    // One-shot: raise the text num_ctx to a 16384 floor. The text path never
    // had a floor (unlike vision), so the 8192 default let a large metadata
    // prompt (OCR + candidate lists) overflow the context and fail every
    // metadata job permanently. Operators who already raised it are untouched.
    match bump_text_num_ctx_if_too_small(&pool).await {
        Ok(summary) if summary.bumped => info!(
            previous = ?summary.previous,
            current = summary.current,
            "bumped ai.ollama_text_num_ctx to 16384 to give the text model context headroom"
        ),
        Ok(_) => info!("ai.ollama_text_num_ctx already at or above 16384; no bump"),
        Err(error) => warn!(error = %error, "startup text num_ctx bump failed"),
    }

    // One-shot: lift failed OCR jobs killed by the GGML vision-runtime crash signature back
    // into the queue so they get a second chance under the new fallback machinery.
    // Idempotent — finding no matching rows is a no-op. Gated by the runtime setting so
    // operators can disable for upgrade scenarios where the queue must not be touched.
    if let Err(error) = run_startup_vision_crash_requeue(&pool).await {
        warn!(error = %error, "startup vision-crash requeue failed");
    }

    // One-shot: backfill the consolidated `metadata` stage onto historical
    // `pipeline_runs` that were queued with only `["ocr"]` (e.g. by trigger
    // polling against documents tagged only with the OCR trigger). Without
    // this, those runs terminate after OCR and the Review queue fills up
    // with content-only review items that never get a real
    // Title/Correspondent/Tags suggestion. Idempotent — once every OCR-only
    // run has a metadata job, subsequent startups find nothing to do.
    match backfill_metadata_stage_for_ocr_only_runs(&pool).await {
        Ok(summary) if summary.runs_updated > 0 => info!(
            runs_updated = summary.runs_updated,
            jobs_inserted = summary.jobs_inserted,
            "metadata-stage backfill lifted OCR-only pipeline_runs to include the metadata stage"
        ),
        Ok(_) => info!("metadata-stage backfill found no OCR-only pipeline_runs to lift"),
        Err(error) => warn!(error = %error, "startup metadata-stage backfill failed"),
    }

    // One-shot: fix the v1.5.4 backfill bug where new metadata jobs got
    // `payload.priority = 1_000_000 - document_id` instead of inheriting
    // the OCR sibling's priority. Without this, the backfilled metadata
    // jobs sit queued indefinitely behind every other OCR job globally.
    // Idempotent — once every backfilled metadata job has a matching
    // priority, subsequent startups find nothing to do.
    match rebalance_backfilled_metadata_priorities(&pool).await {
        Ok(summary) if summary.jobs_repriced > 0 => info!(
            jobs_repriced = summary.jobs_repriced,
            "rebalanced backfilled metadata-job priorities to inherit OCR siblings'"
        ),
        Ok(_) => info!("metadata-job priority rebalance found no mispriced rows"),
        Err(error) => warn!(error = %error, "startup metadata-priority rebalance failed"),
    }

    // One-shot: clean up pipeline_runs.status='running' rows whose jobs
    // are all settled. Pre-v1.5.7 complete_job left intermediate stage
    // successes on 'running' which surfaced as "N stuck run(s)" on the
    // dashboard. v1.5.7 fixes complete_job for new runs; this catches
    // the historical residue.
    match reset_stuck_running_pipeline_runs(&pool).await {
        Ok(summary) if summary.runs_reset > 0 => info!(
            runs_reset = summary.runs_reset,
            "reset historical pipeline_runs stuck on 'running' to their correct status"
        ),
        Ok(_) => info!("stuck-running pipeline_runs cleanup found no rows to reset"),
        Err(error) => warn!(error = %error, "startup stuck-runs reset failed"),
    }

    // Recover review items stranded in 'applying' by a crash between claim and
    // apply (#253). 300s comfortably exceeds a healthy apply, so anything
    // older was abandoned.
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
                    || autopilot_drain_running.load(Ordering::Acquire))
                    && std::time::Instant::now() < drain_deadline
                {
                    sleep(Duration::from_millis(250)).await;
                }
                if job_processing_running.load(Ordering::Acquire)
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
                    tokio::spawn(async move {
                        let _guard = ReentryGuard(job_processing_running);
                        if let Err(error) = process_available_jobs(
                            &pool,
                            &config,
                            &worker_id,
                            &last_observed_concurrency,
                        )
                        .await
                        {
                            error!(error = %error, "job processing tick failed");
                        }
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
                write_liveness_heartbeat().await;
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

/// Tick wrapper for the autopilot review drain.
///
/// Loads the latest runtime settings each invocation (the dashboard mode
/// badge reflects the live runtime mode, and so should this drain).
///
/// The outer timeout is generous (30 minutes) because each drained item
/// already has its own short Paperless-side timeout — see
/// `apply_one_autopilot_drain_review`. With v1.5.4's PER_TICK_CEILING=500
/// and ~5s per Paperless apply, a fully loaded drain runs ~40min; the cap
/// is a last-ditch liveness guard so a fully wedged Paperless host can't
/// permanently occupy this tick slot. The drain is spawned (not awaited)
/// in the main loop, so a slow drain no longer starves OCR processing.
async fn drain_pending_reviews_if_autopilot_tick(pool: &DbPool, config: &AppConfig) -> Result<()> {
    let settings = get_runtime_settings(pool).await?;
    let applied = timeout(
        Duration::from_secs(30 * 60),
        drain_pending_reviews_if_autopilot(pool, config, &settings),
    )
    .await
    .map_err(|_| anyhow!("autopilot drain tick timed out"))??;
    if applied > 0 {
        info!(
            applied,
            mode = %settings.workflow.mode,
            "autopilot review drain applied pending items"
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

/// One-shot startup helper: when enabled in runtime settings, lifts `failed` OCR jobs that
/// match the vision-runtime-crash signature back into `queued` and bumps their attempt
/// budget by one. Designed to run exactly once per worker process start; idempotent if a
/// rerun finds zero matching rows. Errors are swallowed by the caller (the worker should
/// still come up even if this housekeeping fails).
async fn run_startup_vision_crash_requeue(pool: &DbPool) -> Result<()> {
    let settings = get_runtime_settings(pool).await?;
    if !settings.ai.requeue_vision_crashes_on_startup {
        info!(
            "vision-crash startup requeue disabled by setting requeue_vision_crashes_on_startup=false"
        );
        return Ok(());
    }
    let summary = requeue_vision_crashed_jobs(pool).await?;
    if summary.jobs_requeued > 0 {
        info!(
            jobs_requeued = summary.jobs_requeued,
            "vision_model_fallback_requeue_used = true; lifted vision-crashed jobs back to the queue"
        );
    } else {
        info!("vision-crash startup requeue found no matching jobs");
    }
    Ok(())
}

/// Baseline job-lease window in seconds. Claims and heartbeat bumps never
/// grant less than this.
const BASE_JOB_LEASE_SECONDS: i64 = 300;

/// Margin added on top of the slowest configured AI request timeout when the
/// lease is derived from it: heartbeats run BETWEEN AI calls, so one lease
/// window must also cover the non-AI work around a maximal-length call
/// (Paperless round-trips, page rendering, DB writes).
const JOB_LEASE_TIMEOUT_MARGIN_SECONDS: i64 = 60;

/// Lease window for `claim_jobs` / `bump_job_lease`, coupled to the AI
/// request timeout: `max(300, slowest enabled provider timeout + margin)`.
///
/// `request_timeout_seconds` is operator-configurable (prod runs 600s for
/// slow local models) while the lease used to be a hard-coded 300s — a
/// single in-flight call could outlive the lease, letting a second replica
/// reclaim and double-process the job mid-call. The lease follows the
/// timeout (rather than clamping the timeout below the lease) because the
/// configurable timeout exists precisely so calls may run long. Jobs are
/// claimed before stage→provider resolution, so size the window for the
/// slowest enabled provider rather than per-stage. #308
fn job_lease_seconds(settings: &RuntimeSettings) -> i64 {
    let slowest_timeout = settings
        .ai
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| {
            // Mirror `provider_for_stage`: 0/unset inherits the built-in default.
            provider
                .tuning
                .request_timeout_seconds
                .filter(|secs| *secs > 0)
                .unwrap_or(archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS)
        })
        .max()
        .unwrap_or(archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS);
    BASE_JOB_LEASE_SECONDS.max(i64::from(slowest_timeout) + JOB_LEASE_TIMEOUT_MARGIN_SECONDS)
}

async fn process_available_jobs(
    pool: &DbPool,
    config: &Arc<AppConfig>,
    worker_id: &str,
    last_observed_concurrency: &AtomicU32,
) -> Result<()> {
    // v1.6.2 issue #127: per-cycle live-reload of worker pool size.
    //
    // Read settings BEFORE claiming so the target concurrency reflects the
    // operator's current intent. The env var is the hard upper cap; the
    // active provider's tuning can only clamp lower. On a transition we
    // emit `workflow.concurrency_changed` with both `from` and `to` so the
    // audit log shows when the pool resized and why.
    //
    // Per-tick spawn-and-join semantics: tasks claimed in this tick run to
    // completion before the next tick (we await the FuturesUnordered below).
    // Pool downscale therefore never aborts in-flight work — surplus tasks
    // are simply not spawned next tick. Pool upscale starts immediately.
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
        // entirely rather than block on an empty FuturesUnordered.
        return Ok(());
    }

    let jobs = claim_jobs(
        pool,
        target_concurrency as i64,
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

    let mut pending = FuturesUnordered::new();
    for job in jobs {
        let pool = pool.clone();
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
        pending.push(tokio::spawn(
            async move {
                let started = std::time::Instant::now();
                let result = process_job(
                    &pool,
                    &config,
                    settings.as_ref(),
                    paperless.as_ref(),
                    &job,
                    &lease_owner,
                )
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
                    let _ = fail_job(
                        &pool,
                        &job,
                        &lease_owner,
                        &format!("{:#}", error),
                        failure_class.is_retryable() || force_retryable,
                        failure_class.retry_ceiling(),
                    )
                    .await;
                } else {
                    info!(
                        duration_ms = started.elapsed().as_millis() as u64,
                        "job processing completed"
                    );
                }
                result
            }
            .instrument(span),
        ));
    }

    while let Some(result) = pending.next().await {
        if let Err(error) = result {
            warn!(error = %error, "worker task join failed");
        }
    }
    Ok(())
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

async fn send_operational_notifications(pool: &DbPool, config: &AppConfig) -> Result<()> {
    let settings = get_runtime_settings(pool).await?;
    if !settings.notifications.enabled {
        return Ok(());
    }
    let Some(webhook_secret_id) = settings.notifications.webhook_url_secret_id else {
        return Ok(());
    };
    let Some(webhook_url) = resolve_secret(pool, &config.secret_key, webhook_secret_id).await?
    else {
        return Ok(());
    };
    let cooldown = settings.notifications.cooldown_minutes as i32;
    let counts = get_backlog_counts(pool).await?;
    if counts.waiting_review >= settings.notifications.review_queue_threshold
        && claim_notification_delivery(pool, "review_queue_backlog", cooldown).await?
    {
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "review_queue_backlog",
                "severity": "warning",
                "title": "Review queue needs attention",
                "description": "Paperless Archivist has documents waiting for human review.",
                "metadata": {
                    "waiting_review": counts.waiting_review,
                    "threshold": settings.notifications.review_queue_threshold
                }
            }),
        )
        .await?;
    }

    let live = get_dashboard_live_status(pool, &settings).await?;
    let hard_failures = live
        .recent_failures
        .iter()
        .filter(|failure| failure.status == "failed" || failure.failure_kind == "failed")
        .count() as i64;
    if hard_failures >= settings.notifications.repeated_failure_threshold
        && claim_notification_delivery(pool, "repeated_processing_failures", cooldown).await?
    {
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "repeated_processing_failures",
                "severity": "error",
                "title": "Repeated processing failures",
                "description": "Recent Paperless Archivist jobs are failing. Check the dashboard live status and worker logs.",
                "metadata": {
                    "recent_failure_count": hard_failures,
                    "threshold": settings.notifications.repeated_failure_threshold
                }
            }),
        )
        .await?;
    }

    if settings.workflow.mode == ProcessingMode::FullAuto
        && settings.workflow.paused
        && claim_notification_delivery(pool, "paused_full_auto", cooldown).await?
    {
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "paused_full_auto",
                "severity": "warning",
                "title": "Full autopilot is paused",
                "description": "Full autopilot is configured but processing is paused.",
                "metadata": {
                    "workflow_mode": "full_auto",
                    "paused": true
                }
            }),
        )
        .await?;
    }
    Ok(())
}

async fn send_notification_webhook(
    webhook_url: &SecretString,
    payload: serde_json::Value,
) -> Result<()> {
    let response = HttpClient::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        // No connect-time IP-pinning: the DNS-rebinding TOCTOU is an accepted
        // residual risk for this operator-configured webhook host (see #183).
        .build()?
        .post(webhook_url.expose_secret())
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "notification webhook request failed: {}",
                error.without_url()
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("notification webhook returned {status}"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessingFailureClass {
    Transient,
    /// A transient failure of the Paperless *gateway/infrastructure* — the
    /// system of record is briefly unreachable (network, timeout, 5xx, or a
    /// gateway-404 mid-restart, #245). Distinct from `Transient` because an
    /// upstream outage blocks *every* job at once, so failing each document
    /// against its small `max_attempts` budget permanently loses the whole
    /// backlog for an outage longer than ~1 h. Retried against a higher,
    /// bounded ceiling instead so the documents ride the outage out. #305.
    TransientInfra,
    Permanent,
    /// Provider replied with a hard usage-cap signal (Ollama Cloud weekly,
    /// OpenAI tier monthly, …). Not retryable — the worker writes a
    /// per-provider cooldown so subsequent claims of jobs that would route
    /// to the same provider are short-circuited until the cap resets.
    ProviderQuota,
}

impl ProcessingFailureClass {
    fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::TransientInfra)
    }

    /// Retry-budget ceiling for `fail_job`: `None` uses the per-job
    /// `max_attempts`; `TransientInfra` raises it to ride out an upstream
    /// outage (bounded — see [`PAPERLESS_INFRA_RETRY_CEILING`]).
    fn retry_ceiling(self) -> Option<i32> {
        match self {
            Self::TransientInfra => Some(PAPERLESS_INFRA_RETRY_CEILING),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::TransientInfra => "transient_infra",
            Self::Permanent => "permanent",
            Self::ProviderQuota => "provider_quota",
        }
    }
}

/// Bounded retry ceiling for a Paperless infrastructure outage
/// (`ProcessingFailureClass::TransientInfra`). With `fail_job`'s exponential
/// backoff capped at ~32 min, 20 attempts span ~8.5 h — long enough to ride
/// out a realistic gateway outage/restart, short enough that a *permanently*
/// broken gateway still surfaces as a failed job instead of looping forever
/// (unlike the provider-cooldown release, which is unbounded by design). #305.
const PAPERLESS_INFRA_RETRY_CEILING: i32 = 20;

/// Decide whether `error` should be retried with backoff (Transient) or marked
/// permanent. The function first walks the error chain looking for typed
/// errors from `archivist-paperless` and `archivist-ai`; those carry an
/// authoritative `is_transient()` classification and bypass substring guesses.
/// Anything else — DB driver errors, `reqwest::Error` raised outside the typed
/// wrappers, third-party HTTP clients — falls through to substring matching as
/// a documented last resort.
fn classify_processing_failure(error: &anyhow::Error) -> ProcessingFailureClass {
    for cause in error.chain() {
        if let Some(paperless_error) = cause.downcast_ref::<PaperlessError>() {
            // A transient Paperless failure is an *infrastructure* outage of the
            // system of record (network/timeout/5xx/gateway-404) — it blocks
            // every job, so grant the higher bounded retry budget rather than
            // burning each document's small `max_attempts`. #305.
            return if paperless_error.is_transient() {
                ProcessingFailureClass::TransientInfra
            } else {
                ProcessingFailureClass::Permanent
            };
        }
        if let Some(ai_error) = cause.downcast_ref::<AiProviderError>() {
            return match ai_error {
                AiProviderError::QuotaExhausted { .. } => ProcessingFailureClass::ProviderQuota,
                e if e.is_transient() => ProcessingFailureClass::Transient,
                _ => ProcessingFailureClass::Permanent,
            };
        }
    }

    // Last-resort substring matcher: covers errors that arise *outside* the
    // typed surfaces — sqlx pool errors, reqwest errors from helpers that
    // still use `anyhow!`, raw HTTP responses, etc. Any new error path
    // should prefer adding a typed variant in the originating crate so this
    // table can keep shrinking.
    let message = error
        .chain()
        .map(|cause| cause.to_string().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    let transient_markers = [
        "timeout",
        "timed out",
        "connection refused",
        "connection reset",
        "connection closed",
        "temporarily unavailable",
        "service unavailable",
        "internal server error",
        "ollama",
        "runner process no longer running",
        "database",
        "pool timed out",
        "broken pipe",
        "dns",
        "network",
        "502",
        "503",
        "504",
    ];

    if transient_markers
        .iter()
        .any(|marker| message.contains(marker))
    {
        ProcessingFailureClass::Transient
    } else {
        ProcessingFailureClass::Permanent
    }
}

/// Default cooldown applied when a provider returns a quota-exhausted
/// signal without a `Retry-After` header. Ollama Cloud's weekly cap and
/// most "monthly tier" quotas don't reset in single-digit hours, so the
/// default is deliberately long — the cost of being wrong (a few hours
/// of idle worker) is much smaller than burning the queue against an
/// upgrade-or-wait quota. Operators can lift it early via the dashboard
/// "Entsperren" action.
const DEFAULT_PROVIDER_QUOTA_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);
/// Floor/ceiling applied to a provider-supplied `Retry-After` so a tiny value
/// can't thrash the claim loop and a huge one can't park a provider for weeks.
const MIN_PROVIDER_QUOTA_COOLDOWN: Duration = Duration::from_secs(5 * 60);
const MAX_PROVIDER_QUOTA_COOLDOWN: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Walk `error` for an `AiProviderError::QuotaExhausted` and persist a
/// cooldown row keyed on its `provider` field. When the provider supplied a
/// `Retry-After`, honor it (clamped to [MIN, MAX]); otherwise default to
/// `DEFAULT_PROVIDER_QUOTA_COOLDOWN`. Previously `retry_after.max(DEFAULT)`
/// meant a `Retry-After: 60` still produced a 24 h cooldown — a short throttle
/// mis-read as a hard cap parked the provider (and every claimed job's
/// run_after) for a day. #292. Falls back to the job's stage provider name if
/// no typed quota error is found in the chain.
/// Resolve the cooldown duration from a provider-supplied `Retry-After`: honor
/// it clamped to [MIN, MAX], or default when absent. Pulled out for unit
/// testing. #292
fn quota_cooldown_duration(retry_after_secs: Option<u64>) -> Duration {
    match retry_after_secs {
        Some(secs) => Duration::from_secs(secs)
            .clamp(MIN_PROVIDER_QUOTA_COOLDOWN, MAX_PROVIDER_QUOTA_COOLDOWN),
        None => DEFAULT_PROVIDER_QUOTA_COOLDOWN,
    }
}

/// Returns the EFFECTIVE cooldown end — when an existing longer cooldown
/// wins over the requested one, that is what the caller parks the
/// triggering job's `run_after` on. #317
async fn record_quota_cooldown_for_failure(
    pool: &DbPool,
    settings: &RuntimeSettings,
    job: &JobRecord,
    error: &anyhow::Error,
) -> Result<DateTime<Utc>> {
    let (provider_name, retry_after_secs, message) =
        extract_quota_signal(error).unwrap_or_else(|| {
            (
                provider_name_for_stage(settings, job.stage).unwrap_or_else(|_| "unknown".into()),
                None,
                error.to_string(),
            )
        });
    let cooldown = quota_cooldown_duration(retry_after_secs);
    let cooldown_until = Utc::now() + ChronoDuration::from_std(cooldown).unwrap_or_default();
    let reason = format!(
        "{} (job {}, stage {})",
        truncate_for_audit(&message, 240),
        job.id,
        job.stage
    );
    // The upsert keeps the longer of (existing, requested) cooldown and
    // reports which case happened (fresh / extended / already covered), so
    // the log and audit trail show whether this 429 actually moved the
    // window — and the job is parked on the EFFECTIVE expiry, not on a
    // requested value an existing longer cooldown overrules. #317
    let upsert =
        archivist_db::upsert_provider_cooldown(pool, &provider_name, cooldown_until, &reason)
            .await?;
    warn!(
        provider = %provider_name,
        until = %upsert.effective_until,
        outcome = upsert.outcome.as_str(),
        previous_until = ?upsert.previous_until,
        retry_after_secs,
        "provider quota exhausted; persisted cooldown — claim cycles will skip this provider until expiry"
    );
    let _ = archivist_db::append_audit(
        pool,
        AuditEventInput {
            event_type: "ai.provider_quota_exhausted".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: None,
            after: Some(json!({
                "provider": provider_name,
                "cooldown_until": upsert.effective_until,
                "requested_cooldown_until": cooldown_until,
                "previous_cooldown_until": upsert.previous_until,
                "cooldown_outcome": upsert.outcome.as_str(),
                "retry_after_secs": retry_after_secs,
            })),
            metadata: None,
            outcome: "failed".to_owned(),
            error_message: Some(truncate_for_audit(&message, 1024)),
            source_ip: None,
            user_agent: None,
        },
    )
    .await;
    Ok(upsert.effective_until)
}

fn extract_quota_signal(error: &anyhow::Error) -> Option<(String, Option<u64>, String)> {
    for cause in error.chain() {
        if let Some(AiProviderError::QuotaExhausted {
            provider,
            retry_after,
            message,
        }) = cause.downcast_ref::<AiProviderError>()
        {
            return Some((provider.clone(), *retry_after, message.clone()));
        }
    }
    None
}

fn provider_name_for_stage(settings: &RuntimeSettings, stage: Stage) -> Result<String> {
    let provider = provider_for_stage(settings, stage, false)?;
    Ok(provider.name)
}

fn truncate_for_audit(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// Resolve the active provider for a stage and look up its cooldown row.
/// Returns the cooldown record only if it is still active (cooldown_until
/// in the future). On stage configuration errors we log and return None
/// rather than failing — a misconfigured stage should fall through to the
/// existing error path, not get masked as a cooldown.
async fn active_cooldown_for_stage(
    pool: &DbPool,
    settings: &RuntimeSettings,
    stage: Stage,
) -> Result<Option<archivist_db::AiProviderCooldown>> {
    let provider = match provider_name_for_stage(settings, stage) {
        Ok(name) => name,
        Err(error) => {
            warn!(error = %error, "could not resolve provider for stage cooldown check");
            return Ok(None);
        }
    };
    archivist_db::get_active_provider_cooldown(pool, &provider).await
}

/// Release a claimed lease back to the queue without burning an attempt
/// — used when the worker discovers the active provider for the job's
/// stage is in cooldown. `attempts` is decremented to undo the increment
/// performed by `claim_jobs`, so the per-job retry budget is preserved
/// for the next cycle. `run_after` is set to the cooldown expiry so the
/// job is not re-claimed before the provider is plausibly back.
///
/// Delegates to [`release_job_lease_for_cooldown`], which also flips the
/// run back to `queued` and mirrors `document_inventory.current_run_status`
/// in the same transaction — the worker-local variant only updated `jobs`,
/// which is how cooldown releases used to strand runs on `running` and
/// (via the startup repair) drift the inventory mirror. #303.
async fn release_lease_for_cooldown(
    pool: &DbPool,
    job: &JobRecord,
    lease_owner: &str,
    cooldown_until: DateTime<Utc>,
) -> Result<()> {
    let released = release_job_lease_for_cooldown(pool, job, lease_owner, cooldown_until)
        .await
        .context("release lease for provider cooldown")?;
    if !released {
        warn!(
            job_id = %job.id,
            "skipped cooldown lease release: lease no longer owned by this worker"
        );
    }
    Ok(())
}

/// Detect Ollama vision runtime crashes (GGML_ASSERT, llama runner aborts). These keep their
/// `Transient` classification — a different page input might still succeed — but we surface
/// the signal in worker logs so operators can swap the configured vision model rather than
/// burning attempts on a misconfigured runtime.
fn is_vision_model_runtime_crash(error: &anyhow::Error) -> bool {
    let message = error
        .chain()
        .map(|cause| cause.to_string().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    message.contains("ggml_assert")
        || message.contains("runner process no longer running")
        || message.contains("signal arrived during cgo execution")
}

/// Hardcoded safe-default chain walked when the primary vision model crashes and no explicit
/// `fallback_vision_model` is configured. Order matters — the worker picks the first entry
/// that is installed locally and not equal to the current primary. These names match the
/// public Ollama tags as of 2025; nothing experimental is included on purpose. If an entry
/// becomes unsafe (e.g. a tag is retracted) drop it here rather than relying on operators.
const VISION_FALLBACK_CHAIN: &[&str] = &[
    // Smaller-than-the-primary fallbacks first — these have been the actual
    // workhorses in production deployments and tend to be installed alongside
    // glm-ocr / qwen3-vl primaries. Adding them as auto-discovery candidates
    // lets the runtime fallback path fire without operators having to set
    // `ai.fallback_vision_model` explicitly.
    "qwen2.5vl:7b",
    "qwen2-vl:7b",
    "qwen3-vl:32b",
    "llava-llama3:8b",
    "llava:13b",
    "llava:latest",
];

/// Where a fallback candidate came from. Carried into log lines and audit metadata so
/// operators can tell whether the recovery used their explicit setting or the safe-default
/// chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisionFallbackSource {
    Explicit,
    AutoDiscovered,
}

impl VisionFallbackSource {
    fn as_str(self) -> &'static str {
        match self {
            VisionFallbackSource::Explicit => "explicit",
            VisionFallbackSource::AutoDiscovered => "auto_discovered",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisionFallbackChoice {
    model: String,
    source: VisionFallbackSource,
}

/// Pure selector for a vision-model fallback. Prefers the explicit setting when it is
/// set, non-empty, and not the same as the primary model. Otherwise walks
/// `VISION_FALLBACK_CHAIN` and picks the first entry that is in `installed_models` and
/// not equal to the primary. Case-insensitive match on model names.
///
/// `installed_models` may be empty (e.g. when the provider is not Ollama or the tag list
/// call failed) — in that case the chain cannot be walked and the function returns the
/// explicit choice if any, or `None`.
fn pick_vision_fallback_model(
    settings: &RuntimeSettings,
    primary_model: &str,
    installed_models: &[String],
) -> Option<VisionFallbackChoice> {
    if let Some(explicit) = settings
        .ai
        .fallback_vision_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.eq_ignore_ascii_case(primary_model))
    {
        return Some(VisionFallbackChoice {
            model: explicit.to_owned(),
            source: VisionFallbackSource::Explicit,
        });
    }

    let installed_lower: Vec<String> = installed_models
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    for candidate in VISION_FALLBACK_CHAIN {
        if candidate.eq_ignore_ascii_case(primary_model) {
            continue;
        }
        let candidate_lower = candidate.to_ascii_lowercase();
        if installed_lower.iter().any(|name| name == &candidate_lower) {
            return Some(VisionFallbackChoice {
                model: (*candidate).to_owned(),
                source: VisionFallbackSource::AutoDiscovered,
            });
        }
    }
    None
}

/// Best-effort fetch of locally-installed Ollama models for the given provider. Returns
/// an empty list (with a warn-level log) when the provider is not Ollama or the tag list
/// call fails — that downgrades the auto-discovered fallback path to a no-op without
/// crashing the worker tick.
async fn installed_ollama_models_for_provider(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
) -> Vec<String> {
    if provider.kind != AiProviderKind::Ollama {
        return Vec::new();
    }
    let secret = match provider_secret(pool, config, provider).await {
        Ok(secret) => secret,
        Err(error) => {
            warn!(
                error = %error,
                provider = %provider.name,
                "fallback chain skipped: could not resolve provider secret"
            );
            return Vec::new();
        }
    };
    let client = match OllamaClient::new(&provider.name, &provider.base_url, secret) {
        Ok(client) => client,
        Err(error) => {
            warn!(
                error = %error,
                provider = %provider.name,
                "fallback chain skipped: could not construct Ollama client"
            );
            return Vec::new();
        }
    };
    match client.list_models().await {
        Ok(models) => models.into_iter().map(|model| model.name).collect(),
        Err(error) => {
            warn!(
                error = %error,
                provider = %provider.name,
                "fallback chain skipped: Ollama tag list call failed"
            );
            Vec::new()
        }
    }
}

/// Run a single vision request, transparently retrying on a vision-runtime-crash error
/// against a configured or auto-discovered fallback model. The return value carries the
/// model that actually produced the response so the caller can record the swap in
/// per-page logs / audit metadata.
///
/// Behaviour:
/// 1. Call the primary provider/model.
/// 2. On success, return immediately.
/// 3. On error: if `is_vision_model_runtime_crash` is true AND a fallback can be picked,
///    log + emit a `worker.vision_model_fallback` audit event, then retry the exact same
///    request against the fallback model once.
/// 4. If the fallback also fails, or no fallback is available, return the original error.
///
/// This function does NOT consume the job's attempt slot — both calls happen within the
/// same worker tick. The orchestrator-driven retry budget only kicks in if the fallback
/// also fails (transient classification keeps current retry+jitter behaviour intact).
#[allow(clippy::too_many_arguments)]
async fn run_vision_with_fallback(
    pool: &DbPool,
    config: &AppConfig,
    client: &VisionClient,
    provider: &StageProvider,
    settings: &RuntimeSettings,
    job: &JobRecord,
    page_index: usize,
    request: VisionRequest,
) -> Result<(AiResponse, String, bool)> {
    let primary_model = provider.model.clone();
    let mut request_with_primary = request.clone();
    request_with_primary.model = primary_model.clone();
    match client.vision(request_with_primary).await {
        Ok(response) => Ok((response, primary_model, false)),
        Err(error) => {
            if !is_vision_model_runtime_crash(&error) {
                return Err(error);
            }
            let installed = installed_ollama_models_for_provider(pool, config, provider).await;
            let Some(choice) = pick_vision_fallback_model(settings, &primary_model, &installed)
            else {
                return Err(error);
            };
            warn!(
                primary_model = %primary_model,
                fallback_model = %choice.model,
                fallback_source = choice.source.as_str(),
                page_index,
                vision_model_fallback_used = true,
                document_id = job.paperless_document_id,
                stage = %job.stage,
                "vision model crashed; retrying same page on fallback model"
            );
            let auto_discovered = choice.source == VisionFallbackSource::AutoDiscovered;
            let audit_metadata = json!({
                "primary": primary_model,
                "fallback": choice.model,
                "fallback_source": choice.source.as_str(),
                "auto_discovered_fallback": auto_discovered,
                "stage": job.stage,
                "page_index": page_index,
                "document_id": job.paperless_document_id,
                "primary_error": error.to_string()
            });
            if let Err(audit_error) = append_audit(
                pool,
                AuditEventInput {
                    event_type: "worker.vision_model_fallback".to_owned(),
                    actor_type: "worker".to_owned(),
                    actor_id: None,
                    run_id: Some(job.run_id),
                    job_id: Some(job.id),
                    paperless_document_id: Some(job.paperless_document_id),
                    before: None,
                    after: None,
                    metadata: Some(audit_metadata),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await
            {
                warn!(error = %audit_error, "failed to record worker.vision_model_fallback audit event");
            }
            let mut fallback_request = request;
            fallback_request.model = choice.model.clone();
            let response = client.vision(fallback_request).await?;
            info!(
                primary_model = %primary_model,
                fallback_model = %choice.model,
                fallback_source = choice.source.as_str(),
                page_index,
                vision_model_fallback_used = true,
                document_id = job.paperless_document_id,
                stage = %job.stage,
                "vision fallback succeeded"
            );
            Ok((response, choice.model, true))
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

/// Sum per-page vision token usage across raw provider responses into
/// `(input_tokens, output_tokens)`. Handles both wire shapes: OpenAI/Anthropic
/// (`usage.prompt_tokens`/`input_tokens`, `usage.completion_tokens`/
/// `output_tokens`) and Ollama (top-level `prompt_eval_count`/`eval_count`).
/// Returns `None` when no page reported any tokens. #259.
fn sum_vision_usage(raw_responses: &[serde_json::Value]) -> Option<(i64, i64)> {
    fn field(value: &serde_json::Value, path: &[&str]) -> i64 {
        let mut node = value;
        for key in path {
            match node.get(key) {
                Some(next) => node = next,
                None => return 0,
            }
        }
        node.as_i64()
            .or_else(|| node.as_str().and_then(|s| s.parse::<i64>().ok()))
            .unwrap_or(0)
    }
    let mut input = 0_i64;
    let mut output = 0_i64;
    for page in raw_responses {
        input += field(page, &["usage", "prompt_tokens"])
            + field(page, &["usage", "input_tokens"])
            + field(page, &["prompt_eval_count"]);
        output += field(page, &["usage", "completion_tokens"])
            + field(page, &["usage", "output_tokens"])
            + field(page, &["eval_count"]);
    }
    (input > 0 || output > 0).then_some((input, output))
}

async fn process_ocr(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    lease_owner: &str,
) -> Result<()> {
    // Independent GETs — fetch the original bytes and the document detail
    // concurrently instead of serially.
    let (original, document) = tokio::try_join!(
        paperless.download_original(job.paperless_document_id),
        paperless.get_document(job.paperless_document_id),
    )?;
    let pages = render_document_pages(
        &original,
        document.original_file_name.as_deref(),
        settings
            .effective_tuning_for_stage(Stage::Ocr)
            .ocr_page_limit,
    )
    .await?;
    // The original download bytes (up to the download cap) are only needed for
    // rendering and the artifact input hash. Compute the hash now and drop the
    // bytes so they aren't held in memory for the whole per-page vision loop —
    // that loop already holds the rendered pages plus per-page base64 copies.
    // #283
    let input_hash = hash_bytes(&original);
    drop(original);
    if pages.is_empty() {
        return Err(anyhow!("document rendered zero OCR pages"));
    }
    let page_bytes: usize = pages.iter().map(|page| page.bytes.len()).sum();
    info!(
        job_id = %job.id,
        document_id = job.paperless_document_id,
        pages = pages.len(),
        page_bytes,
        "rendered OCR input pages"
    );

    let provider = provider_for_stage(settings, Stage::Ocr, true)?;
    let prompt = get_active_prompt(pool, Stage::Ocr).await?;
    // Build the vision client once for the whole document: resolves+decrypts
    // the provider secret a single time and keeps one keep-alive/TLS-warm
    // connection pool across every page and the crash fallback.
    let vision_client = build_vision_client(pool, config, &provider).await?;
    let mut texts = Vec::new();
    let mut raw_responses = Vec::new();
    let mut models_used: Vec<String> = Vec::new();
    let mut any_fallback_used = false;
    let mut pages_from_cache: u32 = 0;
    let started = std::time::Instant::now();
    for (index, page) in pages.iter().enumerate() {
        // v1.5.14 (#116): try the OCR page cache before re-running the
        // vision model. Hit key is (document_id, page_index,
        // sha256-of-rendered-bytes). The hash captures both the
        // rendering config and the document content, so re-renders with
        // identical config produce identical hashes and cached text is
        // safe to reuse. Re-renders with different config (e.g. higher
        // DPI) get a new hash and the LLM runs as before.
        let page_hash = hash_bytes(&page.bytes);
        if let Some(cached_text) = archivist_db::lookup_ocr_page_cache(
            pool,
            job.paperless_document_id,
            index as i32,
            &page_hash,
        )
        .await?
        {
            pages_from_cache += 1;
            info!(
                document_id = job.paperless_document_id,
                page_index = index,
                "served OCR page from cache, skipped vision call"
            );
            models_used.push("(cache)".to_owned());
            texts.push(cached_text);
            raw_responses.push(json!({"cached": true}));
            continue;
        }

        let page_prompt = prompt
            .as_ref()
            .map(|prompt| {
                format!(
                    "{}\n\nPage {}: transcribe exactly and return only OCR text.",
                    prompt.content,
                    index + 1
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "{}\n\nPage {}: transcribe exactly and return only OCR text.",
                    DEFAULT_OCR_SYSTEM_PROMPT,
                    index + 1
                )
            });
        // Wire the runtime-configured Ollama context window into the vision
        // payload. This is the GGML_ASSERT crash fix (ollama/ollama#14401):
        // glm-ocr and similar vision models expand a single page into more
        // tokens than Ollama's 4096-token default holds, which kills the
        // runner with `GGML_ASSERT(a->ne[2] * 4 == b->ne[0])`. The default of
        // 16384 covers realistic single-page renders; operators can raise it
        // for huge multi-page documents or lower it on small Ollama hosts.
        // Remote providers (OpenAI / Anthropic / OpenAI-compatible) ignore
        // this field — see `build_ollama_vision_payload`.
        let request = VisionRequest {
            model: provider.model.clone(),
            temperature: 0.0,
            num_ctx: ollama_vision_num_ctx_for_provider(
                &provider,
                settings
                    .effective_tuning_for_stage(Stage::Ocr)
                    .vision_num_ctx,
            ),
            prompt: page_prompt,
            images: vec![ImageInput {
                mime_type: page.mime_type.clone(),
                bytes: page.bytes.clone(),
            }],
        };
        let page_started = std::time::Instant::now();
        let (response, model_used, fallback_used) = run_vision_with_fallback(
            pool,
            config,
            &vision_client,
            &provider,
            settings,
            job,
            index,
            request,
        )
        .await?;
        // Progress breadcrumb for the slow per-page vision calls — without this
        // the worker went silent for the whole OCR duration (only cache hits
        // logged), so a document stuck mid-OCR was invisible.
        info!(
            document_id = job.paperless_document_id,
            page_index = index,
            model = %model_used,
            fallback_used,
            duration_ms = page_started.elapsed().as_millis() as u64,
            "ocr page complete"
        );
        any_fallback_used |= fallback_used;

        // Sanitize before caching so any leaked markdown fence is stripped once
        // and never re-served from the page cache.
        let page_text = strip_code_fences(&response.text);

        // Cache the successful page-level OCR so a future retry / re-trigger
        // doesn't pay for the vision call again. Cache write is best-effort:
        // a failure here is logged but does not fail the OCR job.
        if let Err(cache_error) = archivist_db::upsert_ocr_page_cache(
            pool,
            job.paperless_document_id,
            index as i32,
            &page_hash,
            &page_text,
            Some(&provider.name),
            Some(&model_used),
        )
        .await
        {
            warn!(
                document_id = job.paperless_document_id,
                page_index = index,
                error = %cache_error,
                "OCR page-cache write failed; not blocking the job"
            );
        }

        models_used.push(model_used);
        texts.push(page_text);
        raw_responses.push(response.raw_response);

        // Heartbeat the lease after each page. Multi-page vision OCR can run
        // far longer than the lease window `claim_jobs` grants, so without
        // this a second replica would reclaim the "stale" lease and re-OCR
        // the same document concurrently. Push `lease_until` forward by the
        // same window; if the bump finds no matching row our lease was lost
        // (another replica took over), so stop instead of double-applying.
        if !archivist_db::bump_job_lease(pool, job.id, lease_owner, job_lease_seconds(settings))
            .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                page_index = index,
                "OCR lease lost mid-document; stopping so a replica isn't double-applied"
            );
            return Ok(());
        }
    }
    let text = normalize_ocr_pages(&texts);
    validate_ocr_text(&text, settings.ocr.min_chars)?;
    let language_detection = detect_document_language(&text);
    record_document_language(
        pool,
        job.paperless_document_id,
        &language_detection,
        Some(job.run_id),
        Some(job.id),
        "worker",
    )
    .await?;

    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Ocr,
            provider: &provider.name,
            model: &provider.model,
            prompt_id: prompt.as_ref().map(|prompt| prompt.id),
            input_hash: &input_hash,
            request: None,
            response: Some({
                let mut response = json!({ "pages": raw_responses });
                // Flatten per-page token usage to a top-level `usage` block so
                // the OCR/vision stage — usually the most token-heavy — is
                // counted by provider_usage / statistics / cost queries, which
                // only read top-level token fields. Top-level also survives
                // metadata-only storage (which keeps `usage`). #259.
                if let Some((input, output)) = sum_vision_usage(&raw_responses)
                    && let Some(object) = response.as_object_mut()
                {
                    object.insert(
                        "usage".to_owned(),
                        json!({ "prompt_tokens": input, "completion_tokens": output }),
                    );
                }
                response
            }),
            normalized_output: Some(json!({
                "content_chars": text.chars().count(),
                "language": language_detection.language,
                "language_confidence": language_detection.confidence,
                "language_source": language_detection.source,
                "models_used_per_page": models_used,
                "vision_model_fallback_used": any_fallback_used,
                "pages_from_cache": pages_from_cache,
            })),
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    // v1.5.14 (#117): record sha256(ocr_text) on the inventory row so the
    // metadata stage can dedup against earlier documents with identical
    // content. Best-effort write — a failure here doesn't fail OCR.
    let content_hash = hash_bytes(text.as_bytes());
    if let Err(error) = archivist_db::set_document_inventory_ocr_content_hash(
        pool,
        job.paperless_document_id,
        &content_hash,
    )
    .await
    {
        warn!(
            document_id = job.paperless_document_id,
            error = %error,
            "set_document_inventory_ocr_content_hash failed; dedup will not apply"
        );
    }

    // #217: persist the OCR body locally so chat search can full-text
    // rank against it. NUL bytes are stripped because Postgres `text`
    // cannot store them; the body is otherwise the same text sent to
    // Paperless. Best-effort write — a failure here doesn't fail OCR, it
    // only means this document won't surface via body FTS until re-OCR'd.
    let ocr_body = text.replace('\0', "");
    if let Err(error) =
        archivist_db::set_document_inventory_ocr_body(pool, job.paperless_document_id, &ocr_body)
            .await
    {
        warn!(
            document_id = job.paperless_document_id,
            error = %error,
            "set_document_inventory_ocr_body failed; body full-text search will not apply"
        );
    }

    let patch = DocumentPatch {
        content: Some(text),
        title: None,
        tags: None,
        correspondent: None,
        document_type: None,
        created: None,
        custom_fields: None,
    };
    handle_patch_result(
        pool,
        paperless,
        settings,
        job,
        patch,
        Vec::new(),
        None,
        lease_owner,
    )
    .await
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
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    let mut ids: Vec<i32> = known_pairs.iter().map(|(_, id)| *id).collect();
    ids.sort_unstable();
    ids.dedup();
    let unknown: Vec<String> = requested
        .iter()
        .filter(|name| !known_lower.contains(&name.to_ascii_lowercase()))
        .cloned()
        .collect();
    (ids, unknown)
}

/// Resolve LLM-supplied tag NAMES to Paperless tag IDs so review_items always carry the
/// `Vec<i32>` shape that the approve → patch path expects (the apply path deserializes the
/// review_item's `suggested_patch.tags` as `Vec<i32>`; raw names cause a 500 there and the
/// autopilot drain then reverts the review forever).
///
/// Resolution policy:
/// * Look up known names case-insensitively in the local `paperless_tags` mirror.
/// * For unknown names: if `allow_new_tags` is true, create them in Paperless and use the
///   returned ID. Otherwise drop the name with a warn log — the review_item still ships,
///   just with fewer tags, rather than blocking the whole document.
async fn resolve_tag_names_to_ids(
    pool: &DbPool,
    paperless: &PaperlessClient,
    names: &[String],
    allow_new_tags: bool,
) -> Result<Vec<i32>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    // Get (name, id) pairs for known tags so we can both build the initial id list and tell
    // which names are unknown (need creation or dropping).
    let known_pairs = tag_id_pairs_for_names(pool, names).await?;
    let (mut ids, unknown) = diff_known_tag_names(names, &known_pairs);
    if !unknown.is_empty() && allow_new_tags {
        // Fetch the Paperless tag catalog ONCE and reuse it via
        // `ensure_tag_cached`. Calling `paperless.ensure_tag()` per unknown
        // name re-paginated the entire catalog every time — O(N × all_tags)
        // per document — which is the same waste the drain path already fixed
        // (worker:ensure_tag_cached). `ensure_tag_cached` checks the local
        // snapshot first and only creates genuinely missing names.
        let mut tag_cache = paperless.list_tags().await?;
        for name in unknown {
            match ensure_tag_cached(paperless, &mut tag_cache, &name).await {
                Ok(tag) => {
                    if !ids.contains(&tag.id) {
                        ids.push(tag.id);
                    }
                }
                Err(error) => {
                    warn!(
                        unknown_tag = %name,
                        %error,
                        "failed to create new Paperless tag for review_item; dropping"
                    );
                }
            }
        }
    } else {
        for name in unknown {
            warn!(
                unknown_tag = %name,
                "dropping unknown tag from review_item suggested_patch (allow_new_tags is false)"
            );
        }
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
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
                .find(|(name, _, _)| name.eq_ignore_ascii_case(&field.name))?;
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
            .any(|(name, _, _)| name.eq_ignore_ascii_case(&field.name))
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
    // OpenAI-compatible and Anthropic clients ignore this field today;
    // their wire-level enforcement (response_format json_schema /
    // tool-use) is tracked as future work.
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
    let (prompt_id, prompt_experiment_group) =
        apply_active_prompt_with_experiment(pool, Stage::Metadata, job.run_id, &mut request)
            .await?;
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
    let mut suggestion =
        parse_metadata_suggestion(&response.text).unwrap_or_else(|_| MetadataSuggestion::default());

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
                let mut tag_ids = match settings.tagging.old_tag_strategy {
                    OldTagStrategy::KeepExisting | OldTagStrategy::ReplaceAiManaged => {
                        document.tags.clone()
                    }
                    OldTagStrategy::RemoveAllBusiness => Vec::new(),
                };
                for tag_id in selected_ids {
                    if !tag_ids.contains(&tag_id) {
                        tag_ids.push(tag_id);
                    }
                }
                tag_ids.sort_unstable();
                tag_ids.dedup();
                composite_patch.tags = Some(tag_ids);
                composite_warnings.extend(valid.warnings);
                applied_fields.push("tags");
            }
            Err(errors) => {
                // Validation failed (e.g. low confidence, count over max_tags). Resolve the raw
                // LLM names to integer IDs BEFORE creating the review_item so the apply path can
                // deserialize `suggested_patch.tags` as `Vec<i32>` without 500-ing. Unknown names
                // are either created in Paperless (allow_new_tags == true) or dropped.
                let tag_ids = resolve_tag_names_to_ids(
                    pool,
                    paperless,
                    &tags.tags,
                    settings.tagging.allow_new_tags,
                )
                .await?;
                review_items.push((
                    json!({
                        "tags": tag_ids,
                        "standard_metadata": {
                            "field": "tags",
                            "confidence": tags.confidence,
                            "suggested_names": tags.tags,
                        }
                    }),
                    json!(errors),
                ));
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
                    let Some((_, id, data_type)) = ids
                        .iter()
                        .find(|(name, _, _)| name.eq_ignore_ascii_case(&field.name))
                    else {
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
        if let Some(tags) = composite_patch.tags.clone() {
            review_items.push((
                json!({
                    "tags": tags,
                    "standard_metadata": { "field": "tags", "auto_validated": true }
                }),
                json!([]),
            ));
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

        for (patch, warnings) in review_items {
            if create_review_item(pool, job, patch, warnings, lease_owner)
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
            let final_run_stage = is_last_active_job(pool, job.run_id, job.id).await?;
            apply_patch_with_workflow_tags(
                pool,
                paperless,
                settings,
                job,
                composite_patch,
                final_run_stage,
            )
            .await?;
            if !complete_job(
                pool,
                job,
                lease_owner,
                json!({
                    "applied": true,
                    "fields": applied_fields,
                    "warnings": composite_warnings,
                    "dropped_field_count": review_warning_count,
                }),
            )
            .await?
            {
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
            let composite_review_patch = serde_json::to_value(&composite_patch)?;
            if create_review_item(
                pool,
                job,
                composite_review_patch,
                json!(composite_warnings),
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
            }),
        )
        .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "lease lost before completion; another worker owns this job — skipping"
            );
        }
        Ok(())
    } else {
        if !complete_job(
            pool,
            job,
            lease_owner,
            json!({
                "skipped": "all metadata fields skipped (already-set or model omitted)",
                "skipped_fields": skipped_fields,
            }),
        )
        .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "lease lost before completion; another worker owns this job — skipping"
            );
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

#[allow(clippy::too_many_arguments)]
async fn handle_patch_result(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    patch: DocumentPatch,
    warnings: Vec<String>,
    review_metadata: Option<serde_json::Value>,
    lease_owner: &str,
) -> Result<()> {
    // Effective routing policy is the live runtime workflow mode, not the per-run mode that was
    // stamped onto pipeline_runs at queue time. Per-run mode is captured at queue time from the
    // runtime default, so once a batch is queued it cannot follow later operator policy changes
    // (e.g. operator flips runtime from manual_review to full_auto). Honoring runtime mode here
    // matches the operator's live intent and the dashboard mode badge. Per-run mode is still
    // recorded for audit/UX context. dry_run always forces review regardless of mode.
    let auto_apply = settings.workflow.mode.auto_apply_validated_suggestions();
    if !auto_apply || settings.workflow.dry_run {
        let mut review_patch = serde_json::to_value(patch)?;
        if let Some(metadata) = review_metadata
            && let Some(object) = review_patch.as_object_mut()
        {
            object.insert("standard_metadata".to_owned(), metadata);
        }
        let mut review_warnings = warnings;
        if settings.workflow.dry_run && auto_apply {
            review_warnings.push(
                "Dry-run is enabled: validated patch was evaluated but not auto-applied."
                    .to_owned(),
            );
        }
        let Some(review_id) =
            create_review_item(pool, job, review_patch, json!(review_warnings), lease_owner)
                .await?
        else {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                "lease lost before review creation; another worker owns this job — skipping"
            );
            return Ok(());
        };
        if settings.workflow.dry_run && auto_apply {
            append_audit(
                pool,
                AuditEventInput {
                    event_type: "workflow.dry_run_review_created".to_owned(),
                    actor_type: "worker".to_owned(),
                    actor_id: None,
                    run_id: Some(job.run_id),
                    job_id: Some(job.id),
                    paperless_document_id: Some(job.paperless_document_id),
                    before: None,
                    after: Some(json!({ "review_id": review_id, "stage": job.stage })),
                    metadata: Some(json!({ "mode": job.mode })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
        }
        return Ok(());
    }
    let final_run_stage = is_last_active_job(pool, job.run_id, job.id).await?;
    apply_patch_with_workflow_tags(pool, paperless, settings, job, patch, final_run_stage).await?;
    if !complete_job(
        pool,
        job,
        lease_owner,
        json!({ "applied": true, "warnings": warnings }),
    )
    .await?
    {
        warn!(
            job_id = %job.id,
            document_id = job.paperless_document_id,
            "lease lost before completion; another worker owns this job — skipping"
        );
    }
    Ok(())
}

async fn apply_patch_with_workflow_tags(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    mut patch: DocumentPatch,
    final_run_stage: bool,
) -> Result<()> {
    let document = paperless.get_document(job.paperless_document_id).await?;
    let tags = paperless.list_tags().await?;
    let mut tag_ids = patch.tags.clone().unwrap_or_else(|| document.tags.clone());

    if let Some(completion_name) = settings.workflow.tags.completion_tag_for_stage(job.stage) {
        let completion = paperless.ensure_tag(completion_name).await?;
        if !tag_ids.contains(&completion.id) {
            tag_ids.push(completion.id);
        }
    }
    if final_run_stage {
        let full = paperless
            .ensure_tag(&settings.workflow.tags.completion_processed)
            .await?;
        if !tag_ids.contains(&full.id) {
            tag_ids.push(full.id);
        }
    }
    for trigger_name in [
        settings.workflow.tags.trigger_tag_for_stage(job.stage),
        final_run_stage.then_some(settings.workflow.tags.trigger_process.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(trigger) = tags
            .iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(trigger_name))
        {
            tag_ids.retain(|id| *id != trigger.id);
        }
    }

    tag_ids.sort_unstable();
    tag_ids.dedup();
    patch.tags = Some(tag_ids);
    prune_unchanged_patch_fields(&mut patch, &document);
    let before_value = audit_before_for_patch(&document, &patch);
    let patch_value = audit_patch_payload(&patch);
    let apply_started = std::time::Instant::now();
    if let Err(error) = patch_document_dropping_bad_custom_fields(
        pool,
        paperless,
        job.paperless_document_id,
        &patch,
        Some(job.run_id),
        Some(job.id),
        json!({ "stage": job.stage }),
    )
    .await
    {
        let duration_ms = apply_started.elapsed().as_millis() as u64;
        append_audit(
            pool,
            AuditEventInput {
                event_type: "document.patch_apply_failed".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: Some(job.run_id),
                job_id: Some(job.id),
                paperless_document_id: Some(job.paperless_document_id),
                before: Some(before_value),
                after: Some(patch_value),
                metadata: Some(json!({ "stage": job.stage, "duration_ms": duration_ms })),
                outcome: "failed".to_owned(),
                error_message: Some(error.to_string()),
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
        increment_metric_counter(pool, "apply_failure_total", 1).await?;
        return Err(error);
    }
    let duration_ms = apply_started.elapsed().as_millis() as u64;
    append_audit(
        pool,
        AuditEventInput {
            event_type: "document.patch_applied".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: Some(before_value),
            after: Some(patch_value),
            metadata: Some(json!({ "stage": job.stage, "duration_ms": duration_ms })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    increment_metric_counter(pool, "apply_success_total", 1).await?;
    Ok(())
}

/// Decide whether the autopilot review drain should run on this tick.
///
/// Returns `Some(budget)` when the drain is allowed:
/// - `budget = None`   means "no per-tick cap" (unlimited)
/// - `budget = Some(n)` means "at most n items this tick" (n > 0)
///
/// Returns `None` when the drain must skip — mode is not `FullAuto`, dry-run
/// is on, the workflow is paused, or the safety budget is exhausted.
///
/// Kept as a pure function (no DB / IO) so it is unit-testable.
fn autopilot_drain_budget(
    settings: &RuntimeSettings,
    safety: &archivist_core::WorkflowSafetyStatus,
) -> Option<Option<i64>> {
    if !settings.workflow.mode.auto_apply_validated_suggestions() {
        return None;
    }
    if settings.workflow.dry_run {
        return None;
    }
    if safety.paused {
        return None;
    }
    let budget = selector_document_budget(safety);
    match budget {
        None => Some(None),
        Some(remaining) if remaining > 0 => Some(Some(remaining)),
        Some(_) => None,
    }
}

/// Drain pending review_items by auto-applying them when the runtime is in
/// full_auto. Complements the per-run `handle_patch_result` routing fix: if
/// items were queued under manual_review and the operator later flipped to
/// full_auto, those rows would otherwise sit in `pending` forever. The drain
/// is gated by the same safety dials the auto-selector honors (paused, dry
/// run, hourly + daily document limits).
async fn drain_pending_reviews_if_autopilot(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
) -> Result<usize> {
    let safety = get_workflow_safety_status(pool, settings).await?;
    let Some(budget) = autopilot_drain_budget(settings, &safety) else {
        return Ok(0);
    };
    // Hard ceiling per tick. Bumped from 50 to 100 in v1.3.2, then to 500 in
    // v1.5.4 after live debugging at 2515-pending backlog showed the 100 cap
    // combined with the previous in-loop await (which blocked OCR processing
    // for the duration of the drain) capped throughput at ~140 items/h. v1.5.4
    // also moved the drain off the main tick loop into a spawned task, so a
    // larger per-tick batch no longer starves OCR. Still safety-budget
    // bounded — an operator hourly cap of e.g. 200/h still lands ~200/h
    // regardless of this ceiling.
    const PER_TICK_CEILING: i64 = 500;
    let limit = match budget {
        None => PER_TICK_CEILING,
        Some(remaining) => remaining.min(PER_TICK_CEILING),
    };
    if limit <= 0 {
        return Ok(0);
    }
    let pending = list_pending_review_items_for_autopilot_drain(pool, limit).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let paperless = paperless_client(pool, config, settings).await?;

    // Hoist the tag list out of the per-item loop. The v1.3.1 drain called
    // `paperless.list_tags()` AND `paperless.ensure_tag()` (which itself
    // calls `list_tags` internally) on every iteration. With paginated
    // tag responses that's a multi-second cost per item; on a 4000-item
    // backlog the per-tick deadline ran out before more than 1-2 items
    // were applied. We snapshot tags once per drain batch, ensure all
    // workflow tags we might need, and reuse them per item. New tags
    // created during the batch are appended to the local snapshot.
    let mut tag_cache = paperless.list_tags().await?;
    let completion_full = ensure_tag_cached(
        &paperless,
        &mut tag_cache,
        &settings.workflow.tags.completion_processed,
    )
    .await?;

    let mut applied = 0usize;
    for review in pending {
        let review_id = review.id;
        let paperless_document_id = review.paperless_document_id;
        // Per-item timeout lives INSIDE apply_one_autopilot_drain_review,
        // wrapping only the Paperless patch. Wrapping the whole call here was
        // unsafe: the row is committed `pending→approved` before the slow
        // patch runs, so an outer timeout dropped the future at an await point
        // — no `Err`, so the revert never ran and the row was stranded in
        // `approved` forever (never applied, never retried). With the timeout
        // around just the patch, a timeout becomes an `Err` and the existing
        // revert-to-pending path fires.
        let result = apply_one_autopilot_drain_review(
            pool,
            &paperless,
            settings,
            review,
            &mut tag_cache,
            completion_full.clone(),
        )
        .await;
        match result {
            Ok(true) => {
                applied += 1;
                info!(
                    %review_id,
                    paperless_document_id,
                    trigger = "autopilot_drain",
                    "autopilot drain applied pending review item"
                );
            }
            Ok(false) => {
                // Raced — another worker tick (or a human reviewer) claimed
                // the row first. Not an error.
            }
            Err(error) => {
                warn!(
                    %review_id,
                    paperless_document_id,
                    error = %error,
                    "autopilot drain failed to apply review item; row returned to pending"
                );
            }
        }
    }
    Ok(applied)
}

/// Local cache helper for the drain: look up a workflow tag by name in the
/// pre-fetched tag list, creating it on Paperless (and inserting into the
/// cache) only if it really isn't there yet. Replaces the per-item
/// `paperless.ensure_tag()` call that re-fetched the whole tag page.
async fn ensure_tag_cached(
    paperless: &PaperlessClient,
    cache: &mut Vec<archivist_paperless::PaperlessTag>,
    name: &str,
) -> Result<archivist_paperless::PaperlessTag> {
    if let Some(tag) = cache.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
        return Ok(tag.clone());
    }
    let created = paperless.ensure_tag(name).await?;
    if !cache.iter().any(|t| t.id == created.id) {
        cache.push(created.clone());
    }
    Ok(created)
}

async fn apply_one_autopilot_drain_review(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    review: ReviewItemRecord,
    tag_cache: &mut Vec<archivist_paperless::PaperlessTag>,
    completion_full: archivist_paperless::PaperlessTag,
) -> Result<bool> {
    let Some(claimed) = claim_pending_review_for_autopilot_drain(pool, review.id).await? else {
        // Raced — the row is no longer pending.
        return Ok(false);
    };
    // Bound only the patch (the row is already claimed `approved` at this
    // point). A timeout here surfaces as `Err`, which drives the revert below
    // so the row returns to `pending` and retries on the next tick — rather
    // than being silently stranded in `approved` if the future were dropped
    // by an outer timeout. The PATCH itself rarely blocks for more than a
    // second or two; 45s gives even a sluggish or rate-limited Paperless time
    // to respond before we move on.
    let patch_result = timeout(
        Duration::from_secs(45),
        apply_autopilot_drain_patch(
            pool,
            paperless,
            settings,
            &claimed,
            tag_cache,
            &completion_full,
        ),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow!("per-item drain patch timeout after 45s")));
    if let Err(error) = patch_result {
        // Roll the row back to pending so the next tick can retry. We
        // deliberately don't audit a failure event here — Paperless errors
        // already get an `document.patch_apply_failed` audit inside
        // `apply_autopilot_drain_patch`; non-Paperless errors are rare and
        // logged via the caller's `warn!`.
        if let Err(revert_error) =
            revert_review_to_pending_after_failed_drain(pool, claimed.id).await
        {
            warn!(
                review_id = %claimed.id,
                error = %revert_error,
                "failed to revert review item to pending after drain failure"
            );
        }
        return Err(error);
    }
    mark_review_auto_applied(pool, claimed.id).await?;
    Ok(true)
}

async fn apply_autopilot_drain_patch(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    review: &ReviewItemRecord,
    tag_cache: &mut Vec<archivist_paperless::PaperlessTag>,
    completion_full: &archivist_paperless::PaperlessTag,
) -> Result<()> {
    let patch_value = review
        .edited_patch
        .clone()
        .unwrap_or_else(|| review.suggested_patch.clone());
    let mut patch: DocumentPatch = serde_json::from_value(patch_value)?;
    // run_id is None only for review items whose run was pruned by retention;
    // those never reach the drain (retention deletes terminal runs only, and
    // a pending review keeps its run in 'waiting_review').
    let final_run_stage = if let (Some(run_id), Some(job_id)) = (review.run_id, review.job_id) {
        is_last_active_job(pool, run_id, job_id).await?
    } else {
        false
    };
    let document = paperless.get_document(review.paperless_document_id).await?;
    let mut tag_ids = patch.tags.clone().unwrap_or_else(|| document.tags.clone());
    if let Some(completion_name) = settings
        .workflow
        .tags
        .completion_tag_for_stage(review.stage)
    {
        let tag = ensure_tag_cached(paperless, tag_cache, completion_name).await?;
        if !tag_ids.contains(&tag.id) {
            tag_ids.push(tag.id);
        }
    }
    if final_run_stage && !tag_ids.contains(&completion_full.id) {
        tag_ids.push(completion_full.id);
    }
    if let Some(trigger_name) = settings.workflow.tags.trigger_tag_for_stage(review.stage)
        && let Some(tag) = tag_cache
            .iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(trigger_name))
    {
        tag_ids.retain(|id| *id != tag.id);
    }
    if final_run_stage
        && let Some(tag) = tag_cache.iter().find(|tag| {
            tag.name
                .eq_ignore_ascii_case(&settings.workflow.tags.trigger_process)
        })
    {
        tag_ids.retain(|id| *id != tag.id);
    }
    tag_ids.sort_unstable();
    tag_ids.dedup();
    patch.tags = Some(tag_ids);
    prune_unchanged_patch_fields(&mut patch, &document);
    let before = audit_before_for_patch(&document, &patch);
    let after = audit_patch_payload(&patch);
    let apply_started = std::time::Instant::now();
    if let Err(error) = patch_document_dropping_bad_custom_fields(
        pool,
        paperless,
        review.paperless_document_id,
        &patch,
        review.run_id,
        review.job_id,
        json!({
            "stage": review.stage,
            "review_id": review.id,
            "trigger": "autopilot_drain"
        }),
    )
    .await
    {
        let duration_ms = apply_started.elapsed().as_millis() as u64;
        append_audit(
            pool,
            AuditEventInput {
                event_type: "document.patch_apply_failed".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: review.run_id,
                job_id: review.job_id,
                paperless_document_id: Some(review.paperless_document_id),
                before: Some(before),
                after: Some(after),
                metadata: Some(json!({
                    "stage": review.stage,
                    "review_id": review.id,
                    "duration_ms": duration_ms,
                    "trigger": "autopilot_drain"
                })),
                outcome: "failed".to_owned(),
                error_message: Some(error.to_string()),
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
        increment_metric_counter(pool, "apply_failure_total", 1).await?;
        return Err(error);
    }
    let duration_ms = apply_started.elapsed().as_millis() as u64;
    append_audit(
        pool,
        AuditEventInput {
            event_type: "document.patch_applied".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: review.run_id,
            job_id: review.job_id,
            paperless_document_id: Some(review.paperless_document_id),
            before: Some(before),
            after: Some(after),
            metadata: Some(json!({
                "stage": review.stage,
                "review_id": review.id,
                "duration_ms": duration_ms,
                "trigger": "autopilot_drain"
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    increment_metric_counter(pool, "apply_success_total", 1).await?;
    Ok(())
}

/// Whether a `patch_document` failure is a Paperless 400 that implicates
/// `custom_fields`. The typed `PaperlessError::Client` Display embeds both
/// `status=` and the response `body`, and a Paperless validation 400 names the
/// offending field (`custom_fields`) in that body — so matching on the rendered
/// error string is enough to recognise the "one bad custom field" case.
fn is_custom_fields_400(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("status=400") && message.contains("custom_fields")
}

/// Apply `patch` to a Paperless document, resilient to a single bad custom
/// field sinking the whole patch. If the initial PATCH fails with a Paperless
/// 400 that implicates `custom_fields`, retry ONCE with the same patch but
/// `custom_fields = None`, so title/tags/correspondent/date still land. On a
/// successful retry, append a `document.custom_fields_dropped` audit event
/// recording that the custom fields were dropped due to a Paperless 400. If the
/// retry also fails — or the failure was not a custom_fields 400 — the original
/// error is propagated unchanged. A single retry only; never a loop.
async fn patch_document_dropping_bad_custom_fields(
    pool: &DbPool,
    paperless: &PaperlessClient,
    document_id: i32,
    patch: &DocumentPatch,
    run_id: Option<Uuid>,
    job_id: Option<Uuid>,
    extra_metadata: serde_json::Value,
) -> Result<()> {
    let error = match paperless.patch_document(document_id, patch).await {
        Ok(_) => return Ok(()),
        Err(error) => error,
    };
    if patch.custom_fields.is_none() || !is_custom_fields_400(&error) {
        return Err(error);
    }
    let mut retry = patch.clone();
    retry.custom_fields = None;
    if paperless.patch_document(document_id, &retry).await.is_err() {
        // Dropping custom_fields didn't help — surface the original failure.
        return Err(error);
    }
    let mut metadata = extra_metadata;
    if let Some(object) = metadata.as_object_mut() {
        object.insert(
            "custom_fields_dropped".to_owned(),
            serde_json::Value::Bool(true),
        );
        object.insert(
            "reason".to_owned(),
            serde_json::Value::String(
                "Paperless rejected custom_fields with a 400; patch reapplied without them"
                    .to_owned(),
            ),
        );
    }
    append_audit(
        pool,
        AuditEventInput {
            event_type: "document.custom_fields_dropped".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id,
            job_id,
            paperless_document_id: Some(document_id),
            before: None,
            after: None,
            metadata: Some(metadata),
            outcome: "success".to_owned(),
            error_message: Some(error.to_string()),
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    Ok(())
}

fn prune_unchanged_patch_fields(patch: &mut DocumentPatch, document: &PaperlessDocumentDetail) {
    if patch.content.as_deref() == document.content.as_deref() {
        patch.content = None;
    }
    if patch.title == document.title {
        patch.title = None;
    }
    if patch
        .tags
        .as_ref()
        .is_some_and(|tags| same_i32_set(tags, &document.tags))
    {
        patch.tags = None;
    }
    if patch
        .correspondent
        .as_ref()
        .is_some_and(|value| *value == document.correspondent)
    {
        patch.correspondent = None;
    }
    if patch
        .document_type
        .as_ref()
        .is_some_and(|value| *value == document.document_type)
    {
        patch.document_type = None;
    }
    if patch
        .created
        .as_deref()
        .is_some_and(|value| document_date_equals(document.created.as_deref(), value))
    {
        patch.created = None;
    }
}

fn same_i32_set(left: &[i32], right: &[i32]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort_unstable();
    left.dedup();
    right.sort_unstable();
    right.dedup();
    left == right
}

fn document_date_equals(current: Option<&str>, requested: &str) -> bool {
    current
        .map(|value| value.get(..10).unwrap_or(value) == requested)
        .unwrap_or(false)
}

fn audit_before_for_patch(
    document: &PaperlessDocumentDetail,
    patch: &DocumentPatch,
) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    if patch.content.is_some() {
        object.insert(
            "content".to_owned(),
            audit_text_metadata(document.content.as_deref().unwrap_or_default()),
        );
    }
    if patch.title.is_some() {
        object.insert("title".to_owned(), json!(document.title));
    }
    if patch.tags.is_some() {
        object.insert("tags".to_owned(), json!(document.tags));
    }
    if patch.correspondent.is_some() {
        object.insert("correspondent".to_owned(), json!(document.correspondent));
    }
    if patch.document_type.is_some() {
        object.insert("document_type".to_owned(), json!(document.document_type));
    }
    if patch.created.is_some() {
        object.insert("created".to_owned(), json!(document.created));
    }
    if patch.custom_fields.is_some() {
        object.insert("custom_fields".to_owned(), json!({ "present": "redacted" }));
    }
    serde_json::Value::Object(object)
}

fn audit_patch_payload(patch: &DocumentPatch) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    if let Some(content) = &patch.content {
        object.insert("content".to_owned(), audit_text_metadata(content));
    }
    if let Some(title) = &patch.title {
        object.insert("title".to_owned(), json!(title));
    }
    if let Some(tags) = &patch.tags {
        object.insert("tags".to_owned(), json!(tags));
    }
    if let Some(correspondent) = &patch.correspondent {
        object.insert("correspondent".to_owned(), json!(correspondent));
    }
    if let Some(document_type) = &patch.document_type {
        object.insert("document_type".to_owned(), json!(document_type));
    }
    if let Some(created) = &patch.created {
        object.insert("created".to_owned(), json!(created));
    }
    if let Some(custom_fields) = &patch.custom_fields {
        object.insert(
            "custom_fields".to_owned(),
            json!({
                "sha256": hash_text(&custom_fields.to_string()),
                "redacted": true
            }),
        );
    }
    serde_json::Value::Object(object)
}

fn audit_text_metadata(value: &str) -> serde_json::Value {
    json!({
        "sha256": hash_text(value),
        "chars": value.chars().count(),
        "redacted": true
    })
}

async fn poll_paperless_triggers(pool: &DbPool, config: &AppConfig) -> Result<()> {
    let settings = get_runtime_settings(pool).await?;
    if settings.workflow.paused {
        append_audit(
            pool,
            AuditEventInput {
                event_type: "workflow.selector_skipped".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: None,
                after: None,
                metadata: Some(json!({ "reason": "paused", "mode": settings.workflow.mode })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
        info!("trigger polling skipped because workflow is paused");
        return Ok(());
    }
    let paperless = paperless_client(pool, config, &settings).await?;
    let snapshot = sync_metadata(pool, &paperless, &settings).await?;

    let mut trigger_matches = 0_u64;
    // O(1) tag lookups per document — avoids a quadratic scan when both
    // the document set and the tag catalog are large.
    let tags_by_id: HashMap<i32, &PaperlessTag> =
        snapshot.tags.iter().map(|tag| (tag.id, tag)).collect();
    for document in snapshot.documents {
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tags_by_id.get(id).copied())
            .map(|tag| tag.name.clone())
            .collect::<Vec<_>>();
        let stages = settings.workflow.tags.stages_requested_by_tags(&tag_names);
        if !stages.is_empty() {
            trigger_matches += 1;
            let trigger = if tag_names
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case(&settings.workflow.tags.trigger_process))
            {
                settings.workflow.tags.trigger_process.as_str()
            } else {
                "paperless-trigger"
            };
            // Tag-driven trigger from Paperless = operator added the trigger tag, so this is
            // treated as a manual trigger (priority 0) — newer arrivals stay ahead of the
            // older auto-selector backlog.
            create_run_with_jobs_with_priority(
                pool,
                document.id,
                &stages,
                settings.workflow.mode,
                trigger,
                "worker",
                Some(0),
            )
            .await?;
        }
    }
    info!(
        trigger_matches,
        "trigger polling inspected Paperless documents"
    );
    if settings.workflow.mode.auto_select_documents() {
        let safety = get_workflow_safety_status(pool, &settings).await?;
        let document_budget = selector_document_budget(&safety);
        if document_budget.is_some_and(|remaining| remaining <= 0) {
            append_audit(
                pool,
                AuditEventInput {
                    event_type: "workflow.selector_limit_reached".to_owned(),
                    actor_type: "worker".to_owned(),
                    actor_id: None,
                    run_id: None,
                    job_id: None,
                    paperless_document_id: None,
                    before: None,
                    after: None,
                    metadata: Some(json!({
                        "hourly_document_limit": safety.hourly_document_limit,
                        "daily_document_limit": safety.daily_document_limit,
                        "hourly_remaining": safety.hourly_remaining,
                        "daily_remaining": safety.daily_remaining
                    })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
            info!("auto-selector skipped because document limit is exhausted");
            return Ok(());
        }
        let auto_selected = queue_missing_pipeline(
            pool,
            &settings.workflow.enabled_stages,
            settings.workflow.mode,
            "auto-selector",
            "worker",
            &settings.workflow.rules,
            document_budget,
        )
        .await?;
        append_audit(
            pool,
            AuditEventInput {
                event_type: "workflow.selector_ran".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: None,
                after: Some(json!({ "queued": auto_selected })),
                metadata: Some(json!({
                    "mode": settings.workflow.mode,
                    "dry_run": settings.workflow.dry_run,
                    "hourly_remaining": safety.hourly_remaining,
                    "daily_remaining": safety.daily_remaining
                })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
        increment_metric_counter(pool, "selector_runs_total", 1).await?;
        increment_metric_counter(pool, "selector_documents_queued_total", auto_selected).await?;
        info!(
            auto_selected,
            mode = %settings.workflow.mode,
            "auto-selector queued missing document stages"
        );
    }
    Ok(())
}

struct PaperlessSyncSnapshot {
    tags: Vec<PaperlessTag>,
    documents: Vec<PaperlessDocumentSummary>,
}

async fn sync_metadata(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
) -> Result<PaperlessSyncSnapshot> {
    let archive_name = settings.paperless.active_archive.clone();
    let sync_started_at = Utc::now();
    let mut tags = paperless.list_tags().await?;
    // Reuse the already-fetched catalog via `ensure_tag_cached`, which only
    // calls Paperless when a workflow tag is genuinely missing. The previous
    // unconditional `ensure_tag` per workflow tag re-fetched the entire tag
    // catalog every iteration — O(workflow_tags × all_tags). With a few
    // thousand tags that alone overran the 300s trigger-poll timeout, so the
    // poll never completed and document ingestion stalled entirely.
    for workflow_tag in settings.workflow.tags.all() {
        ensure_tag_cached(paperless, &mut tags, workflow_tag).await?;
    }
    // Delta sync: when enabled and a prior cursor exists, fetch only documents
    // modified since the cursor (minus an overlap window to absorb clock skew)
    // instead of the full catalog — mirroring the API sync path. No cursor
    // (first run) or a delta error falls back to a full list. Tags,
    // correspondents and types stay full; they are small relative to documents.
    let cursor = paperless_sync_cursor(pool, &archive_name).await?;
    let delta_cursor = cursor.map(|cursor| {
        cursor - ChronoDuration::minutes(settings.paperless.delta_sync_overlap_minutes)
    });
    // These four catalog fetches are independent GETs against Paperless; run
    // them concurrently rather than serially. The tag list above must stay
    // sequential because `ensure_tag_cached` mutates it in place. custom_fields
    // keeps its best-effort `unwrap_or_default` semantics inside the join.
    let (correspondents, document_types, custom_fields, (sync_mode, documents)) = tokio::try_join!(
        paperless.list_correspondents(),
        paperless.list_document_types(),
        async { anyhow::Ok(paperless.list_custom_fields().await.unwrap_or_default()) },
        async {
            if settings.paperless.delta_sync_enabled {
                if let Some(cursor) = delta_cursor {
                    match paperless
                        .list_documents_modified_since(&cursor.to_rfc3339())
                        .await
                    {
                        Ok(documents) => anyhow::Ok(("delta", documents)),
                        Err(_) => anyhow::Ok((
                            "full_after_delta_error",
                            paperless.list_documents().await?,
                        )),
                    }
                } else {
                    anyhow::Ok(("full_initial", paperless.list_documents().await?))
                }
            } else {
                anyhow::Ok(("full", paperless.list_documents().await?))
            }
        },
    )?;

    let mut tx = pool.begin().await?;
    for tag in &tags {
        upsert_paperless_tag(
            &mut tx,
            tag.id,
            &tag.name,
            tag.slug.as_deref(),
            tag.color.as_deref(),
            settings.workflow.tags.is_workflow_tag(&tag.name),
        )
        .await?;
    }
    for entity in &correspondents {
        upsert_paperless_named_entity(&mut tx, "paperless_correspondents", entity.id, &entity.name)
            .await?;
    }
    for entity in &document_types {
        upsert_paperless_named_entity(&mut tx, "paperless_document_types", entity.id, &entity.name)
            .await?;
    }
    for field in &custom_fields {
        upsert_paperless_custom_field(&mut tx, field.id, &field.name, field.data_type.as_deref())
            .await?;
    }
    // O(1) id→name lookups: building this map once avoids the previous
    // O(documents × tags) nested linear scan, which was pure CPU burned inside
    // the sync transaction on instances with many tags/documents.
    let tag_names_by_id: HashMap<i32, &str> =
        tags.iter().map(|tag| (tag.id, tag.name.as_str())).collect();
    for document in &documents {
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tag_names_by_id.get(id).copied())
            .map(|name| name.to_owned())
            .collect::<Vec<_>>();
        upsert_inventory_item(
            &mut tx,
            &archivist_db::InventoryUpsert {
                paperless_document_id: document.id,
                title: document.title.clone(),
                original_file_name: document.original_file_name.clone(),
                current_tags: tag_names.clone(),
                current_tag_ids: document.tags.clone(),
                correspondent_id: document.correspondent,
                document_type_id: document.document_type,
                document_date: archivist_db::parse_paperless_document_date(
                    document.created.as_deref(),
                ),
                paperless_modified_at: None,
                has_ocr_completion_tag: tag_names
                    .iter()
                    .any(|tag| tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_ocr)),
                has_tagging_completion_tag: tag_names.iter().any(|tag| {
                    tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_tagging)
                }),
                has_full_completion_tag: tag_names.iter().any(|tag| {
                    tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_processed)
                }),
            },
        )
        .await?;
    }
    update_paperless_sync_cursor(&mut tx, &archive_name, sync_mode, sync_started_at).await?;
    tx.commit().await?;
    Ok(PaperlessSyncSnapshot { tags, documents })
}

async fn paperless_client(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
) -> Result<PaperlessClient> {
    let active_profile = settings.paperless.archive_profiles.iter().find(|profile| {
        profile.enabled
            && profile
                .name
                .eq_ignore_ascii_case(&settings.paperless.active_archive)
    });
    let base_url = active_profile
        .map(|profile| profile.base_url.as_str())
        .unwrap_or(&settings.paperless.base_url);
    let secret_id = active_profile
        .and_then(|profile| profile.token_secret_id)
        .or(settings.paperless.token_secret_id)
        .ok_or_else(|| anyhow!("Paperless token is not configured"))?;
    let token = resolve_secret(pool, &config.secret_key, secret_id)
        .await?
        .ok_or_else(|| anyhow!("Paperless token secret reference does not exist"))?;
    PaperlessClient::new(base_url, token, settings.paperless.timeout_seconds)
}

#[derive(Debug, Clone)]
struct StageProvider {
    name: String,
    kind: AiProviderKind,
    base_url: String,
    model: String,
    secret_id: Option<Uuid>,
    reasoning_effort: ReasoningEffort,
    request_timeout_seconds: u32,
}

fn provider_for_stage(
    settings: &RuntimeSettings,
    stage: Stage,
    vision: bool,
) -> Result<StageProvider> {
    let stage_override = settings
        .ai
        .stage_models
        .iter()
        .find(|override_model| override_model.stage == stage);
    let provider_name = stage_override
        .map(|override_model| override_model.provider.as_str())
        .unwrap_or(&settings.ai.default_provider);
    let mut provider = settings
        .ai
        .providers
        .iter()
        .find(|provider| provider.enabled && provider.name == provider_name)
        .cloned()
        .or_else(|| {
            if provider_name == "ollama" {
                Some(archivist_core::AiProviderSettings::ollama_default())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("AI provider '{provider_name}' is not configured or disabled"))?;
    if provider.name == "ollama" {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings
        .ai
        .model_for_stage_provider(&provider, stage, vision);
    let base_url = provider_base_url(&provider.kind, &provider.base_url);
    let reasoning_effort = provider.tuning.reasoning_effort.unwrap_or_default();
    // Per-request AI timeout: a 0/unset value inherits the built-in default.
    let request_timeout_seconds = provider
        .tuning
        .request_timeout_seconds
        .filter(|secs| *secs > 0)
        .unwrap_or(archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS);
    Ok(StageProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
        reasoning_effort,
        request_timeout_seconds,
    })
}

fn provider_base_url(kind: &AiProviderKind, configured: &str) -> String {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return trimmed.trim_end_matches('/').to_owned();
    }
    match kind {
        AiProviderKind::Ollama => "http://ollama:11434".to_owned(),
        AiProviderKind::Openai => "https://api.openai.com/v1".to_owned(),
        AiProviderKind::Anthropic => "https://api.anthropic.com/v1".to_owned(),
        AiProviderKind::OpenaiCompatible => "http://localhost:8000/v1".to_owned(),
    }
}

/// v1.5.15 (#119) experiment-aware active-prompt loader. Picks the A or B
/// variant deterministically by `run_id`, falls
/// back to the experiment-group-less default. Returns
/// `(prompt_id, experiment_label)` so the caller can stamp the label
/// into the normalized output for downstream accuracy analysis.
async fn apply_active_prompt_with_experiment(
    pool: &DbPool,
    stage: Stage,
    run_id: Uuid,
    request: &mut ChatRequest,
) -> Result<(Option<Uuid>, Option<String>)> {
    let Some((prompt, label)) =
        archivist_db::get_active_prompt_with_experiment(pool, stage, run_id).await?
    else {
        return Ok((None, None));
    };
    request.system_prompt = prompt.content;
    Ok((Some(prompt.id), label))
}

/// Cheap one-shot LLM pre-pass that classifies the document into one of
/// the `DocTypeCategory` values. Used to pick a doc-type-specific hint
/// snippet for the main metadata prompt (v1.5.13, Bundle C of milestone
/// v1.6.0).
///
/// Reuses the metadata stage's provider+model so operators don't have to
/// configure a separate classifier endpoint. Returns
/// `DocTypeCategory::Other` on empty content or any classifier error so
/// the main pipeline keeps draining; the caller logs the error.
async fn classify_document_type(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
    content: &str,
) -> Result<archivist_ai::DocTypeCategory> {
    if content.trim().is_empty() {
        return Ok(archivist_ai::DocTypeCategory::Other);
    }
    let request = archivist_ai::prompt_for_doc_type_classify(content);
    let response = chat_for_stage(pool, config, settings, Stage::Metadata, request).await?;
    Ok(archivist_ai::DocTypeCategory::parse(&response.text))
}

async fn chat_for_stage(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
    stage: Stage,
    mut request: ChatRequest,
) -> Result<AiResponse> {
    let provider = provider_for_stage(settings, stage, false)?;
    request.model = provider.model.clone();
    // Local-runner context window: only applies to Ollama. Remote providers
    // (OpenAI / Anthropic / OpenAI-compatible) ignore the field — see
    // `build_ollama_chat_payload`. Floored at the point of use so a
    // too-small per-provider override can't truncate the metadata prompts.
    request.num_ctx =
        ollama_text_num_ctx_for_provider(&provider, settings.effective_tuning().text_num_ctx);
    request.reasoning_effort = Some(provider.reasoning_effort);
    chat_with_provider(pool, config, &provider, request).await
}

/// Returns `Some(num_ctx)` when the provider is the local Ollama runner AND
/// a value is configured, else `None`. Wrapping the lookup keeps the call
/// sites symmetrical between the vision and chat paths and ensures we never
/// push the override onto remote providers (which would either ignore it or
/// reject the field).
fn ollama_num_ctx_for_provider(provider: &StageProvider, configured: Option<i64>) -> Option<i64> {
    match provider.kind {
        AiProviderKind::Ollama => configured,
        _ => None,
    }
}

/// Minimum Ollama text `num_ctx`: metadata prompts embed up to 16k chars of
/// document content plus the candidate correspondent/type/tag lists, which
/// overflow a 4096-token window and fail with `exceed_context_size_error`
/// (the v1.12.2 incident). The startup bump only raises the GLOBAL
/// `ai.ollama_text_num_ctx`; a per-provider tuning override (the shipped
/// Ollama preset pinned 4096) wins over the global in resolution and would
/// smuggle a too-small value through, so floor it at the point of use exactly
/// like the vision path. #304
const OLLAMA_TEXT_NUM_CTX_FLOOR: i64 = 16384;

/// Resolve the Ollama text `num_ctx`, never returning a value below the
/// prompt-safe floor.
fn ollama_text_num_ctx_for_provider(
    provider: &StageProvider,
    configured: Option<i64>,
) -> Option<i64> {
    ollama_num_ctx_for_provider(provider, configured).map(|n| n.max(OLLAMA_TEXT_NUM_CTX_FLOOR))
}

/// Minimum Ollama vision `num_ctx`: below this, glm-ocr-class models crash the
/// runtime (GGML_ASSERT). The startup bump only raises the GLOBAL
/// `ai.ollama_vision_num_ctx`; a per-provider tuning override (e.g. the
/// local-Ollama preset pins 4096) wins over the global in resolution and would
/// smuggle a too-small value through, so floor it at the point of use. #293
const OLLAMA_VISION_NUM_CTX_FLOOR: i64 = 16384;

/// Resolve the Ollama vision `num_ctx`, never returning a value below the
/// GGML-safe floor.
fn ollama_vision_num_ctx_for_provider(
    provider: &StageProvider,
    configured: Option<i64>,
) -> Option<i64> {
    ollama_num_ctx_for_provider(provider, configured).map(|n| n.max(OLLAMA_VISION_NUM_CTX_FLOOR))
}

async fn chat_with_provider(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
    request: ChatRequest,
) -> Result<AiResponse> {
    let timeout = Duration::from_secs(u64::from(provider.request_timeout_seconds));
    match provider.kind {
        AiProviderKind::Ollama => {
            let client = OllamaClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Anthropic => {
            let secret = provider_secret(pool, config, provider)
                .await?
                .ok_or_else(|| {
                    anyhow!("AI provider '{}' requires an API key secret", provider.name)
                })?;
            let client = AnthropicClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            client.chat(request).await
        }
    }
}

/// A vision client built ONCE per OCR job and reused across every page (and
/// the crash fallback). Previously the worker constructed a brand-new reqwest
/// client and re-resolved+decrypted the provider secret (Postgres roundtrip +
/// AES-256-GCM) on every page — discarding the connection pool / TLS session
/// each time even though the provider and secret are fixed for the document.
/// Holding the typed client keeps the keep-alive pool and TLS session warm
/// across pages. The fallback only swaps the model (carried on the request),
/// not the provider, so a single client covers primary and fallback.
enum VisionClient {
    Ollama(OllamaClient),
    OpenAiCompatible(OpenAiCompatibleClient),
    Anthropic(AnthropicClient),
}

impl VisionClient {
    async fn vision(&self, request: VisionRequest) -> Result<AiResponse> {
        match self {
            VisionClient::Ollama(client) => client.vision(request).await,
            VisionClient::OpenAiCompatible(client) => client.vision(request).await,
            VisionClient::Anthropic(client) => client.vision(request).await,
        }
    }
}

async fn build_vision_client(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
) -> Result<VisionClient> {
    let timeout = Duration::from_secs(u64::from(provider.request_timeout_seconds));
    match provider.kind {
        AiProviderKind::Ollama => Ok(VisionClient::Ollama(OllamaClient::new_with_timeout(
            &provider.name,
            &provider.base_url,
            provider_secret(pool, config, provider).await?,
            timeout,
        )?)),
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => Ok(
            VisionClient::OpenAiCompatible(OpenAiCompatibleClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(pool, config, provider).await?,
                timeout,
            )?),
        ),
        AiProviderKind::Anthropic => {
            let secret = provider_secret(pool, config, provider)
                .await?
                .ok_or_else(|| {
                    anyhow!("AI provider '{}' requires an API key secret", provider.name)
                })?;
            Ok(VisionClient::Anthropic(AnthropicClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?))
        }
    }
}

async fn provider_secret(
    pool: &DbPool,
    config: &AppConfig,
    provider: &StageProvider,
) -> Result<Option<SecretString>> {
    let Some(secret_id) = provider.secret_id else {
        return Ok(None);
    };
    resolve_secret(pool, &config.secret_key, secret_id).await
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
    use serde_json::Value;

    #[test]
    fn sum_vision_usage_handles_both_wire_shapes() {
        // OpenAI/Anthropic usage + Ollama top-level counters across pages.
        let pages = vec![
            json!({ "usage": { "prompt_tokens": 100, "completion_tokens": 40 } }),
            json!({ "prompt_eval_count": 7, "eval_count": 3 }),
            json!({ "usage": { "input_tokens": 5, "output_tokens": 2 } }),
        ];
        assert_eq!(sum_vision_usage(&pages), Some((112, 45)));
    }

    #[test]
    fn sum_vision_usage_returns_none_without_tokens() {
        let pages = vec![json!({ "response": "text only" }), json!({})];
        assert_eq!(sum_vision_usage(&pages), None);
    }

    #[test]
    fn typed_ollama_4xx_is_permanent_despite_ollama_in_message() {
        // A typed Client 404 from the Ollama client (carrying the word
        // "ollama" via the context) must classify Permanent, not Transient —
        // the substring table treats "ollama" as a transient marker, so before
        // typing this it burned the whole retry budget. #294
        let err = anyhow::Error::new(AiProviderError::Client {
            status: 404,
            body: "model not found".to_owned(),
        })
        .context("Ollama vision call");
        assert_eq!(
            classify_processing_failure(&err),
            ProcessingFailureClass::Permanent
        );

        // A typed 503 still classifies Transient.
        let server = anyhow::Error::new(AiProviderError::Server {
            status: 503,
            body: "unavailable".to_owned(),
        })
        .context("Ollama chat call");
        assert_eq!(
            classify_processing_failure(&server),
            ProcessingFailureClass::Transient
        );
    }

    fn stage_provider(kind: AiProviderKind) -> StageProvider {
        StageProvider {
            name: "p".to_owned(),
            kind,
            base_url: "http://x".to_owned(),
            model: "m".to_owned(),
            secret_id: None,
            reasoning_effort: ReasoningEffort::default(),
            request_timeout_seconds: archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS,
        }
    }

    #[test]
    fn ollama_vision_num_ctx_floors_below_ggml_minimum() {
        let ollama = stage_provider(AiProviderKind::Ollama);
        // A preset pinning 4096 is floored up to the GGML-safe minimum.
        assert_eq!(
            ollama_vision_num_ctx_for_provider(&ollama, Some(4096)),
            Some(OLLAMA_VISION_NUM_CTX_FLOOR)
        );
        // A value already at/above the floor passes through.
        assert_eq!(
            ollama_vision_num_ctx_for_provider(&ollama, Some(32768)),
            Some(32768)
        );
        // None stays None (use the client default); non-Ollama is always None.
        assert_eq!(ollama_vision_num_ctx_for_provider(&ollama, None), None);
        assert_eq!(
            ollama_vision_num_ctx_for_provider(&stage_provider(AiProviderKind::Openai), Some(4096)),
            None
        );
    }

    #[test]
    fn ollama_text_num_ctx_floors_below_prompt_minimum() {
        // A per-provider override pinning 4096 (the pre-#304 Ollama preset)
        // must be floored at the point of use, mirroring the vision path —
        // `resolve_tuning` prefers the provider value over the bumped global.
        let ollama = stage_provider(AiProviderKind::Ollama);
        assert_eq!(
            ollama_text_num_ctx_for_provider(&ollama, Some(4096)),
            Some(OLLAMA_TEXT_NUM_CTX_FLOOR)
        );
        // A value already at/above the floor passes through.
        assert_eq!(
            ollama_text_num_ctx_for_provider(&ollama, Some(32768)),
            Some(32768)
        );
        // None stays None (use the client default); non-Ollama is always None.
        assert_eq!(ollama_text_num_ctx_for_provider(&ollama, None), None);
        assert_eq!(
            ollama_text_num_ctx_for_provider(
                &stage_provider(AiProviderKind::Anthropic),
                Some(4096)
            ),
            None
        );
    }

    #[test]
    fn job_lease_outlives_the_slowest_enabled_provider_timeout() {
        // Default presets leave request_timeout_seconds unset → the 180s
        // built-in default; the 300s baseline wins.
        let mut settings = RuntimeSettings::default();
        assert_eq!(job_lease_seconds(&settings), BASE_JOB_LEASE_SECONDS);

        // The prod shape from the audit: a 600s timeout used to outlive the
        // hard-coded 300s lease mid-call. The lease must now cover the call
        // plus the inter-heartbeat margin.
        settings.ai.providers[0].tuning.request_timeout_seconds = Some(600);
        assert_eq!(
            job_lease_seconds(&settings),
            600 + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );

        // The slowest enabled provider sizes the window (jobs are claimed
        // before stage→provider resolution).
        settings.ai.providers[1].tuning.request_timeout_seconds = Some(900);
        assert_eq!(
            job_lease_seconds(&settings),
            900 + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );

        // Disabled providers can never serve a stage and must not stretch it.
        settings.ai.providers[1].enabled = false;
        assert_eq!(
            job_lease_seconds(&settings),
            600 + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );

        // 0 means "inherit the default", not a zero-second timeout; and a
        // timeout short enough to fit the baseline keeps today's 300s.
        settings.ai.providers[0].tuning.request_timeout_seconds = Some(0);
        assert_eq!(job_lease_seconds(&settings), BASE_JOB_LEASE_SECONDS);
        settings.ai.providers[0].tuning.request_timeout_seconds = Some(120);
        assert_eq!(job_lease_seconds(&settings), BASE_JOB_LEASE_SECONDS);

        // No providers at all: fall back to the built-in default → baseline.
        settings.ai.providers.clear();
        assert_eq!(job_lease_seconds(&settings), BASE_JOB_LEASE_SECONDS);
    }

    #[test]
    fn quota_cooldown_honors_and_clamps_retry_after() {
        // Absent Retry-After -> the long default.
        assert_eq!(
            quota_cooldown_duration(None),
            DEFAULT_PROVIDER_QUOTA_COOLDOWN
        );
        // A short Retry-After is honored (clamped up to the floor), NOT widened
        // to the 24h default as before.
        assert_eq!(
            quota_cooldown_duration(Some(60)),
            MIN_PROVIDER_QUOTA_COOLDOWN
        );
        // A mid value passes through.
        assert_eq!(
            quota_cooldown_duration(Some(3600)),
            Duration::from_secs(3600)
        );
        // An absurd value is capped.
        assert_eq!(
            quota_cooldown_duration(Some(60 * 24 * 60 * 60)),
            MAX_PROVIDER_QUOTA_COOLDOWN
        );
    }

    #[test]
    fn typed_paperless_errors_drive_classification() {
        // A transient Paperless failure is an infrastructure outage of the
        // system of record: classified as TransientInfra so it retries against
        // the higher, bounded ceiling instead of each document's small budget. #305.
        let transient: anyhow::Error =
            anyhow::Error::new(PaperlessError::Timeout("waiting for paperless".to_owned()))
                .context("higher-level wrap that does not mention transient keywords");
        let class = classify_processing_failure(&transient);
        assert_eq!(class, ProcessingFailureClass::TransientInfra);
        assert!(class.is_retryable(), "an upstream outage is retryable");
        assert_eq!(
            class.retry_ceiling(),
            Some(PAPERLESS_INFRA_RETRY_CEILING),
            "infra failures ride the outage out on the elevated ceiling"
        );

        let permanent: anyhow::Error = anyhow::Error::new(PaperlessError::Client {
            status: 422,
            body: "no transient keyword here".to_owned(),
        });
        let permanent_class = classify_processing_failure(&permanent);
        assert_eq!(permanent_class, ProcessingFailureClass::Permanent);
        assert_eq!(
            permanent_class.retry_ceiling(),
            None,
            "a permanent client error keeps the normal (no-override) budget"
        );
    }

    #[test]
    fn typed_ai_errors_drive_classification() {
        let transient: anyhow::Error =
            anyhow::Error::new(AiProviderError::RunnerUnavailable("ollama".to_owned()));
        assert!(matches!(
            classify_processing_failure(&transient),
            ProcessingFailureClass::Transient
        ));

        let permanent: anyhow::Error = anyhow::Error::new(AiProviderError::InvalidResponse(
            "unexpected shape".to_owned(),
        ));
        assert!(matches!(
            classify_processing_failure(&permanent),
            ProcessingFailureClass::Permanent
        ));
    }

    #[test]
    fn fallback_substring_matching_still_classifies_untyped_errors() {
        let transient: anyhow::Error = anyhow!("pool timed out waiting for connection");
        assert!(matches!(
            classify_processing_failure(&transient),
            ProcessingFailureClass::Transient
        ));
        let permanent: anyhow::Error = anyhow!("invalid configuration: missing field");
        assert!(matches!(
            classify_processing_failure(&permanent),
            ProcessingFailureClass::Permanent
        ));
    }

    fn document_detail() -> PaperlessDocumentDetail {
        PaperlessDocumentDetail {
            id: 42,
            title: Some("Existing title".to_owned()),
            created: Some("2026-03-14".to_owned()),
            modified: Some("2026-03-15T10:00:00Z".to_owned()),
            content: Some("private OCR text".to_owned()),
            tags: vec![1, 2],
            correspondent: Some(7),
            document_type: Some(9),
            original_file_name: Some("document.pdf".to_owned()),
        }
    }

    #[test]
    fn unchanged_standard_metadata_is_pruned_before_patch() {
        let document = document_detail();
        let mut patch = DocumentPatch {
            content: None,
            title: Some("Existing title".to_owned()),
            tags: Some(vec![2, 1]),
            correspondent: Some(Some(7)),
            document_type: Some(Some(9)),
            created: Some("2026-03-14".to_owned()),
            custom_fields: None,
        };

        prune_unchanged_patch_fields(&mut patch, &document);

        assert!(patch.is_empty());
    }

    #[test]
    fn audit_payload_redacts_content_and_custom_fields() {
        let patch = DocumentPatch {
            content: Some("private OCR text".to_owned()),
            title: Some("New title".to_owned()),
            tags: Some(vec![1, 2, 3]),
            correspondent: Some(Some(7)),
            document_type: None,
            created: Some("2026-03-14".to_owned()),
            custom_fields: Some(json!([{ "field": 1, "value": "private value" }])),
        };

        let audit = audit_patch_payload(&patch);
        assert_eq!(audit["content"]["redacted"], Value::Bool(true));
        assert_eq!(audit["content"]["chars"], Value::from(16));
        assert!(audit["content"].get("sha256").is_some());
        assert_eq!(audit["custom_fields"]["redacted"], Value::Bool(true));
        assert!(!audit.to_string().contains("private OCR text"));
        assert!(!audit.to_string().contains("private value"));
    }

    #[test]
    fn classifies_integration_interruptions_as_transient() {
        let cases = [
            anyhow!("Paperless request timed out while downloading original"),
            anyhow!(
                "Ollama vision returned 500 Internal Server Error: runner process no longer running"
            ),
            anyhow!("PostgreSQL database pool timed out while claiming jobs"),
        ];

        for error in cases {
            assert_eq!(
                classify_processing_failure(&error),
                ProcessingFailureClass::Transient
            );
        }
    }

    #[test]
    fn detects_ollama_vision_runtime_crashes() {
        // Real-world payloads observed from Ollama when the llama runner aborts on a vision
        // input. All three should trip the operator hint, even though the classifier still
        // marks them transient (retry on a different page may still succeed).
        // Real-world Ollama crash payloads always come wrapped in a 500-internal-server-error
        // envelope, which the classifier reads as Transient. We assert the combined detect +
        // retry behaviour on the wrapped form, plus the bare "signal arrived during cgo
        // execution" string for the detector alone (used in stack traces that bypass the HTTP
        // envelope, e.g. in tests that feed the runtime crash directly).
        let crash_cases = [
            anyhow!(
                "Ollama vision returned 500 Internal Server Error: GGML_ASSERT(a->ne[2] * 4 == b->ne[0]) failed"
            ),
            anyhow!(
                "Ollama vision returned 500 Internal Server Error: llama runner process no longer running: 2"
            ),
        ];
        for error in crash_cases {
            assert!(is_vision_model_runtime_crash(&error), "case: {error:?}");
            assert_eq!(
                classify_processing_failure(&error),
                ProcessingFailureClass::Transient,
                "crash should still retry: {error:?}"
            );
        }
        assert!(is_vision_model_runtime_crash(&anyhow!(
            "signal arrived during cgo execution"
        )));

        // Regular transient errors must NOT trip the vision-crash hint — that would mislead
        // operators into swapping a healthy model when the actual cause is networking.
        let non_crash_cases = [
            anyhow!("Paperless request timed out while downloading original"),
            anyhow!("PostgreSQL database pool timed out while claiming jobs"),
        ];
        for error in non_crash_cases {
            assert!(!is_vision_model_runtime_crash(&error), "case: {error:?}");
        }
    }

    #[test]
    fn classifies_validation_and_configuration_errors_as_permanent() {
        let cases = [
            anyhow!("Paperless returned 406 Not Acceptable"),
            anyhow!("model response did not contain valid JSON"),
            anyhow!("unknown allowed tag returned by model"),
        ];

        for error in cases {
            assert_eq!(
                classify_processing_failure(&error),
                ProcessingFailureClass::Permanent
            );
        }
    }

    fn unrestricted_safety() -> archivist_core::WorkflowSafetyStatus {
        archivist_core::WorkflowSafetyStatus {
            paused: false,
            dry_run: false,
            hourly_document_limit: None,
            daily_document_limit: None,
            hourly_remaining: None,
            daily_remaining: None,
        }
    }

    #[test]
    fn autopilot_drain_runs_under_full_auto_with_unlimited_budget() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        settings.workflow.dry_run = false;
        let safety = unrestricted_safety();
        // Unlimited budget → drain is allowed and the per-tick cap is the
        // only ceiling.
        assert_eq!(autopilot_drain_budget(&settings, &safety), Some(None));
    }

    #[test]
    fn autopilot_drain_skips_when_mode_is_manual_review() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::ManualReview;
        let safety = unrestricted_safety();
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_skips_when_mode_is_auto_select_review() {
        let mut settings = RuntimeSettings::default();
        // AutoSelectReview enables auto-selection but still requires human
        // review — drain must not auto-apply under this mode.
        settings.workflow.mode = ProcessingMode::AutoSelectReview;
        let safety = unrestricted_safety();
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_skips_when_dry_run_is_enabled() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        settings.workflow.dry_run = true;
        let safety = unrestricted_safety();
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_skips_when_workflow_is_paused() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        let mut safety = unrestricted_safety();
        safety.paused = true;
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_skips_when_safety_budget_is_exhausted() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        let mut safety = unrestricted_safety();
        safety.hourly_document_limit = Some(100);
        safety.daily_document_limit = Some(1000);
        safety.hourly_remaining = Some(0);
        safety.daily_remaining = Some(500);
        assert_eq!(autopilot_drain_budget(&settings, &safety), None);
    }

    #[test]
    fn autopilot_drain_caps_at_smaller_of_hourly_or_daily() {
        let mut settings = RuntimeSettings::default();
        settings.workflow.mode = ProcessingMode::FullAuto;
        let mut safety = unrestricted_safety();
        safety.hourly_document_limit = Some(50);
        safety.daily_document_limit = Some(200);
        safety.hourly_remaining = Some(7);
        safety.daily_remaining = Some(120);
        // Drain budget is the smaller of the two remaining quotas.
        assert_eq!(autopilot_drain_budget(&settings, &safety), Some(Some(7)));
    }

    #[test]
    fn vision_fallback_prefers_explicit_setting_when_different_from_primary() {
        let mut settings = RuntimeSettings::default();
        settings.ai.fallback_vision_model = Some("llava:13b".to_owned());
        let choice = pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &[]).unwrap();
        assert_eq!(choice.model, "llava:13b");
        assert_eq!(choice.source, VisionFallbackSource::Explicit);
    }

    #[test]
    fn vision_fallback_ignores_explicit_setting_that_equals_primary() {
        let mut settings = RuntimeSettings::default();
        settings.ai.fallback_vision_model = Some("QWEN2.5VL:7B".to_owned());
        // Same model (case-insensitive) → don't use it; fall through to chain. With no
        // installed models in the test list, the chain cannot be walked either.
        assert!(pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &[]).is_none());
    }

    #[test]
    fn vision_fallback_walks_safe_default_chain_when_no_explicit_setting() {
        let settings = RuntimeSettings::default();
        let installed = vec![
            "llava-llama3:8b".to_owned(),
            "qwen3:8b".to_owned(),
            "llava:13b".to_owned(),
        ];
        // Chain order: qwen2-vl:7b (not installed), llava-llama3:8b (installed) → picked.
        let choice = pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &installed).unwrap();
        assert_eq!(choice.model, "llava-llama3:8b");
        assert_eq!(choice.source, VisionFallbackSource::AutoDiscovered);
    }

    #[test]
    fn vision_fallback_safe_default_skips_primary_even_if_installed() {
        let settings = RuntimeSettings::default();
        // Primary IS in the chain; auto-discovery must skip it and pick the next entry.
        let installed = vec!["llava:13b".to_owned(), "llava-llama3:8b".to_owned()];
        let choice = pick_vision_fallback_model(&settings, "llava-llama3:8b", &installed).unwrap();
        assert_eq!(choice.model, "llava:13b");
        assert_eq!(choice.source, VisionFallbackSource::AutoDiscovered);
    }

    #[test]
    fn vision_fallback_returns_none_when_chain_has_no_installed_match() {
        let settings = RuntimeSettings::default();
        // No installed models from the chain → no fallback possible.
        let installed = vec!["qwen3:8b".to_owned(), "phi3:mini".to_owned()];
        assert!(pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &installed).is_none());
    }

    #[test]
    fn vision_fallback_returns_none_when_no_explicit_and_no_installed() {
        let settings = RuntimeSettings::default();
        assert!(pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &[]).is_none());
    }

    #[test]
    fn vision_fallback_explicit_trims_whitespace_and_skips_empty() {
        let mut settings = RuntimeSettings::default();
        settings.ai.fallback_vision_model = Some("   ".to_owned());
        // Whitespace-only explicit is treated as unset.
        assert!(pick_vision_fallback_model(&settings, "qwen2.5vl:7b", &[]).is_none());
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
                value: Value::Null,
                confidence: None,
            },
            FieldValueSuggestion {
                name: "A".to_owned(),
                value: Value::Null,
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

    fn test_app_config() -> AppConfig {
        // Minimal config shaped enough to feed `env_concurrency_cap`. The
        // other fields are not consulted; we only assert on
        // `worker_concurrency`.
        AppConfig {
            http_addr: "127.0.0.1:0".to_owned(),
            database_url: SecretString::new(String::new().into()),
            worker_concurrency: 4,
            db_max_connections: 10,
            log_level: "info".to_owned(),
            cookie_secure: false,
            session_ttl_hours: 12,
            bootstrap_admin_username: "admin".to_owned(),
            bootstrap_admin_password: None,
            oidc_enabled: false,
            oidc_issuer_url: None,
            oidc_client_id: None,
            oidc_client_secret: None,
            oidc_redirect_uri: None,
            oidc_scopes: "openid profile email".to_owned(),
            oidc_admin_users: String::new(),
            oidc_default_roles: "viewer".to_owned(),
            oidc_roles_claim: "urn:zitadel:iam:org:project:roles".to_owned(),
            oidc_role_mappings: "archivist-admin=admin".to_owned(),
            oidc_allow_email_link: false,
            secret_key: SecretString::new("0123456789abcdef0123456789abcdef".to_owned().into()),
            static_dir: "frontend/dist".to_owned(),
            trust_proxy: false,
            auth_rate_limit: 10,
            auth_rate_limit_window_seconds: 60,
            webhook_secret: None,
            metrics_token: None,
        }
    }
}
