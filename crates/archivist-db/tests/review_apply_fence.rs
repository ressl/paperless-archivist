//! DB-required integration test: the review-apply path claims a row into the
//! intermediate `applying` status so two concurrent applies (or an operator
//! racing the autopilot drain) cannot both PATCH Paperless. Also covers the
//! stranded-`applying` recovery sweep. #253.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::Stage;
use archivist_db::{
    ApplyIntentInput, DbPool, claim_pending_review_for_autopilot_drain, claim_review_for_apply,
    connect, fail_apply_intent, finalize_apply_intent, get_apply_intent,
    list_recoverable_review_apply_intents, mark_apply_intent_confirmed,
    mark_apply_intent_in_flight, migrate, prepare_apply_intent, reset_stale_applying_reviews,
    revert_review_from_applying,
};
use serde_json::json;
use sqlx::Executor;
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
        r#"truncate paperless_apply_intents, review_items, pipeline_runs, document_inventory, audit_events, metrics_counters restart identity cascade;"#,
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
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
        review_revert_status: Some("approved".to_owned()),
    }
}

async fn seed_review(pool: &DbPool, status: &str) -> Uuid {
    let run_id: Uuid = sqlx::query_scalar(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
        values (1, 'full_auto', 'ai-process', 'waiting_review', '[]'::jsonb)
        returning id
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("insert run");
    sqlx::query_scalar(
        r#"
        insert into review_items (run_id, paperless_document_id, stage, status, suggested_patch, validation_warnings)
        values ($1, 1, $2, $3, '{"title":"x"}'::jsonb, '[]'::jsonb)
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

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn concurrent_apply_claims_are_mutually_exclusive() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "approved").await;

    // First claim wins and carries the prior status; second sees nothing.
    let first = claim_review_for_apply(&pool, review_id)
        .await
        .expect("first claim");
    let second = claim_review_for_apply(&pool, review_id)
        .await
        .expect("second claim");
    assert!(first.is_some());
    assert_eq!(first.unwrap().status, "approved");
    assert!(
        second.is_none(),
        "second concurrent apply must be fenced out"
    );

    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "applying");

    // A failed PATCH reverts to the prior status for retry.
    revert_review_from_applying(&pool, review_id, "approved")
        .await
        .expect("revert");
    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "approved");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn autopilot_drain_claims_into_applying_and_blocks_human_apply() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "pending").await;

    // The drain claims pending -> applying; a human apply cannot then claim it.
    let drained = claim_pending_review_for_autopilot_drain(&pool, review_id)
        .await
        .expect("drain claim");
    assert!(drained.is_some());
    let human = claim_review_for_apply(&pool, review_id)
        .await
        .expect("human claim");
    assert!(human.is_none(), "human apply must not race the drain");

    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "applying");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn stale_applying_rows_are_recovered() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "approved").await;
    claim_review_for_apply(&pool, review_id)
        .await
        .expect("claim");
    // Backdate the claim so it counts as stranded.
    sqlx::query(
        "update review_items set reviewed_at = now() - interval '10 minutes' where id = $1",
    )
    .bind(review_id)
    .execute(&pool)
    .await
    .expect("backdate");

    let recovered = reset_stale_applying_reviews(&pool, 300)
        .await
        .expect("sweep");
    assert_eq!(recovered, 1);
    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "pending");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn apply_intent_is_stable_and_transitions_are_idempotent() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "approved").await;
    let input = intent_input(review_id);

    let first = prepare_apply_intent(&pool, &input).await.expect("prepare");
    let repeated = prepare_apply_intent(&pool, &input)
        .await
        .expect("repeat prepare");
    assert_eq!(first.attempt_id, repeated.attempt_id);
    assert_eq!(first.state, "prepared");
    assert_eq!(first.owner_id, "operator-1");

    assert!(
        mark_apply_intent_in_flight(&pool, first.attempt_id, "operator-1")
            .await
            .expect("start")
    );
    assert!(
        !mark_apply_intent_in_flight(&pool, first.attempt_id, "operator-1")
            .await
            .expect("repeat start")
    );
    assert!(
        mark_apply_intent_confirmed(
            &pool,
            first.attempt_id,
            "operator-1",
            Some(json!({"title": "x"})),
            12,
        )
        .await
        .expect("confirm")
    );
    assert!(
        !mark_apply_intent_confirmed(
            &pool,
            first.attempt_id,
            "operator-1",
            Some(json!({"title": "x"})),
            12,
        )
        .await
        .expect("repeat confirm")
    );

    let stored = get_apply_intent(&pool, first.attempt_id)
        .await
        .expect("get")
        .expect("intent exists");
    assert_eq!(stored.state, "confirmed");

    let success_counter: i64 =
        sqlx::query_scalar("select value from metrics_counters where name = 'apply_success_total'")
            .fetch_one(&pool)
            .await
            .expect("success counter");
    assert_eq!(success_counter, 1);
    let intent_audits: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type = 'document.patch_intent'",
    )
    .fetch_one(&pool)
    .await
    .expect("intent audit count");
    let confirmed_audits: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type = 'document.patch_confirmed'",
    )
    .fetch_one(&pool)
    .await
    .expect("confirmed audit count");
    assert_eq!((intent_audits, confirmed_audits), (1, 1));

    assert!(
        finalize_apply_intent(&pool, first.attempt_id)
            .await
            .expect("finalize")
    );
    assert!(
        !finalize_apply_intent(&pool, first.attempt_id)
            .await
            .expect("repeat finalize")
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn stale_review_recovery_never_requeues_an_active_intent() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "approved").await;
    claim_review_for_apply(&pool, review_id)
        .await
        .expect("claim");
    sqlx::query(
        "update review_items set reviewed_at = now() - interval '10 minutes' where id = $1",
    )
    .bind(review_id)
    .execute(&pool)
    .await
    .expect("backdate");
    let intent = prepare_apply_intent(&pool, &intent_input(review_id))
        .await
        .expect("prepare");

    assert_eq!(
        reset_stale_applying_reviews(&pool, 300)
            .await
            .expect("protected sweep"),
        0
    );
    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "applying");

    assert!(
        fail_apply_intent(&pool, intent.attempt_id, "operator-1", "definite failure")
            .await
            .expect("fail intent")
    );
    assert_eq!(
        reset_stale_applying_reviews(&pool, 300)
            .await
            .expect("terminal sweep"),
        1
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn review_recovery_selects_only_the_newest_fallback_intent() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    let review_id = seed_review(&pool, "approved").await;
    claim_review_for_apply(&pool, review_id)
        .await
        .expect("claim review");
    let first = prepare_apply_intent(&pool, &intent_input(review_id))
        .await
        .expect("prepare first intent");
    fail_apply_intent(
        &pool,
        first.attempt_id,
        "operator-1",
        "custom field rejected",
    )
    .await
    .expect("fail first intent");

    let mut fallback = intent_input(review_id);
    fallback.patch_hash = "sha256:fallback".to_owned();
    fallback.patch = json!({"title": "x"});
    let second = prepare_apply_intent(&pool, &fallback)
        .await
        .expect("prepare fallback intent");

    let recoverable = list_recoverable_review_apply_intents(&pool, 10)
        .await
        .expect("list recoverable");
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].attempt_id, second.attempt_id);
}
