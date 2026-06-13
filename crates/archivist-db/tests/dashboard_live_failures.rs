//! DB-required integration test: the dashboard live-failures panel surfaces a
//! document's CURRENT-run failure but NOT a superseded one whose document was
//! re-run to success. The `jobs` table keeps every failed row forever, so an
//! unfiltered "last N failed jobs" showed stale failures that disagreed with
//! the failed KPI (which reads live `document_inventory`). Binding to
//! `last_run_id` fixes it.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, RuntimeSettings, Stage};
use archivist_db::{
    DbPool, claim_jobs, complete_job, connect, create_run_with_jobs, fail_job,
    get_dashboard_live_status, migrate,
};
use serde_json::json;
use sqlx::Executor;

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

async fn seed_inventory(pool: &DbPool, document_id: i32) {
    sqlx::query(
        "insert into document_inventory (paperless_document_id, current_tags) values ($1, '{}')",
    )
    .bind(document_id)
    .execute(pool)
    .await
    .expect("seed inventory row");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn live_failures_show_current_run_only_not_superseded() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // doc 1: its current (and only) run fails → must appear.
    seed_inventory(&pool, 1).await;
    create_run_with_jobs(
        &pool,
        1,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "test",
        "test",
    )
    .await
    .expect("doc1 run");

    // doc 2: an old run fails, then a fresh run succeeds → the old failure is
    // superseded (last_run_id points at the succeeded run) and must NOT appear.
    seed_inventory(&pool, 2).await;
    create_run_with_jobs(
        &pool,
        2,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "test",
        "test",
    )
    .await
    .expect("doc2 old run");

    // Claim both first-round jobs and fail them.
    let claimed = claim_jobs(&pool, 10, "w", 300)
        .await
        .expect("claim round 1");
    for job in &claimed {
        fail_job(&pool, job, "w", "boom", false, None)
            .await
            .expect("fail job");
    }

    // doc 2 gets a fresh run that succeeds.
    create_run_with_jobs(
        &pool,
        2,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "test",
        "test",
    )
    .await
    .expect("doc2 new run");
    let claimed2 = claim_jobs(&pool, 10, "w", 300)
        .await
        .expect("claim round 2");
    let job2_new = claimed2
        .iter()
        .find(|j| j.paperless_document_id == 2)
        .expect("doc2 new job claimed");
    complete_job(&pool, job2_new, "w", json!({"ok": true}))
        .await
        .expect("complete doc2 new job");

    let settings = RuntimeSettings::default();
    let live = get_dashboard_live_status(&pool, &settings)
        .await
        .expect("live status");
    let failed_docs: Vec<i32> = live
        .recent_failures
        .iter()
        .map(|f| f.paperless_document_id)
        .collect();

    assert!(
        failed_docs.contains(&1),
        "doc 1's current-run failure must show; got {failed_docs:?}"
    );
    assert!(
        !failed_docs.contains(&2),
        "doc 2's superseded failure (re-run to success) must be hidden; got {failed_docs:?}"
    );
}
