//! DB-required integration tests for the document_inventory.current_run_status
//! mirror discipline (#303).
//!
//! `current_run_status` is a hand-maintained mirror of `pipeline_runs.status`.
//! The pre-#303 provider-cooldown lease release only updated `jobs`, which
//! stranded runs on `running`, and the startup repair
//! `reset_stuck_running_pipeline_runs` then flipped those runs back WITHOUT
//! mirroring — leaving ~10% of production inventory rows with a stale
//! `running` badge. These tests pin the fixed behavior: after both repair
//! paths, every inventory row agrees with the run its `last_run_id` points at.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, claim_jobs, connect, create_run_with_jobs, migrate, release_job_lease_for_cooldown,
    reset_stuck_running_pipeline_runs,
};
use chrono::{Duration, Utc};
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
        r#"
        truncate document_inventory, jobs, pipeline_runs, audit_events restart identity cascade;
        "#,
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
}

async fn run_status(pool: &DbPool, run_id: Uuid) -> String {
    sqlx::query("select status from pipeline_runs where id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("fetch run status")
        .try_get("status")
        .expect("read run status")
}

async fn inventory_run_status(pool: &DbPool, document_id: i32) -> Option<String> {
    sqlx::query(
        "select current_run_status from document_inventory where paperless_document_id = $1",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("fetch inventory row")
    .try_get("current_run_status")
    .expect("read current_run_status")
}

/// The #303 invariant: no inventory row may disagree with the run its
/// `last_run_id` points at.
async fn drifted_mirror_rows(pool: &DbPool) -> i64 {
    sqlx::query(
        r#"
        select count(*)::bigint as cnt
          from document_inventory di
          join pipeline_runs pr on pr.id = di.last_run_id
         where di.current_run_status is distinct from pr.status
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("count drifted mirror rows")
    .try_get("cnt")
    .expect("read drift count")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn cooldown_lease_release_keeps_run_status_mirror_in_sync() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    let document_id = 101;
    let run_id = create_run_with_jobs(
        &pool,
        document_id,
        &[Stage::Ocr, Stage::Metadata],
        ProcessingMode::ManualReview,
        "test",
        "test",
    )
    .await
    .expect("create run");

    // Claim the OCR job (metadata is blocked behind it by stage_priority).
    let claimed = claim_jobs(&pool, 10, "test-worker", 300)
        .await
        .expect("claim jobs");
    assert_eq!(claimed.len(), 1, "only the OCR job should be claimable");
    let job = &claimed[0];
    assert_eq!(run_status(&pool, run_id).await, "running");
    assert_eq!(
        inventory_run_status(&pool, document_id).await.as_deref(),
        Some("running")
    );

    // Provider cooldown: release the lease without burning an attempt.
    let cooldown_until = Utc::now() + Duration::hours(2);
    let released = release_job_lease_for_cooldown(&pool, job, "test-worker", cooldown_until)
        .await
        .expect("release lease for cooldown");
    assert!(released, "owned lease should release");

    // Job is parked until the cooldown expiry with its attempt refunded.
    let job_row =
        sqlx::query("select status, attempts, lease_owner, run_after from jobs where id = $1")
            .bind(job.id)
            .fetch_one(&pool)
            .await
            .expect("fetch released job");
    assert_eq!(
        job_row.try_get::<String, _>("status").expect("job status"),
        "queued"
    );
    assert_eq!(
        job_row.try_get::<i32, _>("attempts").expect("job attempts"),
        0,
        "claim's attempt increment should be refunded"
    );
    assert_eq!(
        job_row
            .try_get::<Option<String>, _>("lease_owner")
            .expect("lease owner"),
        None
    );
    let run_after = job_row
        .try_get::<chrono::DateTime<Utc>, _>("run_after")
        .expect("run_after");
    assert!(
        (run_after - cooldown_until).num_milliseconds().abs() <= 1,
        "run_after should be parked on the cooldown expiry"
    );

    // The run follows the job back to queued, and the inventory mirror
    // follows the run — this is exactly the pair the pre-#303 code skipped.
    assert_eq!(run_status(&pool, run_id).await, "queued");
    assert_eq!(
        inventory_run_status(&pool, document_id).await.as_deref(),
        Some("queued")
    );
    assert_eq!(drifted_mirror_rows(&pool).await, 0, "no mirror drift");

    // Fence: a worker that no longer owns the lease must be a no-op.
    let released_again = release_job_lease_for_cooldown(&pool, job, "other-worker", cooldown_until)
        .await
        .expect("fenced release");
    assert!(!released_again, "lost lease must not release");
    assert_eq!(run_status(&pool, run_id).await, "queued");
    assert_eq!(drifted_mirror_rows(&pool).await, 0);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn reset_stuck_running_pipeline_runs_mirrors_inventory_status() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    // Two documents reproducing the legacy stuck-running shapes the startup
    // repair targets: a run whose only job went back to 'queued' (the
    // pre-#303 cooldown release) and a run whose jobs all succeeded.
    let queued_doc = 201;
    let queued_run = create_run_with_jobs(
        &pool,
        queued_doc,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "test",
        "test",
    )
    .await
    .expect("create queued-shape run");
    let succeeded_doc = 202;
    let succeeded_run = create_run_with_jobs(
        &pool,
        succeeded_doc,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "test",
        "test",
    )
    .await
    .expect("create succeeded-shape run");

    let claimed = claim_jobs(&pool, 10, "test-worker", 300)
        .await
        .expect("claim jobs");
    assert_eq!(claimed.len(), 2, "both OCR jobs should be claimed");

    // Replay the PRE-fix cooldown release on the first run: only the job
    // moves, the run stays 'running' and the mirror stays 'running'.
    sqlx::query(
        r#"
        update jobs
           set status = 'queued',
               attempts = greatest(attempts - 1, 0),
               lease_owner = null,
               lease_until = null,
               run_after = now(),
               updated_at = now()
         where run_id = $1
        "#,
    )
    .bind(queued_run)
    .execute(&pool)
    .await
    .expect("simulate legacy cooldown release");
    // Settle every job of the second run while its run text stays 'running'.
    sqlx::query(
        r#"
        update jobs
           set status = 'succeeded',
               lease_owner = null,
               lease_until = null,
               updated_at = now()
         where run_id = $1
        "#,
    )
    .bind(succeeded_run)
    .execute(&pool)
    .await
    .expect("simulate settled jobs");
    assert_eq!(run_status(&pool, queued_run).await, "running");
    assert_eq!(run_status(&pool, succeeded_run).await, "running");

    let summary = reset_stuck_running_pipeline_runs(&pool)
        .await
        .expect("reset stuck running runs");
    assert_eq!(summary.runs_reset, 2, "both stuck runs should be repaired");

    // The runs are repaired AND the inventory mirror follows them — the
    // pre-#303 helper fixed the runs only, which is what drifted ~10% of
    // production inventory rows to a stale 'running'.
    assert_eq!(run_status(&pool, queued_run).await, "queued");
    assert_eq!(
        inventory_run_status(&pool, queued_doc).await.as_deref(),
        Some("queued")
    );
    assert_eq!(run_status(&pool, succeeded_run).await, "succeeded");
    assert_eq!(
        inventory_run_status(&pool, succeeded_doc).await.as_deref(),
        Some("succeeded")
    );
    assert_eq!(drifted_mirror_rows(&pool).await, 0, "no mirror drift");
}
