//! DB-required integration test: the review-apply path claims a row into the
//! intermediate `applying` status so two concurrent applies (or an operator
//! racing the autopilot drain) cannot both PATCH Paperless. Also covers the
//! stranded-`applying` recovery sweep. #253.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::Stage;
use archivist_db::{
    DbPool, claim_pending_review_for_autopilot_drain, claim_review_for_apply, connect, migrate,
    reset_stale_applying_reviews, revert_review_from_applying,
};
use sqlx::Executor;
use uuid::Uuid;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"truncate review_items, pipeline_runs, document_inventory, audit_events restart identity cascade;"#,
    )
    .await
    .expect("truncate test tables");
    Some(pool)
}

async fn seed_review(pool: &DbPool, status: &str) -> Uuid {
    let run_id: Uuid = sqlx::query_scalar(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
        values (1, 'full_auto', 'ai-process', 'waiting_review', '[]'::jsonb)
        returning id
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("insert run");
    sqlx::query_scalar(
        r#"
        insert into review_items (run_id, paperless_document_id, stage, status, suggested_patch, validation_warnings)
        values ($1, 1, $2, $3, '{"title":"x"}'::jsonb, '[]'::jsonb)
        returning id
        "#,
    )
    .bind(run_id)
    .bind(Stage::Metadata.to_string())
    .bind(status)
    .fetch_one(pool)
    .await
    .expect("insert review item")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn concurrent_apply_claims_are_mutually_exclusive() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "approved").await;

    // First claim wins and carries the prior status; second sees nothing.
    let first = claim_review_for_apply(&pool, review_id)
        .await
        .expect("first claim");
    let second = claim_review_for_apply(&pool, review_id)
        .await
        .expect("second claim");
    assert!(first.is_some());
    assert_eq!(first.unwrap().status, "approved");
    assert!(second.is_none(), "second concurrent apply must be fenced out");

    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "applying");

    // A failed PATCH reverts to the prior status for retry.
    revert_review_from_applying(&pool, review_id, "approved")
        .await
        .expect("revert");
    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "approved");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn autopilot_drain_claims_into_applying_and_blocks_human_apply() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "pending").await;

    // The drain claims pending -> applying; a human apply cannot then claim it.
    let drained = claim_pending_review_for_autopilot_drain(&pool, review_id)
        .await
        .expect("drain claim");
    assert!(drained.is_some());
    let human = claim_review_for_apply(&pool, review_id)
        .await
        .expect("human claim");
    assert!(human.is_none(), "human apply must not race the drain");

    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "applying");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn stale_applying_rows_are_recovered() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "approved").await;
    claim_review_for_apply(&pool, review_id)
        .await
        .expect("claim");
    // Backdate the claim so it counts as stranded.
    sqlx::query("update review_items set reviewed_at = now() - interval '10 minutes' where id = $1")
        .bind(review_id)
        .execute(&pool)
        .await
        .expect("backdate");

    let recovered = reset_stale_applying_reviews(&pool, 300)
        .await
        .expect("sweep");
    assert_eq!(recovered, 1);
    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "pending");
}
