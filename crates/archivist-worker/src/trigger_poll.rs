//! Paperless trigger-tag polling and the auto-selector.

use std::collections::HashMap;

use anyhow::Result;
use archivist_config::AppConfig;
use archivist_core::AuditEventInput;
use archivist_db::{
    DbPool, append_audit, create_run_with_jobs_with_priority, get_runtime_settings,
    get_workflow_safety_status, increment_metric_counter, queue_missing_pipeline,
    selector_document_budget,
};
use archivist_paperless::PaperlessTag;
use chrono::{DateTime, Utc};
use serde_json::json;
use tracing::info;

use crate::paperless::paperless_client;
use crate::sync::sync_metadata;

/// #400 poller guard: skip a trigger-tagged document whose most recent run
/// is terminal and finished at or after the document's last Paperless
/// modification — the trigger simply survived that run (dry-run, rejected
/// review, tag removal failed). Re-adding the trigger tag bumps `modified`
/// and therefore queues exactly one new run. Without a `modified` timestamp
/// the old behaviour (queue) is kept.
fn trigger_already_handled(
    latest_terminal_run_finished_at: Option<DateTime<Utc>>,
    document_modified_at: Option<DateTime<Utc>>,
) -> bool {
    matches!(
        (latest_terminal_run_finished_at, document_modified_at),
        (Some(finished), Some(modified)) if finished >= modified
    )
}

pub(crate) async fn poll_paperless_triggers(pool: &DbPool, config: &AppConfig) -> Result<()> {
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
    // #400: one batched lookup of the latest terminal run per triggered doc.
    let triggered_ids: Vec<i32> = snapshot
        .documents
        .iter()
        .filter(|document| {
            let names = document
                .tags
                .iter()
                .filter_map(|id| tags_by_id.get(id))
                .map(|tag| tag.name.clone())
                .collect::<Vec<_>>();
            !settings
                .workflow
                .tags
                .stages_requested_by_tags(&names)
                .is_empty()
        })
        .map(|document| document.id)
        .collect();
    let latest_terminal_runs =
        archivist_db::latest_terminal_run_finished_at(pool, &triggered_ids).await?;
    let mut trigger_skipped_unchanged = 0_u64;
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
            if trigger_already_handled(
                latest_terminal_runs.get(&document.id).copied(),
                archivist_db::parse_paperless_modified_at(document.modified.as_deref()),
            ) {
                trigger_skipped_unchanged += 1;
                continue;
            }
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
        trigger_skipped_unchanged, "trigger polling inspected Paperless documents"
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    #[test]
    fn trigger_poll_skips_documents_unchanged_since_their_terminal_run() {
        let finished = Utc::now();
        let before = finished - ChronoDuration::seconds(30);
        let after = finished + ChronoDuration::seconds(30);
        // Unchanged since the run ended: exactly one run per trigger.
        assert!(trigger_already_handled(Some(finished), Some(before)));
        assert!(trigger_already_handled(Some(finished), Some(finished)));
        // Operator re-added the trigger (modified moved on): queue again.
        assert!(!trigger_already_handled(Some(finished), Some(after)));
        // No terminal run (never processed / still active) or unknown modified.
        assert!(!trigger_already_handled(None, Some(before)));
        assert!(!trigger_already_handled(Some(finished), None));
    }
}
