//! PostgreSQL contract for #344: optimistic-concurrency conflicts remain
//! operator-visible and never finalize the review aggregate.

use archivist_core::{ProcessingMode, Stage};
use archivist_db::{
    DbPool, claim_jobs, claim_review_for_apply, connect, create_review_item,
    create_run_with_jobs_with_priority, mark_review_apply_conflict, migrate, review_decision,
};
use serde_json::{Value, json};
use sqlx::{Executor, Row};
use uuid::Uuid;

async fn fixture() -> Option<(DbPool, Uuid, Uuid, Uuid, Uuid)> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate paperless_apply_intents, review_items, jobs, pipeline_runs,
                 document_inventory, audit_events, users restart identity cascade
        "#,
    )
    .await
    .expect("truncate conflict tables");
    sqlx::query(
        "insert into document_inventory (paperless_document_id, current_tags) values (344, '{1,2}')",
    )
    .execute(&pool)
    .await
    .expect("seed inventory");
    let actor_id = Uuid::now_v7();
    sqlx::query(
        "insert into users (id, username, password_hash) values ($1, 'review-conflict-actor', 'test')",
    )
    .bind(actor_id)
    .execute(&pool)
    .await
    .expect("seed actor");
    let run_id = create_run_with_jobs_with_priority(
        &pool,
        344,
        &[Stage::Metadata],
        ProcessingMode::ManualReview,
        "test",
        "test",
        Some(0),
    )
    .await
    .expect("create run");
    let job = claim_jobs(&pool, 1, "conflict-worker", 300)
        .await
        .expect("claim job")
        .remove(0);
    let review_id = create_review_item(
        &pool,
        &job,
        json!({"title": "review title"}),
        json!([]),
        json!({"title": "sha256:baseline", "tags": [1, 2]}),
        "conflict-worker",
    )
    .await
    .expect("create review")
    .expect("review id");
    Some((pool, run_id, job.id, review_id, actor_id))
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn conflict_reopens_review_without_finalizing_job_or_run() {
    let Some((pool, run_id, job_id, review_id, actor_id)) = fixture().await else {
        return;
    };
    review_decision(&pool, review_id, "approved", None, actor_id)
        .await
        .expect("approve review");
    let claimed = claim_review_for_apply(&pool, review_id)
        .await
        .expect("claim review")
        .expect("applyable review");
    assert_eq!(claimed.baseline["tags"], json!([1, 2]));

    mark_review_apply_conflict(
        &pool,
        review_id,
        "pending",
        &["title".to_owned()],
        "user",
        Some(actor_id.to_string()),
    )
    .await
    .expect("record conflict");

    let review = sqlx::query(
        "select status, conflict_fields, conflicted_at from review_items where id = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("review state");
    assert_eq!(review.try_get::<String, _>("status").unwrap(), "pending");
    assert_eq!(
        review.try_get::<Value, _>("conflict_fields").unwrap(),
        json!(["title"])
    );
    assert!(
        review
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("conflicted_at")
            .unwrap()
            .is_some()
    );

    let job_status: String = sqlx::query_scalar("select status from jobs where id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let run_status: String = sqlx::query_scalar("select status from pipeline_runs where id = $1")
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let inventory = sqlx::query(
        "select needs_review, complete from document_inventory where paperless_document_id = 344",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(job_status, "waiting_review");
    assert_eq!(run_status, "waiting_review");
    assert!(inventory.try_get::<bool, _>("needs_review").unwrap());
    assert!(!inventory.try_get::<bool, _>("complete").unwrap());

    let audit = sqlx::query(
        r#"
        select before, after, metadata, outcome
          from audit_events
         where event_type = 'review.apply_conflict' and paperless_document_id = 344
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("conflict audit");
    assert!(
        audit
            .try_get::<Option<Value>, _>("before")
            .unwrap()
            .is_none()
    );
    assert!(
        audit
            .try_get::<Option<Value>, _>("after")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        audit.try_get::<Value, _>("metadata").unwrap(),
        json!({
            "review_id": review_id,
            "fields": ["title"]
        })
    );
    assert_eq!(audit.try_get::<String, _>("outcome").unwrap(), "failed");
}
