//! DB-required integration test: `fail_job`'s `retry_ceiling` lets a transient
//! Paperless *infrastructure* outage ride past the per-job `max_attempts`
//! budget (so an upstream gateway outage doesn't permanently fail the whole
//! backlog), while a `None` ceiling keeps the normal budget and a
//! budget-exhausted job fails permanently. #305.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{DbPool, claim_jobs, connect, create_run_with_jobs, fail_job, migrate};
use sqlx::{Executor, Row};

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
        "test",
        "test",
    )
    .await
    .expect("create run");
}

async fn job_status(pool: &DbPool, document_id: i32) -> String {
    sqlx::query("select status from jobs where paperless_document_id = $1")
        .bind(document_id)
        .fetch_one(pool)
        .await
        .expect("read job status")
        .try_get("status")
        .expect("status column")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn infra_ceiling_retries_past_max_attempts_while_normal_budget_fails() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Two identical jobs at the edge of the default budget: doc 1 will be
    // failed with the normal (None) ceiling, doc 2 rescued by the infra
    // ceiling. Set attempts = 2 so the single claim below bumps each to
    // attempts = 3 = max_attempts (the normal budget is now exhausted).
    seed_run(&pool, 1).await;
    seed_run(&pool, 2).await;
    sqlx::query("update jobs set attempts = 2 where paperless_document_id in (1, 2)")
        .execute(&pool)
        .await
        .expect("pre-age the retry budget");

    let claimed = claim_jobs(&pool, 10, "worker-a", 300)
        .await
        .expect("claim jobs");
    let job1 = claimed
        .iter()
        .find(|j| j.paperless_document_id == 1)
        .expect("doc 1 claimed");
    let job2 = claimed
        .iter()
        .find(|j| j.paperless_document_id == 2)
        .expect("doc 2 claimed");
    assert_eq!(job1.attempts, 3, "claim bumped attempts to the budget edge");
    assert_eq!(job1.max_attempts, 3, "default max_attempts");

    // doc 1 — normal budget, attempts (3) >= max_attempts (3): permanent fail.
    let held1 = fail_job(&pool, job1, "worker-a", "paperless gateway 503", true, None)
        .await
        .expect("fail_job doc 1");
    assert!(held1, "lease was held, so fail_job applied");
    assert_eq!(
        job_status(&pool, 1).await,
        "failed",
        "normal ceiling: a budget-exhausted job fails permanently"
    );

    // doc 2 — same attempts, but the infra ceiling (20) > attempts (3): retry.
    let held2 = fail_job(
        &pool,
        job2,
        "worker-a",
        "paperless gateway 503",
        true,
        Some(20),
    )
    .await
    .expect("fail_job doc 2");
    assert!(held2, "lease was held, so fail_job applied");
    assert_eq!(
        job_status(&pool, 2).await,
        "queued",
        "infra ceiling: the job is rescheduled instead of failed"
    );

    // The rescue reschedules with backoff but does NOT reset the budget — it
    // raises the ceiling, so attempts is preserved and the job still fails
    // eventually if the outage never ends.
    let row = sqlx::query(
        "select attempts, run_after > now() as deferred from jobs where paperless_document_id = 2",
    )
    .fetch_one(&pool)
    .await
    .expect("read doc 2 job");
    let attempts: i32 = row.try_get("attempts").expect("attempts");
    let deferred: bool = row.try_get("deferred").expect("deferred");
    assert_eq!(
        attempts, 3,
        "ceiling raises the bar, it does not reset attempts"
    );
    assert!(
        deferred,
        "the retry is scheduled into the future with backoff"
    );
}
