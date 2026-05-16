//! DB-required integration tests for queue_missing_stage / queue_missing_pipeline.
//!
//! These tests are marked `#[ignore]` so the default `cargo test` run does not require a live
//! PostgreSQL instance. To exercise them locally, run
//! `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage, WorkflowRules};
use archivist_db::{
    DbPool, connect, custom_field_ids_for_names, migrate, queue_missing_pipeline,
    queue_missing_stage, tag_id_pairs_for_names,
};
use sqlx::Executor;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    // Clear the few tables we touch so the test is hermetic across reruns.
    pool.execute(
        r#"
        truncate document_inventory, jobs, pipeline_runs, audit_events restart identity cascade;
        "#,
    )
    .await
    .expect("truncate test tables");
    Some(pool)
}

async fn seed_inventory(pool: &DbPool, count: i32, ocr_status: &str) {
    for id in 1..=count {
        sqlx::query(
            r#"
            insert into document_inventory (
              paperless_document_id, current_tags, ocr_status, tagging_status, title_status,
              correspondent_status, document_type_status, document_date_status, fields_status,
              has_ocr_completion_tag, has_tagging_completion_tag, has_full_completion_tag,
              current_run_status
            )
            values ($1, '{}', $2, 'unknown', 'unknown', 'unknown', 'unknown', 'unknown', 'unknown',
                    false, false, false, null)
            "#,
        )
        .bind(id)
        .bind(ocr_status)
        .execute(pool)
        .await
        .expect("seed inventory row");
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn queue_missing_stage_respects_sql_limit() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 10, "unknown").await;

    let rules = WorkflowRules::default();
    let created = queue_missing_stage(
        &pool,
        Stage::Ocr,
        ProcessingMode::ManualReview,
        "test",
        &rules,
        Some(3),
    )
    .await
    .expect("queue_missing_stage");
    assert_eq!(created, 3, "exactly three runs should be created");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn queue_missing_pipeline_respects_budget() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 10, "unknown").await;

    let rules = WorkflowRules::default();
    let stages = Stage::all_business_stages();
    let created = queue_missing_pipeline(
        &pool,
        &stages,
        ProcessingMode::ManualReview,
        "test",
        "test",
        &rules,
        Some(3),
    )
    .await
    .expect("queue_missing_pipeline");
    assert_eq!(created, 3, "exactly three runs should be created");
}

/// Regression test for v1.5.2 Bug 1: the API endpoint `/api/batches/full` used to call
/// `queue_missing_stage` once per enabled stage, producing two SEPARATE single-stage runs
/// (one with `stages = ["ocr"]`, one with `stages = ["metadata"]`) per document. After the
/// fix the handler delegates to `queue_missing_pipeline`, which emits ONE run per document
/// carrying the full enabled-stages array so the pipeline drains in a single run.
///
/// This test seeds 5 unknown-OCR documents and asserts that, with enabled_stages
/// `[Ocr, Metadata]`, each resulting `pipeline_runs` row contains both stages.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn queue_missing_pipeline_emits_combined_stage_runs() {
    use sqlx::Row;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 5, "unknown").await;

    let rules = WorkflowRules::default();
    let enabled = vec![Stage::Ocr, Stage::Metadata];
    let created = queue_missing_pipeline(
        &pool,
        &enabled,
        ProcessingMode::ManualReview,
        "manual-batch",
        "operator",
        &rules,
        None,
    )
    .await
    .expect("queue_missing_pipeline");
    assert_eq!(created, 5, "one run per eligible document");

    // Every run row should carry both stages, NOT a single-stage array.
    let rows = sqlx::query("select stages from pipeline_runs order by created_at")
        .fetch_all(&pool)
        .await
        .expect("fetch pipeline_runs");
    assert_eq!(rows.len(), 5, "exactly one run per document, not per-stage");
    for row in rows {
        let stages: serde_json::Value = row.try_get("stages").expect("stages column");
        let arr = stages.as_array().expect("stages is jsonb array");
        let names: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            names.contains(&"ocr") && names.contains(&"metadata"),
            "expected combined ocr+metadata stages, got {names:?}"
        );
    }
}

/// Regression test for v1.5.2 Bug 2: review_items created during the consolidated metadata
/// stage's validation-fallback branch used to contain raw LLM tag NAMES like
/// `["Hardware", "Rechnung"]` where the apply path expects `Vec<i32>`. The worker now uses
/// `tag_id_pairs_for_names` to resolve names → ids BEFORE building the review_item; this
/// test pins the SQL contract (case-insensitive match, name+id pair returned, unknown
/// names omitted) so the worker behavior stays correct.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn tag_id_pairs_for_names_is_case_insensitive_and_skips_unknown() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    sqlx::query(
        r#"
        insert into paperless_tags (id, name) values
            (7, 'Hardware'),
            (12, 'Rechnung')
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed paperless_tags");

    let requested = vec![
        "hardware".to_owned(),  // different case
        "RECHNUNG".to_owned(),  // different case
        "NoSuchTag".to_owned(), // unknown
    ];
    let pairs = tag_id_pairs_for_names(&pool, &requested)
        .await
        .expect("tag_id_pairs_for_names");
    let ids: Vec<i32> = pairs.iter().map(|(_, id)| *id).collect();
    assert!(
        ids.contains(&7),
        "Hardware id should match case-insensitively"
    );
    assert!(
        ids.contains(&12),
        "Rechnung id should match case-insensitively"
    );
    assert_eq!(pairs.len(), 2, "unknown tags are NOT returned");
}

/// Companion to the tag pairs test: same contract for custom_field_ids_for_names, since the
/// worker's resolve_custom_field_values_to_ids uses it for the same name-to-id shape fix.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn custom_field_ids_for_names_is_case_insensitive_and_skips_unknown() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    sqlx::query(
        r#"
        insert into paperless_custom_fields (id, name, data_type) values
            (1, 'Invoice Number', 'string'),
            (2, 'Total', 'monetary')
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed paperless_custom_fields");

    let requested = vec!["INVOICE NUMBER".to_owned(), "ghost_field".to_owned()];
    let pairs = custom_field_ids_for_names(&pool, &requested)
        .await
        .expect("custom_field_ids_for_names");
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].1, 1);
}
