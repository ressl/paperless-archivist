//! One-shot startup repairs (#443) run before the worker loop starts.

use std::future::Future;

use anyhow::Result;
use archivist_db::{
    DbPool, StartupRepair, get_runtime_settings, record_startup_repair,
    requeue_vision_crashed_jobs, startup_repair_applied,
};
use serde_json::json;
use tracing::{info, warn};

/// #443: run the one-shot startup repair `repair` unless its (name, version)
/// marker already exists, and record the marker with the repair's summary
/// once it succeeded. `run` returns `None` when the repair was skipped (e.g.
/// disabled by a runtime setting) so it is retried on a later boot. Errors
/// are logged and swallowed: the worker should still come up even if this
/// housekeeping fails, and a failed repair records no marker.
pub(crate) async fn run_startup_repair<F, Fut>(pool: &DbPool, repair: StartupRepair, run: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Option<serde_json::Value>>>,
{
    match startup_repair_applied(pool, repair).await {
        Ok(true) => {
            info!(
                repair = repair.name,
                version = repair.version,
                "startup repair already applied; skipping"
            );
            return;
        }
        Ok(false) => {}
        Err(error) => {
            warn!(repair = repair.name, error = %error, "startup repair marker lookup failed");
            return;
        }
    }
    match run().await {
        Ok(Some(details)) => {
            match record_startup_repair(pool, repair, env!("CARGO_PKG_VERSION"), details).await {
                Ok(_) => info!(
                    repair = repair.name,
                    version = repair.version,
                    "startup repair applied and recorded"
                ),
                Err(error) => {
                    warn!(repair = repair.name, error = %error, "failed to record startup repair")
                }
            }
        }
        Ok(None) => {}
        Err(error) => warn!(repair = repair.name, error = %error, "startup repair failed"),
    }
}

/// One-shot startup helper: when enabled in runtime settings, lifts `failed` OCR jobs that
/// match the vision-runtime-crash signature back into `queued` and bumps their attempt
/// budget by one (at most once per job, #406). Gated by [`run_startup_repair`] (#443);
/// returns `None` while the setting disables it so no marker is recorded.
pub(crate) async fn run_startup_vision_crash_requeue(
    pool: &DbPool,
) -> Result<Option<serde_json::Value>> {
    let settings = get_runtime_settings(pool).await?;
    if !settings.ai.requeue_vision_crashes_on_startup {
        info!(
            "vision-crash startup requeue disabled by setting requeue_vision_crashes_on_startup=false"
        );
        return Ok(None);
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
    Ok(Some(json!({ "jobs_requeued": summary.jobs_requeued })))
}
