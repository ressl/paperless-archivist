//! DB-required integration tests for queue_missing_stage / queue_missing_pipeline.
//!
//! These tests are marked `#[ignore]` so the default `cargo test` run does not require a live
//! PostgreSQL instance. To exercise them locally, run
//! `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage, WorkflowRules};
use archivist_db::{DbPool, connect, migrate, queue_missing_pipeline, queue_missing_stage};
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
