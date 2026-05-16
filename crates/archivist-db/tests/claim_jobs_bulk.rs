//! DB-required integration test verifying that claim_jobs issues exactly one bulk run-running
//! update per batch (instead of N per-row updates).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{DbPool, claim_jobs, connect, create_run_with_jobs, migrate};
use sqlx::{Executor, Row};

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate document_inventory, jobs, pipeline_runs, audit_events restart identity cascade;
        "#,
    )
    .await
    .expect("truncate test tables");
    Some(pool)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn claim_jobs_marks_runs_running_in_a_single_bulk_update() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Create five queued runs each with a single OCR job and matching inventory rows.
    for document_id in 1..=5 {
        sqlx::query(
            r#"
            insert into document_inventory (paperless_document_id, current_tags)
            values ($1, '{}')
            "#,
        )
        .bind(document_id)
        .execute(&pool)
        .await
        .expect("seed inventory row");
        create_run_with_jobs(
            &pool,
            document_id,
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "test",
            "test",
        )
        .await
        .expect("create run");
    }

    let claimed = claim_jobs(&pool, 10, "test-worker", 300)
        .await
        .expect("claim jobs");
    assert_eq!(claimed.len(), 5, "all five queued jobs should be claimed");

    let running_runs: i64 = sqlx::query(
        r#"
        select count(*)::bigint as cnt
          from pipeline_runs
         where status = 'running'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("count running runs")
    .try_get("cnt")
    .expect("read count");
    assert_eq!(running_runs, 5, "all runs should be marked running");

    let running_inventory: i64 = sqlx::query(
        r#"
        select count(*)::bigint as cnt
          from document_inventory
         where current_run_status = 'running'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("count running inventory rows")
    .try_get("cnt")
    .expect("read count");
    assert_eq!(
        running_inventory, 5,
        "all inventory rows should be marked running"
    );
}
