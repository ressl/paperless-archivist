//! DB-required integration tests for the 2026-09 API audit fixes:
//! typed review-decision errors (#391), abandoned approved/edited reviews
//! (#388), retryable apply intents (#389), bounded bulk reruns (#390),
//! token rights following the creator's roles (#392) and by-ID pending review
//! lookup (#395).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Role, Stage};
use archivist_db::{
    ApplyIntentInput, CREATE_RUNS_CHUNK_SIZE, DbPool, ReviewDecisionError, connect,
    create_api_token, create_runs_for_documents, create_user_with_roles, fail_apply_intent,
    finalize_failed_review_apply_intents, find_api_token, get_pending_review,
    list_pending_review_items_for_autopilot_drain, list_reviews, mark_apply_intent_in_flight,
    migrate, prepare_apply_intent, release_transient_apply_intent, reset_stale_applying_reviews,
    review_decision, set_user_roles, update_user_password_hash,
};
use serde_json::json;
use sqlx::Executor;
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"truncate paperless_apply_intents, review_items, jobs, pipeline_runs, document_inventory,
                    audit_events, metrics_counters restart identity cascade;"#,
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
}

/// One active run per document is enforced, so every seeded run gets its own
/// document ID.
async fn seed_run(pool: &DbPool) -> Uuid {
    sqlx::query_scalar(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
        values ((select coalesce(max(paperless_document_id), 0) + 1 from pipeline_runs),
                'manual_review', 'ai-process', 'waiting_review', '[]'::jsonb)
        returning id
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("insert run")
}

async fn seed_review(pool: &DbPool, status: &str) -> Uuid {
    let run_id = seed_run(pool).await;
    sqlx::query_scalar(
        r#"
        insert into review_items (run_id, paperless_document_id, stage, status, suggested_patch, validation_warnings, reviewed_at)
        values ($1, (select paperless_document_id from pipeline_runs where id = $1),
                $2, $3, '{"title":"x"}'::jsonb, '[]'::jsonb,
                case when $3 = 'pending' then null else now() end)
        returning id
        "#,
    )
    .bind(run_id)
    .bind(Stage::Metadata.to_string())
    .bind(status)
    .fetch_one(pool)
    .await
    .expect("insert review item")
}

async fn review_status(pool: &DbPool, review_id: Uuid) -> String {
    sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(pool)
        .await
        .expect("review status")
}

/// Users are not truncated (other tables reference them), so every test user
/// gets a unique name.
async fn create_user(pool: &DbPool, username: &str, roles: &[Role]) -> Uuid {
    let username = format!("{username}-{}", Uuid::now_v7().simple());
    create_user_with_roles(pool, &username, None, "test-password-hash", roles, None)
        .await
        .expect("create user")
}

async fn token_last_used(pool: &DbPool, token_hash: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("select last_used_at from api_tokens where token_hash = $1")
        .bind(token_hash)
        .fetch_one(pool)
        .await
        .expect("last used")
}

fn intent_input(review_id: Uuid) -> ApplyIntentInput {
    ApplyIntentInput {
        source: "human_review".to_owned(),
        source_key: format!("review:{review_id}"),
        owner_type: "user".to_owned(),
        owner_id: "operator-1".to_owned(),
        paperless_document_id: 1,
        run_id: None,
        job_id: None,
        review_id: Some(review_id),
        patch_hash: "sha256:stable".to_owned(),
        patch: json!({"title": "x"}),
        before: Some(json!({"title": "old"})),
        metadata: json!({"stage": "metadata"}),
        review_revert_status: Some("pending".to_owned()),
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn review_decision_reports_typed_not_found_and_not_pending() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let actor = create_user(&pool, "reviewer", &[Role::Admin]).await;
    let review_id = seed_review(&pool, "pending").await;

    review_decision(&pool, review_id, "rejected", None, actor)
        .await
        .expect("first decision");
    let twice = review_decision(&pool, review_id, "approved", None, actor)
        .await
        .expect_err("second decision must be rejected");
    assert_eq!(
        twice.downcast_ref::<ReviewDecisionError>(),
        Some(&ReviewDecisionError::NotPending)
    );
    let missing = review_decision(&pool, Uuid::now_v7(), "approved", None, actor)
        .await
        .expect_err("unknown review");
    assert_eq!(
        missing.downcast_ref::<ReviewDecisionError>(),
        Some(&ReviewDecisionError::NotFound)
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn pending_review_is_found_by_id_beyond_the_newest_2000() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let old_review = seed_review(&pool, "pending").await;
    sqlx::query("update review_items set created_at = now() - interval '30 days' where id = $1")
        .bind(old_review)
        .execute(&pool)
        .await
        .expect("backdate old review");
    let run_id = seed_run(&pool).await;
    sqlx::query(
        r#"
        insert into review_items (run_id, paperless_document_id, stage, status, suggested_patch, validation_warnings)
        select $1, 1, 'metadata', 'pending', '{"title":"y"}'::jsonb, '[]'::jsonb
          from generate_series(1, 2001)
        "#,
    )
    .bind(run_id)
    .execute(&pool)
    .await
    .expect("insert newer backlog");

    let newest = list_reviews(&pool, Some("pending"), 2000)
        .await
        .expect("list newest");
    assert!(newest.iter().all(|review| review.id != old_review));
    let found = get_pending_review(&pool, old_review)
        .await
        .expect("lookup")
        .expect("old pending review is found by id");
    assert_eq!(found.id, old_review);

    sqlx::query("update review_items set status = 'rejected' where id = $1")
        .bind(old_review)
        .execute(&pool)
        .await
        .expect("reject");
    assert!(
        get_pending_review(&pool, old_review)
            .await
            .expect("lookup")
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn abandoned_approved_and_edited_reviews_return_to_pending() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let approved = seed_review(&pool, "approved").await;
    let edited = seed_review(&pool, "edited").await;
    let fresh = seed_review(&pool, "approved").await;
    let legacy_failed = seed_review(&pool, "approved").await;
    sqlx::query(
        "update review_items set reviewed_at = now() - interval '10 minutes' where id <> $1",
    )
    .bind(fresh)
    .execute(&pool)
    .await
    .expect("backdate");
    // A failed intent still waiting for the recovery worker keeps its row.
    let intent = prepare_apply_intent(&pool, &intent_input(legacy_failed))
        .await
        .expect("prepare");
    assert!(
        fail_apply_intent(&pool, intent.attempt_id, "operator-1", "definite failure")
            .await
            .expect("fail")
    );

    assert_eq!(
        reset_stale_applying_reviews(&pool, 300)
            .await
            .expect("sweep"),
        2
    );
    assert_eq!(review_status(&pool, approved).await, "pending");
    assert_eq!(review_status(&pool, edited).await, "pending");
    assert_eq!(review_status(&pool, fresh).await, "approved");
    assert_eq!(review_status(&pool, legacy_failed).await, "approved");

    // Once settled, the next sweep releases it too.
    finalize_failed_review_apply_intents(&pool, legacy_failed)
        .await
        .expect("settle");
    assert_eq!(
        reset_stale_applying_reviews(&pool, 300)
            .await
            .expect("sweep"),
        1
    );
    assert_eq!(review_status(&pool, legacy_failed).await, "pending");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn settled_failed_intent_is_reprepared_and_skipped_by_the_drain() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "pending").await;
    let first = prepare_apply_intent(&pool, &intent_input(review_id))
        .await
        .expect("prepare");
    assert!(
        fail_apply_intent(&pool, first.attempt_id, "operator-1", "paperless 400")
            .await
            .expect("fail")
    );

    // A terminally failed review is left for a human; the drain must not
    // pick it up every tick.
    let drainable = list_pending_review_items_for_autopilot_drain(&pool, 10)
        .await
        .expect("drain list");
    assert!(drainable.iter().all(|review| review.id != review_id));

    // Unsettled: still blocked.
    let blocked = prepare_apply_intent(&pool, &intent_input(review_id))
        .await
        .expect("prepare");
    assert_eq!(blocked.state, "failed");

    // Settled: a new decision for the same patch starts a fresh attempt.
    assert_eq!(
        finalize_failed_review_apply_intents(&pool, review_id)
            .await
            .expect("settle"),
        1
    );
    let mut retry_input = intent_input(review_id);
    retry_input.owner_id = "operator-2".to_owned();
    let retry = prepare_apply_intent(&pool, &retry_input)
        .await
        .expect("re-prepare");
    assert_eq!(retry.attempt_id, first.attempt_id);
    assert_eq!(retry.state, "prepared");
    assert_eq!(retry.owner_id, "operator-2");
    assert!(retry.finalized_at.is_none());
    assert!(
        mark_apply_intent_in_flight(&pool, retry.attempt_id, "operator-2")
            .await
            .expect("start retry")
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn transient_intent_release_is_bounded() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "applying").await;
    let intent = prepare_apply_intent(&pool, &intent_input(review_id))
        .await
        .expect("prepare");
    for attempt in 1..=3 {
        assert!(
            mark_apply_intent_in_flight(&pool, intent.attempt_id, "operator-1")
                .await
                .expect("start")
        );
        assert!(
            release_transient_apply_intent(&pool, intent.attempt_id, "operator-1", "503", 3)
                .await
                .expect("release"),
            "attempt {attempt} is within the budget"
        );
    }
    assert!(
        mark_apply_intent_in_flight(&pool, intent.attempt_id, "operator-1")
            .await
            .expect("start")
    );
    assert!(
        !release_transient_apply_intent(&pool, intent.attempt_id, "operator-1", "503", 3)
            .await
            .expect("exhausted"),
        "the retry budget is bounded"
    );
    let retries: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type = 'document.patch_retry_scheduled'",
    )
    .fetch_one(&pool)
    .await
    .expect("retry audits");
    assert_eq!(retries, 3);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn api_token_follows_creator_roles_and_password_changes() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let admin = create_user(&pool, "admin", &[Role::Admin]).await;
    let reviewer = create_user(&pool, "reviewer", &[Role::Reviewer]).await;
    let token_hash = format!("token-hash-{}", Uuid::now_v7().simple());
    let scopes = vec!["reviews:read".to_owned(), "reviews:write".to_owned()];
    create_api_token(&pool, "automation", &token_hash, &scopes, reviewer, None)
        .await
        .expect("create token");
    let principal = find_api_token(&pool, &token_hash)
        .await
        .expect("find")
        .expect("active token");
    assert_eq!(principal.creator_roles, vec![Role::Reviewer]);
    let first_use = token_last_used(&pool, &token_hash).await;
    assert!(first_use.is_some());
    find_api_token(&pool, &token_hash)
        .await
        .expect("find again")
        .expect("active token");
    assert_eq!(
        first_use,
        token_last_used(&pool, &token_hash).await,
        "last_used_at is throttled"
    );

    set_user_roles(&pool, reviewer, &[Role::Viewer], admin)
        .await
        .expect("demote");
    let demoted = find_api_token(&pool, &token_hash)
        .await
        .expect("find")
        .expect("token still exists");
    assert_eq!(demoted.creator_roles, vec![Role::Viewer]);

    update_user_password_hash(
        &pool,
        reviewer,
        "new-hash",
        reviewer,
        "auth.password_changed",
    )
    .await
    .expect("change password");
    assert!(
        find_api_token(&pool, &token_hash)
            .await
            .expect("find")
            .is_none(),
        "a password change revokes the user's API tokens"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn bulk_rerun_is_committed_in_bounded_chunks() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let total = (CREATE_RUNS_CHUNK_SIZE * 2 + 7) as i32;
    let document_ids: Vec<i32> = (1..=total).rev().collect();
    let queued = create_runs_for_documents(
        &pool,
        &document_ids,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "bulk-rerun",
        "test",
        Some(0),
    )
    .await
    .expect("chunked bulk rerun");
    assert_eq!(queued, i64::from(total));
    let runs: i64 = sqlx::query_scalar(
        "select count(distinct paperless_document_id) from pipeline_runs where trigger_tag = 'bulk-rerun'",
    )
    .fetch_one(&pool)
    .await
    .expect("count runs");
    assert_eq!(runs, i64::from(total));

    // Idempotent across chunks: a repeat reuses every active run.
    let again = create_runs_for_documents(
        &pool,
        &document_ids,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "bulk-rerun",
        "test",
        Some(0),
    )
    .await
    .expect("repeat");
    assert_eq!(again, i64::from(total));
    let runs_after: i64 = sqlx::query_scalar("select count(*) from pipeline_runs")
        .fetch_one(&pool)
        .await
        .expect("count runs");
    assert_eq!(runs_after, i64::from(total));
}
