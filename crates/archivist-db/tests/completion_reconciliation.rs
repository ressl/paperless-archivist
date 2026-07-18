//! Candidate selection for Paperless' global completion-tag reconciliation.
//! The query must only return documents whose enabled stages are terminal and
//! whose current run is not active or waiting for review.

use std::sync::LazyLock;

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    begin_completion_tag_reconcile_guard, completed_document_ids_missing_full_tag, connect,
    create_run_with_jobs, migrate,
};
use sqlx::{Executor, PgPool};
use tokio::{
    sync::Mutex,
    time::{Duration, Instant, sleep, timeout},
};

static DB_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

async fn wait_for_advisory_waiter(pool: &PgPool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let waiting: bool = sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from pg_stat_activity
               where datname = current_database()
                 and pid <> pg_backend_pid()
                 and wait_event = 'advisory'
            )
            "#,
        )
        .fetch_one(pool)
        .await
        .expect("inspect advisory-lock waiters");
        if waiting {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "parallel run never reached the document advisory lock"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn completion_candidates_require_every_enabled_stage_to_be_terminal() {
    let _db_lock = DB_LOCK.lock().await;
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL test database");
    let pool = connect(&database_url, 10)
        .await
        .expect("connect test database");
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
        .expect("connect test database");
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

    let guard = begin_completion_tag_reconcile_guard(&pool, 10, &[Stage::Ocr, Stage::Metadata])
        .await
        .expect("recheck candidate under lock");
    assert!(
        guard.is_none(),
        "an active run created after candidate selection must cancel the write"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn completion_reconcile_guard_serializes_parallel_run_creation() {
    let _db_lock = DB_LOCK.lock().await;
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL test database");
    let pool = connect(&database_url, 10)
        .await
        .expect("connect test database");
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

    let guard = begin_completion_tag_reconcile_guard(&pool, 11, &[Stage::Ocr, Stage::Metadata])
        .await
        .expect("acquire reconcile guard")
        .expect("candidate remains eligible");

    let run_pool = pool.clone();
    let run_task = tokio::spawn(async move {
        create_run_with_jobs(
            &run_pool,
            11,
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "parallel-run-test",
            "test",
        )
        .await
    });
    tokio::pin!(run_task);
    wait_for_advisory_waiter(&pool).await;
    assert!(
        timeout(Duration::from_millis(150), &mut run_task)
            .await
            .is_err(),
        "parallel run creation must wait while the external write is guarded"
    );

    guard.commit().await.expect("release reconcile guard");
    timeout(Duration::from_secs(5), &mut run_task)
        .await
        .expect("parallel run unblocks after guard commit")
        .expect("parallel run task completes")
        .expect("parallel run succeeds");
}
