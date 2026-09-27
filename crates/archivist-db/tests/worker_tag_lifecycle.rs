//! DB-required integration tests for the worker tag / review lifecycle:
//! * #403 — the autopilot drain never picks review items with hard
//!   validation errors;
//! * #400 — the trigger poller's "latest terminal run" lookup;
//! * #411 — the AI-managed tag set used by `replace_ai_managed`.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, ai_managed_tag_ids_for_document, claim_jobs, connect, create_review_item,
    create_run_with_jobs_with_priority, latest_terminal_run_finished_at,
    list_pending_review_items_for_autopilot_drain, migrate, tag_catalog_entries_for_ids,
};
use serde_json::json;
use sqlx::Executor;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate document_inventory, jobs, pipeline_runs, review_items, audit_events,
                 paperless_apply_intents, paperless_tags restart identity cascade;
        "#,
    )
    .await
    .expect("truncate test tables");
    Some(pool)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn autopilot_drain_skips_review_items_with_validation_errors() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    sqlx::query(
        "insert into document_inventory (paperless_document_id, current_tags) values (9, '{}')",
    )
    .execute(&pool)
    .await
    .expect("seed inventory");
    create_run_with_jobs_with_priority(
        &pool,
        9,
        &[Stage::Metadata],
        ProcessingMode::ManualReview,
        "test",
        "test",
        None,
    )
    .await
    .expect("create run");
    let jobs = claim_jobs(&pool, 1, "worker-a", 300).await.expect("claim");
    let job = &jobs[0];
    let baseline = json!({"tags": [1]});
    let cases = [
        ("clean", json!([])),
        (
            "soft",
            json!(["Dry-run is enabled: validated patch was evaluated but not auto-applied."]),
        ),
        (
            "low_confidence",
            json!([{"LowConfidence": {"actual": 0.3, "threshold": 0.55}}]),
        ),
        ("unknown_tag", json!([{"UnknownTag": "ai-processed"}])),
        ("empty", json!(["EmptyOutput"])),
        ("not_array", json!({"LowConfidence": {}})),
    ];
    for (name, warnings) in cases {
        create_review_item(
            &pool,
            job,
            json!({"tags": [1], "case": name}),
            warnings,
            baseline.clone(),
            "worker-a",
        )
        .await
        .expect("create review item")
        .expect("lease owned");
    }

    let drained = list_pending_review_items_for_autopilot_drain(&pool, 100)
        .await
        .expect("drain list");
    let mut names: Vec<String> = drained
        .iter()
        .map(|item| item.suggested_patch["case"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["clean".to_owned(), "soft".to_owned()]);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn latest_terminal_run_lookup_ignores_active_and_older_runs() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    pool.execute(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages, finished_at, created_at)
        values
          -- doc 1: old success, newer failure -> the failure's finish time
          (1, 'manual_review', 'ai-process', 'succeeded', '["metadata"]', '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z'),
          (1, 'manual_review', 'ai-process', 'failed', '["metadata"]', '2026-09-02T00:00:00Z', '2026-09-02T00:00:00Z'),
          -- doc 2: terminal run followed by an active one -> absent
          (2, 'manual_review', 'ai-process', 'rejected', '["metadata"]', '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z'),
          (2, 'manual_review', 'ai-process', 'queued', '["metadata"]', null, '2026-09-03T00:00:00Z');
        "#,
    )
    .await
    .expect("seed runs");

    let latest = latest_terminal_run_finished_at(&pool, &[1, 2, 3])
        .await
        .expect("lookup");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[&1].to_rfc3339(), "2026-09-02T00:00:00+00:00");
    assert!(
        latest_terminal_run_finished_at(&pool, &[])
            .await
            .expect("empty")
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn ai_managed_tags_are_tags_archivist_added_minus_workflow_tags() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    pool.execute(
        r#"
        insert into paperless_tags (id, name, is_workflow) values
          (1, 'Inbox', false), (2, 'archivist-ocr', true), (3, 'Rechnung', false),
          (4, 'Privat', false), (5, 'Versicherung', false);
        insert into paperless_apply_intents
          (source, source_key, owner_type, owner_id, paperless_document_id, patch_hash, patch, before_state, state)
        values
          -- landed: added 2 (workflow) and 3
          ('worker_auto', 'job:a', 'worker', 'w', 7, 'h1', '{"tags": [1, 2, 3]}', '{"tags": [1]}', 'finalized'),
          -- never landed: must not count
          ('worker_auto', 'job:b', 'worker', 'w', 7, 'h2', '{"tags": [1, 5]}', '{"tags": [1]}', 'failed'),
          -- no before_state tags (e.g. scalar-only patch): ignored
          ('worker_auto', 'job:c', 'worker', 'w', 7, 'h3', '{"tags": [4]}', '{}', 'finalized'),
          -- other document
          ('worker_auto', 'job:d', 'worker', 'w', 8, 'h4', '{"tags": [4]}', '{"tags": []}', 'finalized');
        "#,
    )
    .await
    .expect("seed intents");

    assert_eq!(
        ai_managed_tag_ids_for_document(&pool, 7)
            .await
            .expect("ai-managed"),
        vec![3]
    );
    let catalog = tag_catalog_entries_for_ids(&pool, &[2, 3, 99])
        .await
        .expect("catalog");
    assert_eq!(
        catalog,
        vec![
            (2, "archivist-ocr".to_owned(), true),
            (3, "Rechnung".to_owned(), false)
        ]
    );
}
