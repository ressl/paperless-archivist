//! DB-required regression test for dashboard blocked-job counts.
//!
//! Run with a disposable PostgreSQL database:
//! `DATABASE_URL=postgres://... cargo test -p archivist-db --test blocked_queued_jobs -- --ignored`

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{DbPool, connect, count_blocked_queued_jobs, create_run_with_jobs, migrate};
use sqlx::Executor;
use uuid::Uuid;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        "truncate document_inventory, jobs, pipeline_runs, audit_events restart identity cascade;",
    )
    .await
    .expect("truncate test tables");
    Some(pool)
}

async fn seed_two_stage_run(pool: &DbPool, document_id: i32) -> Uuid {
    sqlx::query(
        "insert into document_inventory (paperless_document_id, current_tags) values ($1, '{}')",
    )
    .bind(document_id)
    .execute(pool)
    .await
    .expect("seed inventory row");
    create_run_with_jobs(
        pool,
        document_id,
        &[Stage::Ocr, Stage::Metadata],
        ProcessingMode::ManualReview,
        "blocked-count-test",
        "blocked-count-test",
    )
    .await
    .expect("create run")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn counts_only_failed_or_review_predecessors_as_blocked() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let normal_run = seed_two_stage_run(&pool, 1).await;
    let failed_run = seed_two_stage_run(&pool, 2).await;
    let review_run = seed_two_stage_run(&pool, 3).await;

    sqlx::query("update jobs set status = 'failed' where run_id = $1 and stage = 'ocr'")
        .bind(failed_run)
        .execute(&pool)
        .await
        .expect("mark failed predecessor");
    sqlx::query("update jobs set status = 'waiting_review' where run_id = $1 and stage = 'ocr'")
        .bind(review_run)
        .execute(&pool)
        .await
        .expect("mark review predecessor");

    let queued_predecessor = count_blocked_queued_jobs(&pool)
        .await
        .expect("count blocked jobs with queued predecessor");
    assert_eq!(queued_predecessor.blocked_by_failed, 1);
    assert_eq!(queued_predecessor.blocked_by_review, 1);
    assert_eq!(
        queued_predecessor.total, 2,
        "a normal queued predecessor must not create a blocked-job alert"
    );

    sqlx::query("update jobs set status = 'running' where run_id = $1 and stage = 'ocr'")
        .bind(normal_run)
        .execute(&pool)
        .await
        .expect("mark normal predecessor running");
    let running_predecessor = count_blocked_queued_jobs(&pool)
        .await
        .expect("count blocked jobs with running predecessor");
    assert_eq!(running_predecessor.blocked_by_failed, 1);
    assert_eq!(running_predecessor.blocked_by_review, 1);
    assert_eq!(
        running_predecessor.total, 2,
        "a normal running predecessor must not create a blocked-job alert"
    );
}
