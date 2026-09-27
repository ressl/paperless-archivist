//! Autopilot review drain: auto-applies pending review items under full_auto.

use std::time::Duration;

use anyhow::{Result, anyhow};
use archivist_apply::{
    ApplyExecution, ApplyRequest, ReviewApplyConflict, ReviewApplyPrecondition,
    ReviewTagOperations, apply_document,
};
use archivist_config::AppConfig;
use archivist_core::{DocumentPatch, RuntimeSettings};
use archivist_db::{
    DbPool, ReviewItemRecord, claim_pending_review_for_autopilot_drain, get_runtime_settings,
    get_workflow_safety_status, is_last_active_job, list_pending_review_items_for_autopilot_drain,
    mark_review_apply_conflict, mark_review_auto_applied, revert_review_from_applying,
    selector_document_budget,
};
use archivist_paperless::PaperlessClient;
use serde_json::json;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::apply::audit_custom_fields_dropped;
use crate::paperless::paperless_client;

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
pub(crate) async fn drain_pending_reviews_if_autopilot_tick(
    pool: &DbPool,
    config: &AppConfig,
) -> Result<()> {
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
pub(crate) async fn ensure_tag_cached(
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
    let execution = match patch_result {
        Ok(execution) => execution,
        Err(error) => {
            if let Some(conflict) = error.downcast_ref::<ReviewApplyConflict>() {
                mark_review_apply_conflict(
                    pool,
                    claimed.id,
                    "pending",
                    conflict.fields(),
                    "worker",
                    Some("autopilot-drain".to_owned()),
                )
                .await?;
                return Err(error);
            }
            // Revert only when no prepared/in-flight/confirmed intent exists.
            // An ambiguous timeout must remain fenced until recovery performs
            // GET reconciliation; reverting it would permit a duplicate PATCH.
            match archivist_db::review_has_nonterminal_apply_intent(pool, claimed.id).await {
                Ok(false) => {
                    if let Err(revert_error) =
                        revert_review_from_applying(pool, claimed.id, "pending").await
                    {
                        warn!(
                            review_id = %claimed.id,
                            error = %revert_error,
                            "failed to revert review item after terminal drain failure"
                        );
                    }
                }
                Ok(true) => warn!(
                    review_id = %claimed.id,
                    "autopilot review remains fenced while its apply intent is recovered"
                ),
                Err(lookup_error) => warn!(
                    review_id = %claimed.id,
                    error = %lookup_error,
                    "could not prove autopilot review safe to revert; leaving it fenced"
                ),
            }
            return Err(error);
        }
    };
    mark_review_auto_applied(pool, claimed.id).await?;
    archivist_db::finalize_apply_intent(pool, execution.attempt_id()).await?;
    Ok(true)
}

async fn apply_autopilot_drain_patch(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    review: &ReviewItemRecord,
    tag_cache: &mut Vec<archivist_paperless::PaperlessTag>,
    completion_full: &archivist_paperless::PaperlessTag,
) -> Result<ApplyExecution> {
    let patch_value = review
        .edited_patch
        .clone()
        .unwrap_or_else(|| review.suggested_patch.clone());
    let pending_new_objects =
        archivist_apply::PendingNewObjects::from_patch_value(&patch_value, &settings.workflow.tags);
    let mut patch: DocumentPatch = serde_json::from_value(patch_value)?;
    // #404: objects proposed by the model are created only now, at apply.
    let new_tag_ids = archivist_apply::materialize_pending_new_objects(
        paperless,
        &pending_new_objects,
        archivist_apply::NewObjectPolicy::from_settings(settings),
        &mut patch,
    )
    .await?;
    // run_id is None only for review items whose run was pruned by retention;
    // those never reach the drain (retention deletes terminal runs only, and
    // a pending review keeps its run in 'waiting_review').
    let final_run_stage = if let (Some(run_id), Some(job_id)) = (review.run_id, review.job_id) {
        is_last_active_job(pool, run_id, job_id).await?
    } else {
        false
    };
    let mut additions = Vec::new();
    let mut removals = Vec::new();
    if let Some(completion_name) = settings
        .workflow
        .tags
        .completion_tag_for_stage(review.stage)
    {
        let tag = ensure_tag_cached(paperless, tag_cache, completion_name).await?;
        additions.push(tag.id);
    }
    if final_run_stage {
        additions.push(completion_full.id);
    }
    additions.extend(new_tag_ids);
    for trigger_name in settings
        .workflow
        .tags
        .trigger_tags_requesting_stage(review.stage)
    {
        if let Some(tag) = tag_cache
            .iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(trigger_name))
        {
            removals.push(tag.id);
        }
    }
    if final_run_stage
        && let Some(tag) = tag_cache.iter().find(|tag| {
            tag.name
                .eq_ignore_ascii_case(&settings.workflow.tags.trigger_process)
        })
    {
        removals.push(tag.id);
    }
    additions.sort_unstable();
    additions.dedup();
    removals.sort_unstable();
    removals.dedup();
    let execution = apply_document(
        pool,
        paperless,
        ApplyRequest {
            source: "autopilot_drain".to_owned(),
            source_key: format!("review:{}", review.id),
            owner_type: "worker".to_owned(),
            owner_id: "autopilot-drain".to_owned(),
            paperless_document_id: review.paperless_document_id,
            run_id: review.run_id,
            job_id: review.job_id,
            review_id: Some(review.id),
            patch,
            before: None,
            metadata: json!({
                "stage": review.stage,
                "review_id": review.id,
                "trigger": "autopilot_drain"
            }),
            review_revert_status: Some("pending".to_owned()),
            review_precondition: Some(ReviewApplyPrecondition {
                baseline: review.baseline.clone(),
                tag_operations: ReviewTagOperations {
                    additions,
                    removals,
                },
            }),
            // A review apply must have exactly one preconditioned PATCH body.
            // Retrying a reduced body after a 400 would create a second write
            // without another field-level concurrency decision.
            allow_custom_fields_fallback: false,
        },
    )
    .await?;
    if execution.custom_fields_dropped() {
        audit_custom_fields_dropped(
            pool,
            review.paperless_document_id,
            review.run_id,
            review.job_id,
            json!({
                "stage": review.stage,
                "review_id": review.id,
                "trigger": "autopilot_drain"
            }),
        )
        .await?;
    }
    Ok(execution)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archivist_core::ProcessingMode;

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
}
