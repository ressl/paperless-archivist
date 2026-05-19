//! DB-required integration test for the v1.4.0 age-derived job priority + manual override.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.
//!
//! The test seeds three runs:
//!   * doc 100  — auto-selector path (priority = 1_000_000 - 100)
//!   * doc 500  — auto-selector path (priority = 1_000_000 - 500, smaller -> newer -> wins)
//!   * doc 42   — manual trigger    (priority = 0, top priority)
//!
//! Expected claim order: 42 (manual) -> 500 (newer auto) -> 100 (older auto).

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, age_derived_priority, claim_jobs, connect, create_run_with_jobs_with_priority, migrate,
};
use sqlx::Executor;

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

async fn seed_inventory(pool: &DbPool, document_id: i32) {
    sqlx::query(
        r#"
        insert into document_inventory (paperless_document_id, current_tags)
        values ($1, '{}')
        "#,
    )
    .bind(document_id)
    .execute(pool)
    .await
    .expect("seed inventory row");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn claim_jobs_prefers_manual_then_newest_auto() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Older auto-selected document.
    seed_inventory(&pool, 100).await;
    create_run_with_jobs_with_priority(
        &pool,
        100,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "auto-selector",
        "test",
        Some(age_derived_priority(100)),
    )
    .await
    .expect("create older auto run");

    // Newer auto-selected document.
    seed_inventory(&pool, 500).await;
    create_run_with_jobs_with_priority(
        &pool,
        500,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "auto-selector",
        "test",
        Some(age_derived_priority(500)),
    )
    .await
    .expect("create newer auto run");

    // Manual trigger — priority 0 jumps the queue regardless of age.
    seed_inventory(&pool, 42).await;
    create_run_with_jobs_with_priority(
        &pool,
        42,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "manual",
        "test",
        Some(0),
    )
    .await
    .expect("create manual run");

    let claimed = claim_jobs(&pool, 1, "test-worker", 300)
        .await
        .expect("claim first job");
    assert_eq!(
        claimed.len(),
        1,
        "claim limit honored: exactly one job claimed"
    );
    assert_eq!(
        claimed[0].paperless_document_id, 42,
        "manual trigger (priority 0) wins over both auto runs"
    );

    let claimed = claim_jobs(&pool, 1, "test-worker", 300)
        .await
        .expect("claim second job");
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].paperless_document_id, 500,
        "newer auto document (doc id 500) wins over older (doc id 100)"
    );

    let claimed = claim_jobs(&pool, 1, "test-worker", 300)
        .await
        .expect("claim third job");
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].paperless_document_id, 100,
        "older auto document drains last"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn claim_jobs_preserves_stage_order_within_a_run() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // A single run with two stages: Ocr should drain before Metadata even though both
    // share the same cross-run priority (age-derived from the same doc id).
    seed_inventory(&pool, 999).await;
    create_run_with_jobs_with_priority(
        &pool,
        999,
        &[Stage::Ocr, Stage::Metadata],
        ProcessingMode::ManualReview,
        "manual",
        "test",
        Some(0),
    )
    .await
    .expect("create run");

    let claimed = claim_jobs(&pool, 2, "test-worker", 300)
        .await
        .expect("claim first batch");
    assert_eq!(
        claimed.len(),
        1,
        "stage ordering must surface only the OCR job; Metadata waits behind it"
    );
    assert_eq!(claimed[0].stage, Stage::Ocr);
}

#[test]
fn age_derived_priority_floors_at_one() {
    // Documents beyond a million IDs (or i32::MAX) saturate to 1 rather than going negative
    // and inadvertently outranking a manual trigger.
    assert_eq!(age_derived_priority(1), 999_999);
    assert_eq!(age_derived_priority(1_000_000), 1);
    assert_eq!(age_derived_priority(1_000_001), 1);
    assert_eq!(age_derived_priority(i32::MAX), 1);
}
