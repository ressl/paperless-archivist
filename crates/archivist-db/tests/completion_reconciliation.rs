//! Candidate selection for Paperless' global completion-tag reconciliation.
//! The query must only return documents whose enabled stages are terminal and
//! whose current run is not active or waiting for review.

use std::sync::LazyLock;

use archivist_core::{ProcessingMode, Stage, WorkflowRules};
use archivist_db::{
    completed_document_ids_missing_full_tag, connect, create_run_with_jobs, migrate,
    queue_missing_pipeline, release_completion_tag_reservation, reserve_completion_tag_reconcile,
};
use sqlx::{Executor, Row};
use tokio::{
    sync::Mutex,
    time::{Duration, timeout},
};

static DB_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn completion_candidates_require_every_enabled_stage_to_be_terminal() {
    let _db_lock = DB_LOCK.lock().await;
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL test database");
    let pool = connect(&database_url, 10)
        .await
        .expect("connect test database (DB integration tests share one database: run them serially with `-- --ignored --test-threads=1`, see scripts/verify/migration_smoke.sh)");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate document_inventory, jobs, pipeline_runs, review_items,
                 ai_artifacts, audit_events restart identity cascade;
        insert into document_inventory (
          paperless_document_id, current_tags, has_full_completion_tag,
          ocr_status, metadata_status, current_run_status, complete
        ) values
          (1, '{}',             false, 'succeeded', 'succeeded', null,             false),
          (2, '{}',             false, 'succeeded', 'unknown',   null,             false),
          (3, '{}',             false, 'succeeded', 'rejected',  'rejected',       false),
          (4, '{ai-processed}', true,  'succeeded', 'succeeded', 'succeeded',      true),
          (5, '{}',             false, 'succeeded', 'succeeded', 'running',        false),
          (6, '{}',             false, 'succeeded', 'succeeded', 'waiting_review', false),
          (7, '{}',             false, 'succeeded', 'succeeded', 'queued',         false),
          (8, '{}',             false, 'succeeded', 'succeeded', 'applying',       false);
        "#,
    )
    .await
    .expect("seed reconciliation candidates");

    let both = completed_document_ids_missing_full_tag(&pool, &[Stage::Ocr, Stage::Metadata])
        .await
        .expect("both-stage candidates");
    assert_eq!(both, vec![1, 3]);

    let ocr_only = completed_document_ids_missing_full_tag(&pool, &[Stage::Ocr])
        .await
        .expect("OCR-only candidates");
    assert_eq!(ocr_only, vec![1, 2, 3]);

    let disabled = completed_document_ids_missing_full_tag(&pool, &[])
        .await
        .expect("disabled-stage candidates");
    assert!(disabled.is_empty());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn completion_reconcile_guard_rechecks_after_candidate_selection() {
    let _db_lock = DB_LOCK.lock().await;
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL test database");
    let pool = connect(&database_url, 10)
        .await
        .expect("connect test database (DB integration tests share one database: run them serially with `-- --ignored --test-threads=1`, see scripts/verify/migration_smoke.sh)");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate document_inventory, jobs, pipeline_runs, review_items,
                 ai_artifacts, audit_events restart identity cascade;
        insert into document_inventory (
          paperless_document_id, current_tags, has_full_completion_tag,
          ocr_status, metadata_status, current_run_status, complete
        ) values (10, '{}', false, 'succeeded', 'succeeded', null, false);
        "#,
    )
    .await
    .expect("seed reconciliation race candidate");

    let initially_selected =
        completed_document_ids_missing_full_tag(&pool, &[Stage::Ocr, Stage::Metadata])
            .await
            .expect("select initial candidate");
    assert_eq!(initially_selected, vec![10]);

    create_run_with_jobs(
        &pool,
        10,
        &[Stage::Ocr, Stage::Metadata],
        ProcessingMode::ManualReview,
        "race-test",
        "test",
    )
    .await
    .expect("start a run after initial selection");

    let reserved = reserve_completion_tag_reconcile(&pool, 10, &[Stage::Ocr, Stage::Metadata])
        .await
        .expect("recheck candidate under lock");
    assert!(
        !reserved,
        "an active run created after candidate selection must cancel the write"
    );
}

/// #410: the reservation commits immediately (no advisory lock or pooled
/// connection held across the Paperless PATCH), records the tag in the
/// inventory so neither reconciliation nor the auto-selector pick the
/// document again, and can be released when the Paperless write fails.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn completion_reconcile_reservation_does_not_hold_locks_across_http() {
    let _db_lock = DB_LOCK.lock().await;
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL test database");
    let pool = connect(&database_url, 10)
        .await
        .expect("connect test database (DB integration tests share one database: run them serially with `-- --ignored --test-threads=1`, see scripts/verify/migration_smoke.sh)");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate document_inventory, jobs, pipeline_runs, review_items,
                 ai_artifacts, audit_events restart identity cascade;
        insert into document_inventory (
          paperless_document_id, current_tags, has_full_completion_tag,
          ocr_status, metadata_status, current_run_status, complete
        ) values (11, '{}', false, 'succeeded', 'succeeded', null, false);
        "#,
    )
    .await
    .expect("seed lock candidate");

    let stages = [Stage::Ocr, Stage::Metadata];
    assert!(
        reserve_completion_tag_reconcile(&pool, 11, &stages)
            .await
            .expect("reserve")
    );
    let row = sqlx::query(
        "select has_full_completion_tag, complete from document_inventory where paperless_document_id = 11",
    )
    .fetch_one(&pool)
    .await
    .expect("inventory");
    assert!(row.get::<bool, _>("has_full_completion_tag"));
    assert!(row.get::<bool, _>("complete"));
    assert!(
        completed_document_ids_missing_full_tag(&pool, &stages)
            .await
            .expect("candidates")
            .is_empty()
    );
    // A second reservation for the same document is refused.
    assert!(
        !reserve_completion_tag_reconcile(&pool, 11, &stages)
            .await
            .expect("second reserve")
    );
    let queued = queue_missing_pipeline(
        &pool,
        &stages,
        ProcessingMode::ManualReview,
        "auto-selector",
        "worker",
        &WorkflowRules::default(),
        Some(10),
    )
    .await
    .expect("auto-selector");
    assert_eq!(queued, 0, "reserved document is not auto-selected");

    // No lock is held while the Paperless write would run: run creation for
    // the same document is not blocked.
    timeout(
        Duration::from_secs(2),
        create_run_with_jobs(
            &pool,
            11,
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "parallel-run-test",
            "test",
        ),
    )
    .await
    .expect("run creation is not blocked by the reservation")
    .expect("run created");

    release_completion_tag_reservation(&pool, 11)
        .await
        .expect("release");
    let row = sqlx::query(
        "select has_full_completion_tag, complete from document_inventory where paperless_document_id = 11",
    )
    .fetch_one(&pool)
    .await
    .expect("inventory");
    assert!(!row.get::<bool, _>("has_full_completion_tag"));
    assert!(!row.get::<bool, _>("complete"));
}
