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
