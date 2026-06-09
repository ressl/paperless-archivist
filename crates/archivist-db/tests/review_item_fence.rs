//! DB-required integration test: `create_review_item` is fenced on lease
//! ownership — a worker whose lease was reclaimed must not insert review
//! items or flip the job's status, while the owning worker can create several
//! per-field items back to back.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, claim_jobs, connect, create_review_item, create_run_with_jobs_with_priority, migrate,
};
use serde_json::json;
use sqlx::Executor;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate document_inventory, jobs, pipeline_runs, review_items, audit_events restart identity cascade;
        "#,
    )
    .await
    .expect("truncate test tables");
    Some(pool)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn create_review_item_is_fenced_on_lease_owner() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    sqlx::query(
        "insert into document_inventory (paperless_document_id, current_tags) values (7, '{}')",
    )
    .execute(&pool)
    .await
    .expect("seed inventory");
    create_run_with_jobs_with_priority(
        &pool,
        7,
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

    // A worker that no longer owns the lease is fenced out entirely.
    let foreign = create_review_item(&pool, job, json!({"title": "x"}), json!([]), "worker-b")
        .await
        .expect("fenced call succeeds");
    assert_eq!(foreign, None);
    let status: String = sqlx::query_scalar("select status from jobs where id = $1")
        .bind(job.id)
        .fetch_one(&pool)
        .await
        .expect("job status");
    assert_eq!(status, "running");
    let count: i64 = sqlx::query_scalar("select count(*) from review_items")
        .fetch_one(&pool)
        .await
        .expect("review count");
    assert_eq!(count, 0);

    // The owner creates two per-field items back to back (second call runs
    // against the job already in waiting_review).
    let first = create_review_item(&pool, job, json!({"title": "x"}), json!([]), "worker-a")
        .await
        .expect("first item");
    assert!(first.is_some());
    let second = create_review_item(&pool, job, json!({"tags": [1]}), json!([]), "worker-a")
        .await
        .expect("second item");
    assert!(second.is_some());

    let status: String = sqlx::query_scalar("select status from jobs where id = $1")
        .bind(job.id)
        .fetch_one(&pool)
        .await
        .expect("job status");
    assert_eq!(status, "waiting_review");
    let count: i64 = sqlx::query_scalar("select count(*) from review_items")
        .fetch_one(&pool)
        .await
        .expect("review count");
    assert_eq!(count, 2);
}
