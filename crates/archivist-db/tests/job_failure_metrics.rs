//! DB-required regression test for the monotone permanent-job-failure metric.
//!
//! Run with a disposable PostgreSQL database:
//! `DATABASE_URL=postgres://... cargo test -p archivist-db --test job_failure_metrics -- --ignored`

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, claim_jobs, connect, create_run_with_jobs, fail_job, migrate, read_metric_counters,
};
use sqlx::Executor;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        "truncate document_inventory, jobs, pipeline_runs, audit_events, metrics_counters restart identity cascade;",
    )
    .await
    .expect("truncate test tables");
    Some(pool)
}

async fn seed_run(pool: &DbPool, document_id: i32) {
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
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "metric-test",
        "metric-test",
    )
    .await
    .expect("create run");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn only_committed_permanent_failures_increment_the_counter() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    for document_id in 1..=3 {
        seed_run(&pool, document_id).await;
    }
    let claimed = claim_jobs(&pool, 10, "metrics-worker", 300)
        .await
        .expect("claim jobs");

    let permanent = claimed
        .iter()
        .find(|job| job.paperless_document_id == 1)
        .expect("permanent job claimed");
    assert!(
        fail_job(&pool, permanent, "metrics-worker", "permanent", false, None,)
            .await
            .expect("record permanent failure")
    );

    let retry = claimed
        .iter()
        .find(|job| job.paperless_document_id == 2)
        .expect("retry job claimed");
    assert!(
        fail_job(&pool, retry, "metrics-worker", "transient", true, None,)
            .await
            .expect("schedule retry")
    );

    let lost = claimed
        .iter()
        .find(|job| job.paperless_document_id == 3)
        .expect("lost-lease job claimed");
    assert!(
        !fail_job(&pool, lost, "another-worker", "stale", false, None)
            .await
            .expect("reject stale worker")
    );

    let counters = read_metric_counters(&pool)
        .await
        .expect("read metric counters");
    assert_eq!(
        counters.get("job_failures_total"),
        Some(&1),
        "only the committed permanent job.failed transition is counted"
    );
}
