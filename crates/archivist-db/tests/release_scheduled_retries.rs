//! DB-required integration test: `release_scheduled_retries` wakes queued jobs
//! whose `run_after` a provider cooldown pushed into the future, without
//! touching already-eligible jobs or the retry budget.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{DbPool, connect, create_run_with_jobs, migrate, release_scheduled_retries};
use sqlx::{Executor, Row};

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
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
async fn release_scheduled_retries_wakes_future_queued_jobs_only() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Four queued runs, each one OCR job (queued, run_after ~ now()).
    for document_id in 1..=4 {
        sqlx::query("insert into document_inventory (paperless_document_id, current_tags) values ($1, '{}')")
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

    // Simulate a cooldown defer: push three jobs' run_after a day out and bump
    // their attempts, leaving one eligible now.
    sqlx::query(
        r#"
        update jobs
           set run_after = now() + interval '1 day', attempts = 2
         where paperless_document_id in (1, 2, 3)
        "#,
    )
    .execute(&pool)
    .await
    .expect("defer three jobs");

    let released = release_scheduled_retries(&pool)
        .await
        .expect("release scheduled retries");
    assert_eq!(released, 3, "only the three future-dated jobs are released");

    // None remain parked in the future, and the retry budget is preserved
    // (this reschedules, it does not reset attempts).
    let future_remaining: i64 = sqlx::query(
        "select count(*)::bigint as cnt from jobs where status = 'queued' and run_after > now()",
    )
    .fetch_one(&pool)
    .await
    .expect("count future jobs")
    .try_get("cnt")
    .expect("read count");
    assert_eq!(future_remaining, 0, "no queued job is left parked");

    let preserved_attempts: i64 = sqlx::query(
        "select count(*)::bigint as cnt from jobs where paperless_document_id in (1,2,3) and attempts = 2",
    )
    .fetch_one(&pool)
    .await
    .expect("count preserved attempts")
    .try_get("cnt")
    .expect("read count");
    assert_eq!(preserved_attempts, 3, "attempts are preserved, not reset");

    // A second call is a no-op now that nothing is parked.
    let released_again = release_scheduled_retries(&pool)
        .await
        .expect("release again");
    assert_eq!(released_again, 0, "idempotent once the queue is drained");
}
