//! DB-required integration tests for #343: sibling review items form one
//! aggregate lifecycle for their shared job.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, claim_jobs, claim_review_for_apply, connect, create_review_item,
    create_run_with_jobs_with_priority, mark_review_applied, migrate, review_decision,
};
use serde_json::json;
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

struct Fixture {
    _guard: MutexGuard<'static, ()>,
    pool: DbPool,
    run_id: Uuid,
    job_id: Uuid,
    review_ids: Vec<Uuid>,
    actor_id: Uuid,
}

async fn fixture() -> Option<Fixture> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate paperless_apply_intents, review_items, jobs, pipeline_runs,
                 document_inventory, audit_events, metrics_counters, users
        restart identity cascade
        "#,
    )
    .await
    .expect("truncate aggregate tables");
    sqlx::query(
        "insert into document_inventory (paperless_document_id, current_tags) values (343, '{}')",
    )
    .execute(&pool)
    .await
    .expect("seed inventory");
    let actor_id = Uuid::now_v7();
    sqlx::query(
        "insert into users (id, username, password_hash) values ($1, 'review-aggregate-actor', 'test')",
    )
    .bind(actor_id)
    .execute(&pool)
    .await
    .expect("seed actor");
    let run_id = create_run_with_jobs_with_priority(
        &pool,
        343,
        &[Stage::Ocr, Stage::Metadata],
        ProcessingMode::ManualReview,
        "test",
        "test",
        Some(0),
    )
    .await
    .expect("create two-stage run");
    let jobs = claim_jobs(&pool, 1, "aggregate-worker", 300)
        .await
        .expect("claim first-stage job");
    let job = jobs.first().expect("OCR job");
    let mut review_ids = Vec::new();
    for index in 0..3 {
        review_ids.push(
            create_review_item(
                &pool,
                job,
                json!({"content": format!("candidate-{index}")}),
                json!([]),
                "aggregate-worker",
            )
            .await
            .expect("create sibling review")
            .expect("review ID"),
        );
    }
    Some(Fixture {
        _guard: guard,
        pool,
        run_id,
        job_id: job.id,
        review_ids,
        actor_id,
    })
}

async fn apply_review(pool: &DbPool, review_id: Uuid, actor_id: Uuid) {
    review_decision(pool, review_id, "approved", None, actor_id)
        .await
        .expect("approve review");
    claim_review_for_apply(pool, review_id)
        .await
        .expect("claim review")
        .expect("applyable review");
    mark_review_applied(pool, review_id, actor_id)
        .await
        .expect("mark applied");
}

async fn assert_aggregate_open(fixture: &Fixture) {
    let job_status: String = sqlx::query_scalar("select status from jobs where id = $1")
        .bind(fixture.job_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("job status");
    let run_status: String = sqlx::query_scalar("select status from pipeline_runs where id = $1")
        .bind(fixture.run_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("run status");
    let row = sqlx::query(
        "select needs_review, complete from document_inventory where paperless_document_id = 343",
    )
    .fetch_one(&fixture.pool)
    .await
    .expect("inventory status");
    let needs_review: bool = row.try_get("needs_review").expect("needs_review");
    let complete: bool = row.try_get("complete").expect("complete");
    assert_eq!(job_status, "waiting_review");
    assert_eq!(run_status, "waiting_review");
    assert!(needs_review);
    assert!(!complete);
    assert!(
        claim_jobs(&fixture.pool, 1, "next-stage-worker", 300)
            .await
            .expect("try claim next stage")
            .is_empty(),
        "next stage must remain fenced by the review aggregate"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn reject_first_then_mixed_result_waits_for_every_sibling() {
    let Some(fixture) = fixture().await else {
        return;
    };
    review_decision(
        &fixture.pool,
        fixture.review_ids[0],
        "rejected",
        None,
        fixture.actor_id,
    )
    .await
    .expect("reject first review");
    assert_aggregate_open(&fixture).await;

    apply_review(&fixture.pool, fixture.review_ids[1], fixture.actor_id).await;
    assert_aggregate_open(&fixture).await;

    apply_review(&fixture.pool, fixture.review_ids[2], fixture.actor_id).await;
    let job_status: String = sqlx::query_scalar("select status from jobs where id = $1")
        .bind(fixture.job_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("job status");
    let row = sqlx::query(
        r#"
        select current_run_status, needs_review, ocr_status
          from document_inventory where paperless_document_id = 343
        "#,
    )
    .fetch_one(&fixture.pool)
    .await
    .expect("inventory status");
    let run_status: String = row.try_get("current_run_status").expect("run status");
    let needs_review: bool = row.try_get("needs_review").expect("needs_review");
    let stage_status: String = row.try_get("ocr_status").expect("stage status");
    assert_eq!(job_status, "succeeded");
    assert_eq!(run_status, "queued");
    assert!(!needs_review);
    assert_eq!(stage_status, "succeeded");
    let next = claim_jobs(&fixture.pool, 1, "next-stage-worker", 300)
        .await
        .expect("claim next stage");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].stage, Stage::Metadata);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn all_rejected_finalizes_only_after_the_last_review() {
    let Some(fixture) = fixture().await else {
        return;
    };
    for review_id in &fixture.review_ids[..2] {
        review_decision(
            &fixture.pool,
            *review_id,
            "rejected",
            None,
            fixture.actor_id,
        )
        .await
        .expect("reject review");
        assert_aggregate_open(&fixture).await;
    }
    review_decision(
        &fixture.pool,
        fixture.review_ids[2],
        "rejected",
        None,
        fixture.actor_id,
    )
    .await
    .expect("reject final review");

    let job_status: String = sqlx::query_scalar("select status from jobs where id = $1")
        .bind(fixture.job_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("job status");
    let run_status: String = sqlx::query_scalar("select status from pipeline_runs where id = $1")
        .bind(fixture.run_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("run status");
    let row = sqlx::query(
        r#"
        select current_run_status, needs_review, ocr_status
          from document_inventory where paperless_document_id = 343
        "#,
    )
    .fetch_one(&fixture.pool)
    .await
    .expect("inventory status");
    let inventory_status: String = row.try_get("current_run_status").expect("inventory status");
    let needs_review: bool = row.try_get("needs_review").expect("needs_review");
    let stage_status: String = row.try_get("ocr_status").expect("stage status");
    assert_eq!(job_status, "cancelled");
    assert_eq!(run_status, "rejected");
    assert_eq!(inventory_status, "rejected");
    assert!(!needs_review);
    assert_eq!(stage_status, "rejected");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn concurrent_last_applies_finalize_the_aggregate_once() {
    let Some(fixture) = fixture().await else {
        return;
    };
    apply_review(&fixture.pool, fixture.review_ids[0], fixture.actor_id).await;
    assert_aggregate_open(&fixture).await;
    for review_id in &fixture.review_ids[1..] {
        review_decision(
            &fixture.pool,
            *review_id,
            "approved",
            None,
            fixture.actor_id,
        )
        .await
        .expect("approve review");
        claim_review_for_apply(&fixture.pool, *review_id)
            .await
            .expect("claim review")
            .expect("applyable review");
    }

    let left_pool = fixture.pool.clone();
    let right_pool = fixture.pool.clone();
    let actor_id = fixture.actor_id;
    let (left, right) = tokio::join!(
        mark_review_applied(&left_pool, fixture.review_ids[1], actor_id),
        mark_review_applied(&right_pool, fixture.review_ids[2], actor_id)
    );
    left.expect("left final apply");
    right.expect("right final apply");

    let aggregate_audits: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type = 'review.aggregate_finalized'",
    )
    .fetch_one(&fixture.pool)
    .await
    .expect("aggregate audit count");
    assert_eq!(aggregate_audits, 1);
    let statuses: Vec<String> =
        sqlx::query_scalar("select status from review_items where job_id = $1 order by id")
            .bind(fixture.job_id)
            .fetch_all(&fixture.pool)
            .await
            .expect("review statuses");
    assert_eq!(statuses, vec!["applied", "applied", "applied"]);
}
