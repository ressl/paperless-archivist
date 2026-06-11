//! DB-required integration tests for apply_security_retention:
//!   * ocr_page_cache rows must not outlive the artifact retention (full OCR
//!     page text);
//!   * terminal pipeline_runs past runs_retention_days are pruned — jobs go
//!     with them via CASCADE, review history survives with run_id nulled,
//!     active runs and recent terminal runs stay (#310).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::RuntimeSettings;
use archivist_db::{DbPool, apply_security_retention, connect, migrate};
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

/// The tests in this binary truncate shared tables and then assert on their
/// global contents; run in parallel they race each other's truncate. Serialize
/// them on a shared lock (held for the whole test via the returned guard).
static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"truncate ocr_page_cache, ai_artifacts, review_items, jobs, pipeline_runs,
                    document_inventory, audit_events restart identity cascade;"#,
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn retention_prunes_expired_ocr_page_cache_rows() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    sqlx::query(
        r#"
        insert into ocr_page_cache (paperless_document_id, page_index, page_hash, ocr_text, created_at)
        values (1, 0, 'hash-old', 'old page text', now() - interval '60 days'),
               (1, 1, 'hash-new', 'new page text', now())
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed cache rows");

    let mut settings = RuntimeSettings::default();
    settings.security.ai_artifact_retention_days = 30;
    settings.security.audit_retention_days = 30;

    let result = apply_security_retention(&pool, &settings, Uuid::now_v7())
        .await
        .expect("apply retention");
    assert_eq!(result.ocr_page_cache_deleted, 1);

    let remaining: i64 = sqlx::query_scalar("select count(*) from ocr_page_cache")
        .fetch_one(&pool)
        .await
        .expect("count cache rows");
    assert_eq!(remaining, 1);
}

async fn seed_run(pool: &DbPool, document_id: i32, status: &str, age_days: i32) -> Uuid {
    sqlx::query_scalar(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages, created_at, updated_at)
        values ($1, 'full_auto', 'ai-process', $2, '["ocr"]'::jsonb,
                now() - make_interval(days => $3), now() - make_interval(days => $3))
        returning id
        "#,
    )
    .bind(document_id)
    .bind(status)
    .bind(age_days)
    .fetch_one(pool)
    .await
    .expect("insert pipeline run")
}

async fn seed_job(pool: &DbPool, run_id: Uuid, document_id: i32, status: &str) -> Uuid {
    sqlx::query_scalar(
        r#"
        insert into jobs (run_id, paperless_document_id, stage, status, payload)
        values ($1, $2, 'ocr', $3, '{}'::jsonb)
        returning id
        "#,
    )
    .bind(run_id)
    .bind(document_id)
    .bind(status)
    .fetch_one(pool)
    .await
    .expect("insert job")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn retention_prunes_terminal_runs_but_keeps_reviews_and_active_runs() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    // Old terminal run: must be pruned, its job CASCADEs away, its review
    // item survives with run_id nulled.
    let old_done = seed_run(&pool, 1, "succeeded", 400).await;
    seed_job(&pool, old_done, 1, "succeeded").await;
    let review_id: Uuid = sqlx::query_scalar(
        r#"
        insert into review_items (run_id, paperless_document_id, stage, status, suggested_patch)
        values ($1, 1, 'metadata', 'applied', '{}'::jsonb)
        returning id
        "#,
    )
    .bind(old_done)
    .fetch_one(&pool)
    .await
    .expect("insert review item");

    // Old but still active run: never pruned regardless of age.
    let old_active = seed_run(&pool, 2, "waiting_review", 400).await;
    seed_job(&pool, old_active, 2, "waiting_review").await;

    // Recent terminal run: inside the retention window, stays.
    let recent_done = seed_run(&pool, 3, "failed", 5).await;

    // Inventory row pointing at the pruned run: last_run_id nulls out via the
    // FK added in migration 0041 instead of dangling.
    sqlx::query(
        r#"
        insert into document_inventory (paperless_document_id, current_tags, last_run_id)
        values (1, '{}', $1)
        "#,
    )
    .bind(old_done)
    .execute(&pool)
    .await
    .expect("seed inventory row");

    let mut settings = RuntimeSettings::default();
    settings.security.runs_retention_days = 90;

    let result = apply_security_retention(&pool, &settings, Uuid::now_v7())
        .await
        .expect("apply retention");
    assert_eq!(result.pipeline_runs_deleted, 1, "only the old terminal run");

    let remaining_runs: Vec<Uuid> = sqlx::query("select id from pipeline_runs")
        .fetch_all(&pool)
        .await
        .expect("list runs")
        .into_iter()
        .map(|row| row.try_get("id").expect("run id"))
        .collect();
    assert!(
        !remaining_runs.contains(&old_done),
        "old terminal run pruned"
    );
    assert!(remaining_runs.contains(&old_active), "active run survives");
    assert!(remaining_runs.contains(&recent_done), "recent run survives");

    let job_count: i64 = sqlx::query_scalar("select count(*) from jobs where run_id = $1")
        .bind(old_done)
        .fetch_one(&pool)
        .await
        .expect("count cascaded jobs");
    assert_eq!(job_count, 0, "jobs of the pruned run CASCADE away");

    let review_run: Option<Uuid> = sqlx::query("select run_id from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("review item survives")
        .try_get("run_id")
        .expect("read review run_id");
    assert_eq!(
        review_run, None,
        "review history keeps the row, loses the run pointer"
    );

    let last_run: Option<Uuid> =
        sqlx::query("select last_run_id from document_inventory where paperless_document_id = 1")
            .fetch_one(&pool)
            .await
            .expect("inventory row")
            .try_get("last_run_id")
            .expect("read last_run_id");
    assert_eq!(
        last_run, None,
        "inventory pointer nulls out instead of dangling"
    );
}
