//! DB-required integration tests for the central status model (#439) and the
//! startup-repair markers (#443).
//!
//! The pure transition tables are unit-tested in `src/transitions.rs`; these
//! tests pin that the SQL writers actually enforce them (a live transition
//! never overwrites a terminal run, the inventory badge follows the run, the
//! single review revert path refuses illegal targets) and that a startup
//! repair is recorded exactly once per (name, version).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, claim_jobs, claim_review_for_apply, complete_job, connect, create_review_item,
    create_run_with_jobs, mark_review_apply_conflict, migrate, revert_review_from_applying,
};
use serde_json::json;
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database (DB integration tests share one database: run them serially with `-- --ignored --test-threads=1`, see scripts/verify/migration_smoke.sh)");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate paperless_apply_intents, review_items, jobs, pipeline_runs,
                 document_inventory, audit_events, metrics_counters
          restart identity cascade;
        "#,
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
}

async fn run_status(pool: &DbPool, run_id: Uuid) -> String {
    sqlx::query_scalar("select status from pipeline_runs where id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("run status")
}

async fn badge(pool: &DbPool, document_id: i32) -> Option<String> {
    sqlx::query_scalar(
        "select current_run_status from document_inventory where paperless_document_id = $1",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("inventory badge")
}

async fn review_status(pool: &DbPool, review_id: Uuid) -> String {
    sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(pool)
        .await
        .expect("review status")
}

/// #439: `complete_job` used to set `succeeded` without a source guard, so a
/// run an operator/recovery had already failed was silently resurrected.
/// The transition table only lets it fire from an active run, and the
/// inventory mirror follows whatever the run really is.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn completing_a_job_never_overwrites_a_terminal_run() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let run_id = create_run_with_jobs(
        &pool,
        11,
        &[Stage::Ocr],
        ProcessingMode::FullAuto,
        "test",
        "test",
    )
    .await
    .expect("create run");
    let jobs = claim_jobs(&pool, 1, "worker-a", 300).await.expect("claim");
    assert_eq!(jobs.len(), 1);
    assert_eq!(run_status(&pool, run_id).await, "running");
    assert_eq!(badge(&pool, 11).await.as_deref(), Some("running"));

    sqlx::query("update pipeline_runs set status = 'failed' where id = $1")
        .bind(run_id)
        .execute(&pool)
        .await
        .expect("fail run out of band");
    assert!(
        complete_job(&pool, &jobs[0], "worker-a", json!({"ok": true}))
            .await
            .expect("complete job"),
        "the lease is still held"
    );
    assert_eq!(run_status(&pool, run_id).await, "failed");
    assert_eq!(badge(&pool, 11).await.as_deref(), Some("failed"));
}

/// #439: the review stage mirrors `waiting_review` onto the inventory badge
/// (it used to stay `running`), and completing the only stage settles both.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn review_creation_mirrors_waiting_review_onto_the_badge() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let run_id = create_run_with_jobs(
        &pool,
        12,
        &[Stage::Ocr, Stage::Metadata],
        ProcessingMode::ManualReview,
        "test",
        "test",
    )
    .await
    .expect("create run");
    let jobs = claim_jobs(&pool, 1, "worker-a", 300).await.expect("claim");
    let review_id = create_review_item(
        &pool,
        &jobs[0],
        json!({"content": "x"}),
        json!([]),
        json!({}),
        "worker-a",
    )
    .await
    .expect("create review")
    .expect("lease held");
    assert_eq!(run_status(&pool, run_id).await, "waiting_review");
    assert_eq!(badge(&pool, 12).await.as_deref(), Some("waiting_review"));

    // Illegal revert targets are refused by the review transition table.
    sqlx::query("update review_items set status = 'approved' where id = $1")
        .bind(review_id)
        .execute(&pool)
        .await
        .expect("approve");
    claim_review_for_apply(&pool, review_id)
        .await
        .expect("claim review")
        .expect("claimed");
    assert!(
        revert_review_from_applying(&pool, review_id, "rejected")
            .await
            .is_err()
    );
    assert!(
        mark_review_apply_conflict(&pool, review_id, "applied", &[], "user", None)
            .await
            .is_err()
    );
    assert_eq!(review_status(&pool, review_id).await, "applying");

    // The conflict path and the plain revert share one transition.
    assert!(
        mark_review_apply_conflict(
            &pool,
            review_id,
            "pending",
            &["title".to_owned()],
            "user",
            None,
        )
        .await
        .expect("conflict revert")
    );
    assert_eq!(review_status(&pool, review_id).await, "pending");
    let row = sqlx::query(
        "select conflict_fields, reviewed_at is null as cleared from review_items where id = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("conflict fields");
    let fields: serde_json::Value = row.get("conflict_fields");
    let reviewed_at_cleared: bool = row.get("cleared");
    assert_eq!(fields, json!(["title"]));
    assert!(reviewed_at_cleared);
    // Not `applying` any more: a second revert is a no-op.
    revert_review_from_applying(&pool, review_id, "pending")
        .await
        .expect("idempotent revert");
    assert_eq!(review_status(&pool, review_id).await, "pending");
}
