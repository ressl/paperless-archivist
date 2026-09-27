//! Paperless patch application: workflow tags, trigger retirement, tag strategies
//! and the audit payloads that accompany a patch.

use anyhow::Result;
use archivist_apply::{
    ApplyExecution, ApplyRequest, apply_document, resume_apply_source, review_apply_baseline,
};
use archivist_core::{AuditEventInput, DocumentPatch, OldTagStrategy, RuntimeSettings, Stage};
use archivist_db::{
    DbPool, JobRecord, append_audit, complete_job, create_review_item, is_last_active_job,
};
use archivist_paperless::{PaperlessClient, PaperlessDocumentDetail};
use serde_json::json;
use tracing::{info, warn};
use uuid::Uuid;

use crate::drain::ensure_tag_cached;
use crate::hash_text;

/// Tag set a validated suggestion produces under the configured
/// `old_tag_strategy` (#411). `protected` holds ids that no strategy may
/// remove (workflow tags, include/exclude rule tags, ids unknown to the local
/// mirror); `ai_managed` holds ids Archivist added in earlier applies.
///
/// * `keep_existing`      — current ∪ selected
/// * `replace_ai_managed` — (current − unprotected AI-managed) ∪ selected
/// * `remove_all_business`— (current ∩ protected) ∪ selected
fn merge_tags_for_strategy(
    strategy: &OldTagStrategy,
    current: &[i32],
    selected: &[i32],
    protected: &std::collections::HashSet<i32>,
    ai_managed: &std::collections::HashSet<i32>,
) -> Vec<i32> {
    let mut tags: Vec<i32> = current
        .iter()
        .copied()
        .filter(|id| match strategy {
            OldTagStrategy::KeepExisting => true,
            OldTagStrategy::ReplaceAiManaged => protected.contains(id) || !ai_managed.contains(id),
            OldTagStrategy::RemoveAllBusiness => protected.contains(id),
        })
        .collect();
    tags.extend(selected.iter().copied());
    tags.sort_unstable();
    tags.dedup();
    tags
}

/// Look up what `merge_tags_for_strategy` needs for this document and merge.
/// `keep_existing` needs no lookups.
pub(crate) async fn tags_for_old_tag_strategy(
    pool: &DbPool,
    settings: &RuntimeSettings,
    document: &PaperlessDocumentDetail,
    selected: &[i32],
) -> Result<Vec<i32>> {
    let strategy = &settings.tagging.old_tag_strategy;
    if matches!(strategy, OldTagStrategy::KeepExisting) {
        return Ok(merge_tags_for_strategy(
            strategy,
            &document.tags,
            selected,
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        ));
    }
    let catalog = archivist_db::tag_catalog_entries_for_ids(pool, &document.tags).await?;
    let rules = &settings.workflow.rules;
    let is_rule_tag = |name: &str| {
        rules
            .include_tags
            .iter()
            .chain(rules.exclude_tags.iter())
            .any(|rule| rule.trim().eq_ignore_ascii_case(name.trim()))
    };
    let protected: std::collections::HashSet<i32> = document
        .tags
        .iter()
        .copied()
        .filter(|id| {
            catalog
                .iter()
                .find(|(tag_id, _, _)| tag_id == id)
                .is_none_or(|(_, name, is_workflow)| {
                    *is_workflow
                        || settings.workflow.tags.is_workflow_tag(name)
                        || is_rule_tag(name)
                })
        })
        .collect();
    let ai_managed: std::collections::HashSet<i32> =
        if matches!(strategy, OldTagStrategy::ReplaceAiManaged) {
            archivist_db::ai_managed_tag_ids_for_document(pool, document.id)
                .await?
                .into_iter()
                .collect()
        } else {
            std::collections::HashSet::new()
        };
    Ok(merge_tags_for_strategy(
        strategy,
        &document.tags,
        selected,
        &protected,
        &ai_managed,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_patch_result(
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
        let document = paperless.get_document(job.paperless_document_id).await?;
        let baseline = review_apply_baseline(&document);
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
        let Some(review_id) = create_review_item(
            pool,
            job,
            review_patch,
            json!(review_warnings),
            baseline,
            lease_owner,
        )
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
    let execution = apply_patch_with_workflow_tags(
        pool,
        paperless,
        settings,
        job,
        patch,
        final_run_stage,
        lease_owner,
    )
    .await?;
    if complete_job(
        pool,
        job,
        lease_owner,
        json!({ "applied": true, "warnings": warnings }),
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
}

/// Which workflow tags a terminal outcome that wrote no business patch must
/// remove and add (#400). A permanent failure aborts the whole run, so every
/// trigger goes and the failure markers are set; a skip retires only the
/// triggers of this stage, plus `trigger_process` when it was the run's last
/// active stage (mirroring the success path).
fn terminal_trigger_tag_plan(
    tags: &archivist_core::WorkflowTags,
    stage: Stage,
    final_run_stage: bool,
    failed: bool,
) -> (Vec<&str>, Vec<&str>) {
    if failed {
        return (
            tags.all_trigger_tags(),
            vec![tags.failed.as_str(), tags.failed_tag_for_stage(stage)],
        );
    }
    let mut removals = tags.trigger_tags_requesting_stage(stage);
    if final_run_stage {
        removals.push(tags.trigger_process.as_str());
    }
    (removals, Vec::new())
}

/// #400: a job that ends without applying a patch (skip, omission, all fields
/// invalid, permanent failure) must still retire its trigger tags, otherwise
/// the trigger poll re-creates a run for the same unchanged document every
/// minute. Best-effort: errors are logged, and the poller additionally skips
/// documents whose latest run is terminal and newer than their Paperless
/// `modified` timestamp. In dry-run nothing is written to Paperless; the
/// poller guard alone prevents the loop there.
pub(crate) async fn retire_trigger_tags_after_terminal_outcome(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    failed: bool,
) {
    if settings.workflow.dry_run {
        info!(
            document_id = job.paperless_document_id,
            "dry-run: leaving trigger tags in Paperless after terminal outcome"
        );
        return;
    }
    let result: Result<()> = async {
        let final_run_stage = failed || is_last_active_job(pool, job.run_id, job.id).await?;
        let (remove_names, add_names) =
            terminal_trigger_tag_plan(&settings.workflow.tags, job.stage, final_run_stage, failed);
        let mut catalog = paperless.list_tags().await?;
        let removals: Vec<i32> = catalog
            .iter()
            .filter(|tag| {
                remove_names
                    .iter()
                    .any(|name| tag.name.eq_ignore_ascii_case(name))
            })
            .map(|tag| tag.id)
            .collect();
        let mut additions = Vec::new();
        for name in add_names {
            additions.push(ensure_tag_cached(paperless, &mut catalog, name).await?.id);
        }
        let document = paperless.get_document(job.paperless_document_id).await?;
        let needs_change = document.tags.iter().any(|id| removals.contains(id))
            || additions.iter().any(|id| !document.tags.contains(id));
        if needs_change {
            paperless
                .add_and_remove_tags(job.paperless_document_id, &additions, &removals)
                .await?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        warn!(
            document_id = job.paperless_document_id,
            error = %error,
            failed,
            "could not retire trigger tags after terminal outcome; poller guard prevents a requeue loop"
        );
    }
}

/// Mirror of `fail_job`'s retry decision so the caller knows whether the
/// failure it just recorded was terminal (#400).
pub(crate) fn failure_is_terminal(
    job: &JobRecord,
    retryable: bool,
    retry_ceiling: Option<i32>,
) -> bool {
    let ceiling = retry_ceiling
        .map(|ceiling| ceiling.max(job.max_attempts))
        .unwrap_or(job.max_attempts);
    !(retryable && job.attempts < ceiling)
}

pub(crate) async fn apply_patch_with_workflow_tags(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    mut patch: DocumentPatch,
    final_run_stage: bool,
    lease_owner: &str,
) -> Result<ApplyExecution> {
    let source_key = format!("job:{}", job.id);
    if let Some(execution) = resume_apply_source(pool, paperless, &source_key).await? {
        return Ok(execution);
    }
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
    // #400: retire every trigger that requested this stage (incl. the legacy
    // per-field metadata triggers), not just the stage's primary trigger.
    for trigger_name in settings
        .workflow
        .tags
        .trigger_tags_requesting_stage(job.stage)
        .into_iter()
        .chain(final_run_stage.then_some(settings.workflow.tags.trigger_process.as_str()))
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
    let execution = apply_document(
        pool,
        paperless,
        ApplyRequest {
            source: "worker_auto".to_owned(),
            source_key,
            owner_type: "worker".to_owned(),
            owner_id: lease_owner.to_owned(),
            paperless_document_id: job.paperless_document_id,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            review_id: None,
            patch,
            before: Some(before_value),
            metadata: json!({ "stage": job.stage }),
            review_revert_status: None,
            review_precondition: None,
            allow_custom_fields_fallback: true,
        },
    )
    .await?;
    if execution.custom_fields_dropped() {
        audit_custom_fields_dropped(
            pool,
            job.paperless_document_id,
            Some(job.run_id),
            Some(job.id),
            json!({ "stage": job.stage }),
        )
        .await?;
    }
    Ok(execution)
}

/// Keep the existing operator-visible marker when the shared durable executor
/// had to prepare a second, reduced intent after Paperless rejected custom
/// fields. The failed and successful HTTP attempts themselves are audited by
/// the intent state transitions.
pub(crate) async fn audit_custom_fields_dropped(
    pool: &DbPool,
    document_id: i32,
    run_id: Option<Uuid>,
    job_id: Option<Uuid>,
    extra_metadata: serde_json::Value,
) -> Result<()> {
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
            error_message: None,
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

#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    use crate::test_support::vision_test_job;

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
            custom_fields: serde_json::Value::Null,
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

    // ---- #411: old_tag_strategy semantics ----
    // Fixture: 1 = user tag "Inbox" (protected by an include/exclude rule),
    // 2 = workflow tag "archivist-ocr", 3 = earlier AI-added business tag,
    // 4 = user-added business tag, 5 = newly selected AI tag.
    fn strategy_fixture() -> (Vec<i32>, Vec<i32>, HashSetI32, HashSetI32) {
        (
            vec![1, 2, 3, 4],
            vec![5],
            [1, 2].into_iter().collect(),
            [3].into_iter().collect(),
        )
    }
    type HashSetI32 = std::collections::HashSet<i32>;

    #[test]
    fn keep_existing_strategy_only_adds() {
        let (current, selected, protected, ai) = strategy_fixture();
        assert_eq!(
            merge_tags_for_strategy(
                &OldTagStrategy::KeepExisting,
                &current,
                &selected,
                &protected,
                &ai
            ),
            vec![1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn replace_ai_managed_strategy_drops_only_earlier_ai_tags() {
        let (current, selected, protected, ai) = strategy_fixture();
        assert_eq!(
            merge_tags_for_strategy(
                &OldTagStrategy::ReplaceAiManaged,
                &current,
                &selected,
                &protected,
                &ai
            ),
            vec![1, 2, 4, 5]
        );
        // A protected tag is kept even if Archivist once added it.
        let ai_including_workflow: HashSetI32 = [2, 3].into_iter().collect();
        assert_eq!(
            merge_tags_for_strategy(
                &OldTagStrategy::ReplaceAiManaged,
                &current,
                &selected,
                &protected,
                &ai_including_workflow
            ),
            vec![1, 2, 4, 5]
        );
        // Re-selecting an AI tag keeps it.
        assert_eq!(
            merge_tags_for_strategy(
                &OldTagStrategy::ReplaceAiManaged,
                &current,
                &[3],
                &protected,
                &ai
            ),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn remove_all_business_strategy_keeps_workflow_and_rule_tags() {
        let (current, selected, protected, ai) = strategy_fixture();
        assert_eq!(
            merge_tags_for_strategy(
                &OldTagStrategy::RemoveAllBusiness,
                &current,
                &selected,
                &protected,
                &ai
            ),
            vec![1, 2, 5]
        );
    }

    // ---- #400: terminal outcomes retire trigger tags ----

    #[test]
    fn terminal_skip_retires_stage_triggers_and_process_only_when_final() {
        let tags = archivist_core::WorkflowTags::default();
        let (remove, add) = terminal_trigger_tag_plan(&tags, Stage::Metadata, false, false);
        assert!(add.is_empty());
        assert!(remove.contains(&"ai-title"));
        assert!(!remove.contains(&"ai-process"));
        let (remove, _) = terminal_trigger_tag_plan(&tags, Stage::Metadata, true, false);
        assert!(remove.contains(&"ai-process"));
        assert!(remove.contains(&"ai-tags"));
        assert!(!remove.contains(&"ai-ocr"));
    }

    #[test]
    fn terminal_failure_retires_every_trigger_and_marks_failure() {
        let tags = archivist_core::WorkflowTags::default();
        let (remove, add) = terminal_trigger_tag_plan(&tags, Stage::Ocr, false, true);
        assert_eq!(remove, tags.all_trigger_tags());
        assert_eq!(add, vec!["ai-failed", "ai-failed-ocr"]);
    }

    #[test]
    fn failure_is_terminal_mirrors_fail_job_retry_budget() {
        let mut job = vision_test_job();
        job.max_attempts = 3;
        job.attempts = 1;
        assert!(!failure_is_terminal(&job, true, None));
        assert!(failure_is_terminal(&job, false, None));
        job.attempts = 3;
        assert!(failure_is_terminal(&job, true, None));
        // A higher infrastructure ceiling keeps it retryable (#305).
        assert!(!failure_is_terminal(&job, true, Some(10)));
        // A ceiling never lowers the budget.
        job.attempts = 2;
        assert!(!failure_is_terminal(&job, true, Some(1)));
    }
}
