//! DB-required integration test: `release_scheduled_retries` wakes queued jobs
//! whose `run_after` a provider cooldown pushed into the future, without
//! touching already-eligible jobs or the retry budget; the scoped
//! `release_cooldown_parked_retries` additionally leaves regular backoff
//! retries (inside the 40-minute horizon) on their schedule (#313).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, connect, create_run_with_jobs, migrate, release_cooldown_parked_retries,
    release_scheduled_retries,
};
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard};

/// Both tests truncate the shared jobs/runs tables and then assert on their
/// global contents; run in parallel they race each other's truncate. Serialize
/// them on a shared lock (held for the whole test via the returned guard).
static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
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
    Some((guard, pool))
}

async fn seed_queued_runs(pool: &DbPool, document_ids: std::ops::RangeInclusive<i32>) {
    for document_id in document_ids {
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
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn release_scheduled_retries_wakes_future_queued_jobs_only() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    // Four queued runs, each one OCR job (queued, run_after ~ now()).
    seed_queued_runs(&pool, 1..=4).await;

    // Simulate a cooldown defer: push three jobs' run_after a day out and bump
    // their attempts, leaving one eligible now.
    sqlx::query(
        r#"
        update jobs
           set run_after = now() + interval '1 day', attempts = 2
         where paperless_document_id in (1, 2, 3)
        "#,
    )
    .execute(&pool)
    .await
    .expect("defer three jobs");

    let released = release_scheduled_retries(&pool)
        .await
        .expect("release scheduled retries");
    assert_eq!(released, 3, "only the three future-dated jobs are released");

    // None remain parked in the future, and the retry budget is preserved
    // (this reschedules, it does not reset attempts).
    let future_remaining: i64 = sqlx::query(
        "select count(*)::bigint as cnt from jobs where status = 'queued' and run_after > now()",
    )
    .fetch_one(&pool)
    .await
    .expect("count future jobs")
    .try_get("cnt")
    .expect("read count");
    assert_eq!(future_remaining, 0, "no queued job is left parked");

    let preserved_attempts: i64 = sqlx::query(
        "select count(*)::bigint as cnt from jobs where paperless_document_id in (1,2,3) and attempts = 2",
    )
    .fetch_one(&pool)
    .await
    .expect("count preserved attempts")
    .try_get("cnt")
    .expect("read count");
    assert_eq!(preserved_attempts, 3, "attempts are preserved, not reset");

    // A second call is a no-op now that nothing is parked.
    let released_again = release_scheduled_retries(&pool)
        .await
        .expect("release again");
    assert_eq!(released_again, 0, "idempotent once the queue is drained");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn release_cooldown_parked_retries_spares_regular_backoff_retries() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    // Three queued runs: doc 1 sits on a regular transient backoff (well
    // inside fail_job's 2400 s horizon), doc 2 is parked by a multi-hour
    // provider cooldown, doc 3 is eligible now.
    seed_queued_runs(&pool, 1..=3).await;
    sqlx::query(
        "update jobs set run_after = now() + interval '10 minutes' where paperless_document_id = 1",
    )
    .execute(&pool)
    .await
    .expect("schedule backoff retry");
    sqlx::query(
        "update jobs set run_after = now() + interval '6 hours' where paperless_document_id = 2",
    )
    .execute(&pool)
    .await
    .expect("park job on cooldown expiry");

    let released = release_cooldown_parked_retries(&pool)
        .await
        .expect("release cooldown-parked retries");
    assert_eq!(released, 1, "only the cooldown-parked job is woken");

    let backoff_kept: bool = sqlx::query(
        "select run_after > now() + interval '5 minutes' as kept from jobs where paperless_document_id = 1",
    )
    .fetch_one(&pool)
    .await
    .expect("read backoff job")
    .try_get("kept")
    .expect("read kept flag");
    assert!(backoff_kept, "the regular backoff retry keeps its schedule");

    let cooldown_released: bool = sqlx::query(
        "select run_after <= now() as released from jobs where paperless_document_id = 2",
    )
    .fetch_one(&pool)
    .await
    .expect("read cooldown job")
    .try_get("released")
    .expect("read released flag");
    assert!(
        cooldown_released,
        "the cooldown-parked job is claimable now"
    );
}
