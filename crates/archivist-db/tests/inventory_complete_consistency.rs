//! DB-required integration tests for the unified `complete` definition (#410)
//! and the `last_run_id` guards in `complete_job` / `recover_stuck_runs`
//! (#414).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage, WorkflowRules};
use archivist_db::{
    DbPool, InventoryUpsert, claim_jobs, complete_job, connect, create_run_with_jobs, migrate,
    queue_missing_stage, recover_stuck_runs, upsert_inventory_item,
};
use serde_json::json;
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        "truncate document_inventory, jobs, pipeline_runs, audit_events restart identity cascade;",
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
}

async fn seed_inventory(pool: &DbPool, document_id: i32, full_tag: bool) {
    sqlx::query(
        r#"
        insert into document_inventory (paperless_document_id, current_tags, has_full_completion_tag)
        values ($1, '{}', $2)
        "#,
    )
    .bind(document_id)
    .bind(full_tag)
    .execute(pool)
    .await
    .expect("seed inventory row");
}

async fn complete_flag(pool: &DbPool, document_id: i32) -> (bool, Option<String>) {
    let row = sqlx::query(
        "select complete, current_run_status from document_inventory where paperless_document_id = $1",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("inventory row");
    (row.get("complete"), row.get("current_run_status"))
}

fn upsert(document_id: i32, full_tag: bool) -> InventoryUpsert {
    InventoryUpsert {
        paperless_document_id: document_id,
        title: None,
        original_file_name: None,
        current_tags: Vec::new(),
        current_tag_ids: Vec::new(),
        correspondent_id: None,
        document_type_id: None,
        document_date: None,
        paperless_modified_at: None,
        has_ocr_completion_tag: false,
        has_tagging_completion_tag: false,
        has_full_completion_tag: full_tag,
    }
}

/// #410: `complete_job` and the Paperless sync compute `complete` the same
/// way (the global completion tag), so the flag no longer flips between them.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn complete_job_and_sync_agree_on_complete() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 1, false).await;
    seed_inventory(&pool, 2, true).await;
    for document_id in [1, 2] {
        create_run_with_jobs(
            &pool,
            document_id,
            &[Stage::Ocr],
            ProcessingMode::ManualReview,
            "test",
            "test",
        )
        .await
        .expect("create run");
    }
    let jobs = claim_jobs(&pool, 10, "worker", 300).await.expect("claim");
    assert_eq!(jobs.len(), 2);
    for job in &jobs {
        assert!(
            complete_job(&pool, job, "worker", json!({"ok": true}))
                .await
                .expect("complete job")
        );
    }
    let after_job = [complete_flag(&pool, 1).await, complete_flag(&pool, 2).await];
    assert_eq!(after_job[0], (false, Some("succeeded".to_owned())));
    assert_eq!(after_job[1], (true, Some("succeeded".to_owned())));

    // A sync with unchanged Paperless tags must not flip the flag.
    let mut tx = pool.begin().await.expect("begin");
    upsert_inventory_item(&mut tx, &upsert(1, false))
        .await
        .expect("sync doc 1");
    upsert_inventory_item(&mut tx, &upsert(2, true))
        .await
        .expect("sync doc 2");
    tx.commit().await.expect("commit");
    assert_eq!(complete_flag(&pool, 1).await.0, after_job[0].0);
    assert_eq!(complete_flag(&pool, 2).await.0, after_job[1].0);
}

/// #414: recovering an old stuck run must not overwrite the inventory status
/// of the run the inventory row now points at.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn recovering_a_superseded_run_leaves_inventory_untouched() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 5, false).await;
    // Old run: stuck `running` for an hour with every job settled.
    let old_run: Uuid = sqlx::query_scalar(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages,
                                   created_at, updated_at)
        values (5, 'manual_review', 'test', 'running', '["ocr"]'::jsonb,
                now() - interval '2 hours', now() - interval '1 hour')
        returning id
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("old run");
    sqlx::query(
        "insert into jobs (run_id, paperless_document_id, stage, status) values ($1, 5, 'ocr', 'succeeded')",
    )
    .bind(old_run)
    .execute(&pool)
    .await
    .expect("old job");
    // Newer run that the inventory row now tracks.
    let new_run: Uuid = sqlx::query_scalar(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
        values (5, 'manual_review', 'test', 'failed', '["ocr"]'::jsonb)
        returning id
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("new run");
    sqlx::query(
        "update document_inventory set last_run_id = $1, current_run_status = 'failed' where paperless_document_id = 5",
    )
    .bind(new_run)
    .execute(&pool)
    .await
    .expect("point inventory at new run");

    let summary = recover_stuck_runs(&pool, 600, Uuid::now_v7())
        .await
        .expect("recover stuck runs");
    assert_eq!(
        summary.stuck_runs_completed, 1,
        "old run is still recovered"
    );
    assert_eq!(
        complete_flag(&pool, 5).await,
        (false, Some("failed".to_owned())),
        "inventory keeps the newer run's status"
    );
}

/// #410: `queue_missing_stage` treats `rejected` as terminal, like
/// `stage_needs_work` does.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn queue_missing_stage_skips_rejected_documents() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 1, false).await;
    seed_inventory(&pool, 2, false).await;
    sqlx::query(
        "update document_inventory set ocr_status = 'rejected' where paperless_document_id = 1",
    )
    .execute(&pool)
    .await
    .expect("reject doc 1");
    let created = queue_missing_stage(
        &pool,
        Stage::Ocr,
        ProcessingMode::ManualReview,
        "test",
        &WorkflowRules::default(),
        None,
    )
    .await
    .expect("queue missing stage");
    assert_eq!(created, 1);
    let queued: Vec<i32> =
        sqlx::query_scalar("select paperless_document_id from pipeline_runs order by 1")
            .fetch_all(&pool)
            .await
            .expect("runs");
    assert_eq!(queued, vec![2]);
}
