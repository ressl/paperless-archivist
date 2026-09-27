//! DB-required integration tests for #420 (synced metadata options) and #445
//! ("retry with provider/model/prompt" creates a fresh metadata run).

use archivist_core::{METADATA_RETRY_OVERRIDES_KEY, ProcessingMode, Stage};
use archivist_db::{
    DbPool, PaperlessNamedOption, ReviewDecisionError, ReviewRetryOutcome, claim_jobs, connect,
    create_review_item, create_run_with_jobs_with_priority, get_prompt_by_id,
    list_paperless_correspondent_options, list_paperless_document_type_options, migrate,
    retry_review_with_overrides, review_decision, review_document_id,
};
use serde_json::json;
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

const DOCUMENT_ID: i32 = 445;

struct Fixture {
    _guard: MutexGuard<'static, ()>,
    pool: DbPool,
    run_id: Uuid,
    review_ids: Vec<Uuid>,
    actor_id: Uuid,
}

async fn fixture() -> Option<Fixture> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database (DB integration tests share one database: run them serially with `-- --ignored --test-threads=1`, see scripts/verify/migration_smoke.sh)");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate paperless_apply_intents, review_items, jobs, pipeline_runs,
                 document_inventory, audit_events, metrics_counters, users
        restart identity cascade
        "#,
    )
    .await
    .expect("truncate retry tables");
    sqlx::query(
        "insert into document_inventory (paperless_document_id, current_tags) values ($1, '{}')",
    )
    .bind(DOCUMENT_ID)
    .execute(&pool)
    .await
    .expect("seed inventory");
    let actor_id = Uuid::now_v7();
    sqlx::query(
        "insert into users (id, username, password_hash) values ($1, 'review-retry-actor', 'test')",
    )
    .bind(actor_id)
    .execute(&pool)
    .await
    .expect("seed actor");
    let run_id = create_run_with_jobs_with_priority(
        &pool,
        DOCUMENT_ID,
        &[Stage::Metadata],
        ProcessingMode::FullAuto,
        "test",
        "test",
        Some(0),
    )
    .await
    .expect("create metadata run");
    let jobs = claim_jobs(&pool, 1, "retry-worker", 300)
        .await
        .expect("claim metadata job");
    let job = jobs.first().expect("metadata job");
    let mut review_ids = Vec::new();
    for field in ["correspondent", "document_type"] {
        review_ids.push(
            create_review_item(
                &pool,
                job,
                json!({ field: 7, "standard_metadata": { "field": field } }),
                json!([]),
                json!({}),
                "retry-worker",
            )
            .await
            .expect("create sibling review")
            .expect("review id"),
        );
    }
    Some(Fixture {
        _guard: guard,
        pool,
        run_id,
        review_ids,
        actor_id,
    })
}

async fn status_of(pool: &DbPool, review_id: Uuid) -> String {
    sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(pool)
        .await
        .expect("review status")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn retry_rejects_all_pending_siblings_and_queues_an_overridden_metadata_run() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let overrides = json!({ "provider_name": "cloud", "model": "m-2" });
    let outcome = retry_review_with_overrides(
        &fixture.pool,
        fixture.review_ids[0],
        fixture.actor_id,
        &overrides,
    )
    .await
    .expect("retry");
    let ReviewRetryOutcome::Queued {
        run_id,
        rejected_review_ids,
    } = outcome
    else {
        panic!("expected a queued retry, got {outcome:?}");
    };
    assert_ne!(run_id, fixture.run_id);
    assert_eq!(rejected_review_ids.len(), 2, "both siblings are closed");
    for review_id in &fixture.review_ids {
        assert_eq!(status_of(&fixture.pool, *review_id).await, "rejected");
    }
    let old_status: String = sqlx::query_scalar("select status from pipeline_runs where id = $1")
        .bind(fixture.run_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("old run");
    assert_eq!(old_status, "rejected");
    let new_run = sqlx::query("select mode, trigger_tag from pipeline_runs where id = $1")
        .bind(run_id)
        .fetch_one(&fixture.pool)
        .await
        .expect("new run");
    let mode: String = new_run.get("mode");
    let trigger: String = new_run.get("trigger_tag");
    assert_eq!(mode, "manual_review");
    assert_eq!(trigger, "review-retry");
    let payload: serde_json::Value = sqlx::query_scalar(
        "select payload from jobs where run_id = $1 and stage = 'metadata' and status = 'queued'",
    )
    .bind(run_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("new metadata job");
    assert_eq!(payload[METADATA_RETRY_OVERRIDES_KEY], overrides);
    assert_eq!(payload["priority"], 0);
    let audits: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type = 'review.retried' and run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("retry audit");
    assert_eq!(audits, 1);

    // A second retry of the same (now rejected) review is a NotPending race.
    let again = retry_review_with_overrides(
        &fixture.pool,
        fixture.review_ids[1],
        fixture.actor_id,
        &overrides,
    )
    .await
    .expect_err("already decided");
    assert!(matches!(
        again.downcast_ref::<ReviewDecisionError>(),
        Some(ReviewDecisionError::NotPending)
    ));
    let missing =
        retry_review_with_overrides(&fixture.pool, Uuid::now_v7(), fixture.actor_id, &overrides)
            .await
            .expect_err("unknown review");
    assert!(matches!(
        missing.downcast_ref::<ReviewDecisionError>(),
        Some(ReviewDecisionError::NotFound)
    ));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn retry_refuses_while_a_sibling_is_being_applied_and_changes_nothing() {
    let Some(fixture) = fixture().await else {
        return;
    };
    review_decision(
        &fixture.pool,
        fixture.review_ids[1],
        "approved",
        None,
        fixture.actor_id,
    )
    .await
    .expect("approve sibling");
    let outcome = retry_review_with_overrides(
        &fixture.pool,
        fixture.review_ids[0],
        fixture.actor_id,
        &json!({}),
    )
    .await
    .expect("retry outcome");
    assert_eq!(outcome, ReviewRetryOutcome::SiblingInFlight);
    assert_eq!(
        status_of(&fixture.pool, fixture.review_ids[0]).await,
        "pending"
    );
    let runs: i64 = sqlx::query_scalar("select count(*) from pipeline_runs")
        .fetch_one(&fixture.pool)
        .await
        .expect("run count");
    assert_eq!(runs, 1, "no retry run was created");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn metadata_options_prompt_lookup_and_review_document_resolve_from_local_tables() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = &fixture.pool;
    pool.execute("truncate paperless_correspondents, paperless_document_types")
        .await
        .expect("truncate mirrors");
    pool.execute(
        r#"
        insert into paperless_correspondents (id, name) values (3, 'zeta'), (1, 'Alpha'), (2, 'beta');
        insert into paperless_document_types (id, name) values (9, 'Invoice');
        "#,
    )
    .await
    .expect("seed mirrors");
    let correspondents = list_paperless_correspondent_options(pool, 100)
        .await
        .expect("correspondents");
    let names: Vec<&str> = correspondents.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Alpha", "beta", "zeta"],
        "case-insensitive order"
    );
    assert_eq!(
        list_paperless_correspondent_options(pool, 2)
            .await
            .expect("limited")
            .len(),
        2
    );
    assert_eq!(
        list_paperless_document_type_options(pool, 100)
            .await
            .expect("types"),
        vec![PaperlessNamedOption {
            id: 9,
            name: "Invoice".to_owned()
        }]
    );

    assert_eq!(
        review_document_id(pool, fixture.review_ids[0])
            .await
            .expect("review doc"),
        Some(DOCUMENT_ID)
    );
    assert_eq!(
        review_document_id(pool, Uuid::now_v7())
            .await
            .expect("missing review"),
        None
    );

    let prompt_id: Uuid = sqlx::query_scalar(
        r#"
        insert into prompts (stage, name, version, content, active)
        values ('metadata', 'retry-test', 1, 'prompt body', false)
        returning id
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("seed prompt");
    let prompt = get_prompt_by_id(pool, prompt_id)
        .await
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.stage, Stage::Metadata);
    assert_eq!(prompt.content, "prompt body");
    assert!(
        get_prompt_by_id(pool, Uuid::now_v7())
            .await
            .expect("missing prompt")
            .is_none()
    );
    sqlx::query("delete from prompts where id = $1")
        .bind(prompt_id)
        .execute(pool)
        .await
        .expect("cleanup prompt");
}
