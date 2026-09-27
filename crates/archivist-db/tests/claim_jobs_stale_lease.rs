//! DB-required integration tests for the stale-lease handling (#402) and the
//! index-backed claim passes (#412) of `claim_jobs`.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    ClaimPass, DbPool, claim_jobs, claim_jobs_candidate_sql, connect, create_run_with_jobs, migrate,
};
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
        r#"
        truncate document_inventory, jobs, pipeline_runs, audit_events restart identity cascade;
        "#,
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
}

async fn seed_run(pool: &DbPool, document_id: i32) -> Uuid {
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
        &[Stage::Ocr, Stage::Metadata],
        ProcessingMode::ManualReview,
        "test",
        "test",
    )
    .await
    .expect("create run")
}

/// Simulate a worker that claimed the OCR job and died (OOM) mid-job.
async fn crash_ocr_job(pool: &DbPool, run_id: Uuid, attempts: i32) -> Uuid {
    sqlx::query_scalar(
        r#"
        update jobs
           set status = 'running',
               lease_owner = 'dead-worker',
               lease_until = now() - interval '1 minute',
               attempts = $2,
               max_attempts = 3
         where run_id = $1 and stage = 'ocr'
        returning id
        "#,
    )
    .bind(run_id)
    .bind(attempts)
    .fetch_one(pool)
    .await
    .expect("crash ocr job")
}

async fn job_status(pool: &DbPool, job_id: Uuid) -> (String, Option<String>) {
    let row = sqlx::query("select status, error_message from jobs where id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .expect("job status");
    (row.get("status"), row.get("error_message"))
}

/// #402: a job whose lease expired after its final attempt is failed (with
/// run/inventory/siblings following) instead of being reclaimed again, while a
/// crashed job with budget left is still reclaimed.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn claim_jobs_fails_exhausted_stale_leases_instead_of_reclaiming() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let exhausted_run = seed_run(&pool, 1).await;
    let exhausted_job = crash_ocr_job(&pool, exhausted_run, 3).await;
    let retry_run = seed_run(&pool, 2).await;
    let retry_job = crash_ocr_job(&pool, retry_run, 1).await;

    let claimed = claim_jobs(&pool, 10, "live-worker", 300)
        .await
        .expect("claim jobs");
    let claimed_ids: Vec<Uuid> = claimed.iter().map(|job| job.id).collect();
    assert_eq!(
        claimed_ids,
        vec![retry_job],
        "only the job with budget left is reclaimed"
    );
    assert_eq!(claimed[0].attempts, 2);

    let (status, error) = job_status(&pool, exhausted_job).await;
    assert_eq!(status, "failed");
    assert!(
        error
            .unwrap_or_default()
            .contains("lease expired after attempt 3 of 3"),
        "failure carries an explanatory error message"
    );
    let run_status: String = sqlx::query_scalar("select status from pipeline_runs where id = $1")
        .bind(exhausted_run)
        .fetch_one(&pool)
        .await
        .expect("run status");
    assert_eq!(run_status, "failed");
    let sibling_status: String =
        sqlx::query_scalar("select status from jobs where run_id = $1 and stage = 'metadata'")
            .bind(exhausted_run)
            .fetch_one(&pool)
            .await
            .expect("sibling status");
    assert_eq!(sibling_status, "cancelled");
    let inventory = sqlx::query(
        "select ocr_status, current_run_status from document_inventory where paperless_document_id = 1",
    )
    .fetch_one(&pool)
    .await
    .expect("inventory");
    assert_eq!(inventory.get::<String, _>("ocr_status"), "failed");
    assert_eq!(
        inventory
            .get::<Option<String>, _>("current_run_status")
            .as_deref(),
        Some("failed")
    );
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type = 'job.failed' and job_id = $1",
    )
    .bind(exhausted_job)
    .fetch_one(&pool)
    .await
    .expect("audit count");
    assert_eq!(audited, 1);

    // A second poll must not touch the failed job again.
    let claimed = claim_jobs(&pool, 10, "live-worker", 300)
        .await
        .expect("claim jobs again");
    assert!(claimed.iter().all(|job| job.id != exhausted_job));
}

/// #412: queued retries still jump ahead of fresh work, and each claim pass
/// stays within its budget.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn claim_jobs_keeps_retry_bias_across_passes() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let fresh_run = seed_run(&pool, 10).await;
    let retry_run = seed_run(&pool, 11).await;
    // Give the fresh run the better priority so only the retry bias can put
    // the retry first.
    sqlx::query(
        "update jobs set payload = jsonb_set(payload, '{priority}', '1') where run_id = $1",
    )
    .bind(fresh_run)
    .execute(&pool)
    .await
    .expect("prioritise fresh run");
    sqlx::query(
        r#"
        update jobs
           set attempts = 1,
               error_message = 'transient',
               run_after = now() - interval '1 second',
               payload = jsonb_set(payload, '{priority}', '500')
         where run_id = $1 and stage = 'ocr'
        "#,
    )
    .bind(retry_run)
    .execute(&pool)
    .await
    .expect("mark retry");

    let claimed = claim_jobs(&pool, 1, "worker", 300).await.expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].run_id, retry_run, "retry bias wins");

    let claimed = claim_jobs(&pool, 5, "worker", 300).await.expect("claim");
    assert_eq!(
        claimed.len(),
        1,
        "metadata jobs stay behind their OCR stage"
    );
    assert_eq!(claimed[0].run_id, fresh_run);
}

async fn explain(pool: &DbPool, pass: ClaimPass) -> String {
    let sql = claim_jobs_candidate_sql(pass).replace("$1", "10");
    let mut tx = pool.begin().await.expect("begin");
    // Disable the alternatives so the planner reveals whether an index can
    // deliver the ORDER BY at all: a disabled sort that is still needed would
    // show up as a `Sort` node.
    tx.execute("set local enable_seqscan = off")
        .await
        .expect("disable seqscan");
    tx.execute("set local enable_sort = off")
        .await
        .expect("disable sort");
    tx.execute("set local enable_bitmapscan = off")
        .await
        .expect("disable bitmapscan");
    let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("explain {sql}")))
        .fetch_all(&mut *tx)
        .await
        .expect("explain claim pass");
    tx.rollback().await.expect("rollback");
    rows.join("\n")
}

/// #412 acceptance: EXPLAIN shows every claim pass walking an index in ORDER BY
/// order (no Sort node), so a poll no longer sorts the whole backlog.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn claim_job_passes_use_ordered_indexes() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    for (pass, index) in [
        (ClaimPass::Queued, "idx_jobs_claim on jobs"),
        (ClaimPass::Retry, "idx_jobs_claim_retry"),
        (ClaimPass::StaleLease, "jobs_lease_until_idx"),
    ] {
        let plan = explain(&pool, pass).await;
        assert!(
            plan.contains(&format!("Index Scan using {index}")),
            "{pass:?} must scan {index}:\n{plan}"
        );
        assert!(!plan.contains("Sort"), "{pass:?} must not sort:\n{plan}");
    }
}
