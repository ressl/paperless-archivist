use std::sync::Arc;

use archivist_apply::{
    ApplyExecution, ApplyRequest, ReviewApplyConflict, ReviewApplyPrecondition,
    ReviewTagOperations, apply_document, patch_hash, recover_review_apply_intents,
    resume_apply_source, review_apply_baseline,
};
use archivist_core::DocumentPatch;
use archivist_db::{
    ApplyIntentInput, DbPool, connect, get_apply_intent, mark_apply_intent_confirmed,
    mark_apply_intent_in_flight, migrate, prepare_apply_intent,
};
use archivist_paperless::{PaperlessClient, PaperlessDocumentDetail};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use secrecy::SecretString;
use serde_json::{Value, json};
use sqlx::Executor;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
struct MockPaperless {
    document: Value,
    patch_count: usize,
    invalid_first_response: bool,
    patch_gate: Option<Arc<PatchGate>>,
}

#[derive(Default)]
struct PatchGate {
    started: Notify,
    finish: Notify,
    applied: Notify,
}

async fn get_document(
    State(state): State<Arc<Mutex<MockPaperless>>>,
    Path(_id): Path<i32>,
) -> Json<Value> {
    Json(state.lock().await.document.clone())
}

async fn patch_document(
    State(state): State<Arc<Mutex<MockPaperless>>>,
    Path(_id): Path<i32>,
    Json(patch): Json<Value>,
) -> Response {
    let gate = state.lock().await.patch_gate.clone();
    if let Some(gate) = &gate {
        gate.started.notify_one();
        gate.finish.notified().await;
    }
    let mut state = state.lock().await;
    state.patch_count += 1;
    let document = state.document.as_object_mut().expect("document object");
    for (key, value) in patch.as_object().expect("patch object") {
        document.insert(key.clone(), value.clone());
    }
    if let Some(gate) = gate {
        gate.applied.notify_one();
    }
    if state.invalid_first_response && state.patch_count == 1 {
        return (StatusCode::OK, "response-lost-after-commit").into_response();
    }
    Json(state.document.clone()).into_response()
}

async fn mock_client(invalid_first_response: bool) -> (PaperlessClient, Arc<Mutex<MockPaperless>>) {
    let state = Arc::new(Mutex::new(MockPaperless {
        document: json!({
            "id": 1,
            "title": "old",
            "created": "2026-07-17",
            "modified": "2026-07-17T06:00:00Z",
            "content": "body",
            "tags": [1],
            "correspondent": null,
            "document_type": null,
            "custom_fields": [],
            "original_file_name": "doc.pdf"
        }),
        patch_count: 0,
        invalid_first_response,
        patch_gate: None,
    }));
    let app = Router::new()
        .route(
            "/api/documents/{id}/",
            get(get_document).patch(patch_document),
        )
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let address = listener.local_addr().expect("mock address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    let client = PaperlessClient::new(
        &format!("http://{address}/"),
        SecretString::from("test-token".to_owned()),
        5,
    )
    .expect("client");
    (client, state)
}

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database (DB integration tests share one database: run them serially with `-- --ignored --test-threads=1`, see scripts/verify/migration_smoke.sh)");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        "truncate paperless_apply_intents, review_items, pipeline_runs, document_inventory, audit_events, metrics_counters restart identity cascade",
    )
    .await
    .expect("truncate state-machine tables");
    Some(pool)
}

fn request(patch: DocumentPatch) -> ApplyRequest {
    ApplyRequest {
        source: "worker_auto".to_owned(),
        source_key: "job:00000000-0000-0000-0000-000000000001".to_owned(),
        owner_type: "worker".to_owned(),
        owner_id: "worker-a".to_owned(),
        paperless_document_id: 1,
        run_id: None,
        job_id: None,
        review_id: None,
        patch,
        before: Some(json!({"title": "old"})),
        metadata: json!({"stage": "metadata"}),
        review_revert_status: None,
        review_precondition: None,
        allow_custom_fields_fallback: false,
    }
}

fn title_patch() -> DocumentPatch {
    DocumentPatch {
        content: None,
        title: Some("new".to_owned()),
        tags: None,
        correspondent: None,
        document_type: None,
        created: None,
        custom_fields: None,
    }
}

#[tokio::test]
async fn review_conflict_reads_latest_document_but_never_creates_an_intent_or_patches() {
    let (client, state) = mock_client(false).await;
    let original: PaperlessDocumentDetail =
        serde_json::from_value(state.lock().await.document.clone()).expect("original document");
    let patch = title_patch();
    let baseline = review_apply_baseline(&original);
    state.lock().await.document["title"] = json!("newer manual title");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .expect("lazy pool");
    let mut request = request(patch);
    request.review_precondition = Some(ReviewApplyPrecondition {
        baseline,
        tag_operations: ReviewTagOperations::default(),
    });

    let error = apply_document(&pool, &client, request)
        .await
        .expect_err("manual title must conflict");
    let conflict = error
        .downcast_ref::<ReviewApplyConflict>()
        .expect("typed review conflict");
    assert_eq!(conflict.fields(), &["title".to_owned()]);
    assert_eq!(state.lock().await.patch_count, 0);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn ambiguous_success_is_reconciled_and_never_patched_twice() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let (client, state) = mock_client(true).await;
    let request = request(title_patch());

    let first = apply_document(&pool, &client, request.clone())
        .await
        .expect("reconcile ambiguous response");
    assert!(matches!(first, ApplyExecution::Reconciled { .. }));
    let replay = apply_document(&pool, &client, request)
        .await
        .expect("replay confirmed state");
    assert!(matches!(replay, ApplyExecution::Reconciled { .. }));
    assert_eq!(state.lock().await.patch_count, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn existing_in_flight_mismatch_fails_without_blind_patch() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let (client, state) = mock_client(false).await;
    let request = request(title_patch());
    let hash = patch_hash(&request.patch).expect("hash");
    let intent = prepare_apply_intent(
        &pool,
        &ApplyIntentInput {
            source: request.source.clone(),
            source_key: request.source_key.clone(),
            owner_type: request.owner_type.clone(),
            owner_id: request.owner_id.clone(),
            paperless_document_id: request.paperless_document_id,
            run_id: request.run_id,
            job_id: request.job_id,
            review_id: request.review_id,
            patch_hash: hash,
            patch: serde_json::to_value(&request.patch).expect("patch JSON"),
            before: request.before.clone(),
            metadata: request.metadata.clone(),
            review_revert_status: None,
        },
    )
    .await
    .expect("prepare");
    assert!(
        mark_apply_intent_in_flight(&pool, intent.attempt_id, "dead-worker")
            .await
            .expect("mark in flight")
    );

    let error = apply_document(&pool, &client, request)
        .await
        .expect_err("mismatch must stop");
    assert!(error.to_string().contains("ambiguous"));
    assert_eq!(state.lock().await.patch_count, 0);
    assert_eq!(
        get_apply_intent(&pool, intent.attempt_id)
            .await
            .expect("get")
            .expect("intent")
            .state,
        "failed"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn confirmed_job_is_resumed_after_lease_change_without_second_patch() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let (client, state) = mock_client(false).await;
    let request = request(title_patch());

    let first = apply_document(&pool, &client, request.clone())
        .await
        .expect("initial apply");
    assert!(matches!(first, ApplyExecution::Confirmed { .. }));
    let replay = resume_apply_source(&pool, &client, &request.source_key)
        .await
        .expect("resume source")
        .expect("recoverable intent");
    assert!(matches!(replay, ApplyExecution::Confirmed { .. }));
    assert_eq!(state.lock().await.patch_count, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn confirmed_human_intent_finalizes_local_review_after_restart() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let actor_id = uuid::Uuid::now_v7();
    sqlx::query("insert into users (id, username, password_hash) values ($1, $2, 'test-hash')")
        .bind(actor_id)
        .bind(format!("reviewer-{actor_id}"))
        .execute(&pool)
        .await
        .expect("insert review actor");
    let run_id: uuid::Uuid = sqlx::query_scalar(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
        values (1, 'manual_review', 'ai-process', 'waiting_review', '["metadata"]'::jsonb)
        returning id
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("insert run");
    let review_id: uuid::Uuid = sqlx::query_scalar(
        r#"
        insert into review_items (
          run_id, paperless_document_id, stage, status,
          suggested_patch, validation_warnings, reviewed_at
        )
        values ($1, 1, 'metadata', 'applying', '{"title":"new"}'::jsonb, '[]'::jsonb, now())
        returning id
        "#,
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("insert review");
    let request = ApplyRequest {
        source: "human_review".to_owned(),
        source_key: format!("review:{review_id}"),
        owner_type: "user".to_owned(),
        owner_id: actor_id.to_string(),
        paperless_document_id: 1,
        run_id: Some(run_id),
        job_id: None,
        review_id: Some(review_id),
        patch: title_patch(),
        before: Some(json!({"title": "old"})),
        metadata: json!({"stage": "metadata"}),
        review_revert_status: Some("approved".to_owned()),
        review_precondition: None,
        allow_custom_fields_fallback: false,
    };
    let intent = prepare_apply_intent(
        &pool,
        &ApplyIntentInput {
            source: request.source.clone(),
            source_key: request.source_key.clone(),
            owner_type: request.owner_type.clone(),
            owner_id: request.owner_id.clone(),
            paperless_document_id: request.paperless_document_id,
            run_id: request.run_id,
            job_id: request.job_id,
            review_id: request.review_id,
            patch_hash: patch_hash(&request.patch).expect("hash"),
            patch: serde_json::to_value(&request.patch).expect("patch JSON"),
            before: request.before.clone(),
            metadata: request.metadata.clone(),
            review_revert_status: request.review_revert_status.clone(),
        },
    )
    .await
    .expect("prepare");
    assert!(
        mark_apply_intent_in_flight(&pool, intent.attempt_id, &request.owner_id)
            .await
            .expect("start")
    );
    assert!(
        mark_apply_intent_confirmed(
            &pool,
            intent.attempt_id,
            &request.owner_id,
            Some(json!({"title": "new"})),
            8,
        )
        .await
        .expect("confirm")
    );

    let (client, state) = mock_client(false).await;
    let summary = recover_review_apply_intents(&pool, &client, 10)
        .await
        .expect("recover review intents");
    assert_eq!(summary.applied, 1);
    assert_eq!(state.lock().await.patch_count, 0);
    let status: String = sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("review status");
    assert_eq!(status, "applied");
    assert_eq!(
        get_apply_intent(&pool, intent.attempt_id)
            .await
            .expect("get intent")
            .expect("intent")
            .state,
        "finalized"
    );
}

/// Paperless whose first PATCH answers 503 without applying anything (a
/// proxy/gateway error in front of a healthy instance). #389
async fn transient_failure_mock() -> (PaperlessClient, Arc<Mutex<MockPaperless>>) {
    async fn flaky_patch(
        State(state): State<Arc<Mutex<MockPaperless>>>,
        Path(id): Path<i32>,
        Json(patch): Json<Value>,
    ) -> Response {
        let first = {
            let mut guard = state.lock().await;
            guard.patch_count += 1;
            guard.patch_count == 1
        };
        if first {
            return (StatusCode::SERVICE_UNAVAILABLE, "upstream unavailable").into_response();
        }
        state.lock().await.patch_count -= 1;
        patch_document(State(state), Path(id), Json(patch)).await
    }

    let (_, state) = mock_client(false).await;
    let app = Router::new()
        .route("/api/documents/{id}/", get(get_document).patch(flaky_patch))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let address = listener.local_addr().expect("mock address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    let client = PaperlessClient::new(
        &format!("http://{address}/"),
        SecretString::from("test-token".to_owned()),
        5,
    )
    .expect("client");
    (client, state)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn transient_patch_failure_keeps_the_same_patch_retryable() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let (client, state) = transient_failure_mock().await;
    let request = request(title_patch());

    let error = apply_document(&pool, &client, request.clone())
        .await
        .expect_err("503 must surface as an error");
    assert!(error.to_string().contains("will be retried"), "{error:#}");
    let hash = patch_hash(&request.patch).expect("hash");
    let state_after_failure: String = sqlx::query_scalar(
        "select state from paperless_apply_intents where source_key = $1 and patch_hash = $2",
    )
    .bind(&request.source_key)
    .bind(&hash)
    .fetch_one(&pool)
    .await
    .expect("intent state");
    assert_eq!(
        state_after_failure, "prepared",
        "a transient failure must not block the patch terminally"
    );

    let retried = apply_document(&pool, &client, request)
        .await
        .expect("the same patch applies on retry");
    assert!(matches!(retried, ApplyExecution::Confirmed { .. }));
    let guard = state.lock().await;
    assert_eq!(guard.patch_count, 2);
    assert_eq!(guard.document["title"], "new");
}

async fn human_review_request(pool: &DbPool) -> ApplyRequest {
    let actor_id = uuid::Uuid::now_v7();
    sqlx::query("insert into users (id, username, password_hash) values ($1, $2, 'test-hash')")
        .bind(actor_id)
        .bind(format!("reviewer-{actor_id}"))
        .execute(pool)
        .await
        .expect("review actor");
    let review_id = sqlx::query_scalar(
        "insert into review_items (paperless_document_id, stage, status, suggested_patch, validation_warnings, reviewed_at) values (1, 'metadata', 'applying', '{\"title\":\"new\"}', '[]', now()) returning id",
    ).fetch_one(pool).await.expect("review");
    ApplyRequest {
        source: "human_review".to_owned(),
        source_key: format!("review:{review_id}"),
        owner_type: "user".to_owned(),
        owner_id: actor_id.to_string(),
        review_id: Some(review_id),
        review_revert_status: Some("pending".to_owned()),
        ..request(title_patch())
    }
}

async fn prepare_request(pool: &DbPool, request: &ApplyRequest) -> uuid::Uuid {
    prepare_apply_intent(
        pool,
        &ApplyIntentInput {
            source: request.source.clone(),
            source_key: request.source_key.clone(),
            owner_type: request.owner_type.clone(),
            owner_id: request.owner_id.clone(),
            paperless_document_id: request.paperless_document_id,
            run_id: request.run_id,
            job_id: request.job_id,
            review_id: request.review_id,
            patch_hash: patch_hash(&request.patch).expect("hash"),
            patch: serde_json::to_value(&request.patch).expect("patch"),
            before: request.before.clone(),
            metadata: request.metadata.clone(),
            review_revert_status: request.review_revert_status.clone(),
        },
    )
    .await
    .expect("prepare intent")
    .attempt_id
}

async fn wait_for(signal: &Notify) {
    tokio::time::timeout(std::time::Duration::from_secs(5), signal.notified())
        .await
        .expect("mock Paperless reached the expected request phase");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn recovery_does_not_fail_a_live_review_patch() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let request = human_review_request(&pool).await;
    let source_key = request.source_key.clone();
    let (client, state) = mock_client(false).await;
    let gate = Arc::new(PatchGate::default());
    state.lock().await.patch_gate = Some(gate.clone());
    let task_pool = pool.clone();
    let task_client = client.clone();
    let apply =
        tokio::spawn(async move { apply_document(&task_pool, &task_client, request).await });
    wait_for(&gate.started).await;

    // Paperless still returns the old title while the API's PATCH is alive.
    let sweep = recover_review_apply_intents(&pool, &client, 10)
        .await
        .expect("sweep");
    assert_eq!(sweep.examined, 1);
    assert_eq!(sweep.deferred, 1);
    assert_eq!(sweep.failed_settled, 0);
    let intent = archivist_db::get_recoverable_apply_intent_by_source_key(&pool, &source_key)
        .await
        .expect("intent")
        .expect("active intent");
    assert_eq!(intent.state, "in_flight");

    gate.finish.notify_one();
    assert!(matches!(
        apply.await.expect("join").expect("apply"),
        ApplyExecution::Confirmed { .. }
    ));
    let sweep = recover_review_apply_intents(&pool, &client, 10)
        .await
        .expect("settle");
    assert_eq!(sweep.applied, 1);
    assert_eq!(state.lock().await.patch_count, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn concurrent_recovery_workers_do_not_take_over_each_others_patch() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let request = human_review_request(&pool).await;
    prepare_request(&pool, &request).await;
    let (client, state) = mock_client(false).await;
    let gate = Arc::new(PatchGate::default());
    state.lock().await.patch_gate = Some(gate.clone());
    let task_pool = pool.clone();
    let task_client = client.clone();
    let first =
        tokio::spawn(
            async move { recover_review_apply_intents(&task_pool, &task_client, 10).await },
        );
    wait_for(&gate.started).await;

    let second = recover_review_apply_intents(&pool, &client, 10)
        .await
        .expect("competing recovery");
    assert_eq!(second.deferred, 1);
    assert_eq!(second.failed_settled, 0);
    gate.finish.notify_one();
    assert_eq!(
        first.await.expect("join").expect("first recovery").applied,
        1
    );
    assert_eq!(state.lock().await.patch_count, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn cancelled_execution_releases_ownership_and_recovers_without_second_patch() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let request = human_review_request(&pool).await;
    let source_key = request.source_key.clone();
    let (client, state) = mock_client(false).await;
    let gate = Arc::new(PatchGate::default());
    state.lock().await.patch_gate = Some(gate.clone());
    let task_pool = pool.clone();
    let task_client = client.clone();
    let apply =
        tokio::spawn(async move { apply_document(&task_pool, &task_client, request).await });
    wait_for(&gate.started).await;
    let intent = archivist_db::get_recoverable_apply_intent_by_source_key(&pool, &source_key)
        .await
        .expect("intent")
        .expect("active intent");
    // Block local confirmation while letting the remote PATCH commit, so
    // cancellation occurs precisely between the HTTP and database writes.
    let mut confirmation_fence = pool.begin().await.expect("confirmation fence");
    sqlx::query("select attempt_id from paperless_apply_intents where attempt_id = $1 for update")
        .bind(intent.attempt_id)
        .fetch_one(&mut *confirmation_fence)
        .await
        .expect("lock intent");
    gate.finish.notify_one();
    wait_for(&gate.applied).await;
    apply.abort();
    assert!(apply.await.expect_err("cancelled task").is_cancelled());
    confirmation_fence
        .rollback()
        .await
        .expect("release confirmation fence");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let sweep = recover_review_apply_intents(&pool, &client, 10)
                .await
                .expect("recovery");
            if sweep.applied == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session lock released after cancellation");
    assert_eq!(state.lock().await.patch_count, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn recovery_preserves_null_clears_and_uses_only_one_pool_connection() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let mut request = human_review_request(&pool).await;
    request.patch.correspondent = Some(None);
    request.patch.document_type = Some(None);
    prepare_request(&pool, &request).await;
    let (client, state) = mock_client(false).await;
    state.lock().await.document["correspondent"] = json!(7);
    state.lock().await.document["document_type"] = json!(9);
    let single = connect(&std::env::var("DATABASE_URL").expect("database URL"), 1)
        .await
        .expect("single connection pool");

    let sweep = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        recover_review_apply_intents(&single, &client, 10),
    )
    .await
    .expect("no second connection required")
    .expect("recovery");
    assert_eq!(sweep.applied, 1);
    let state = state.lock().await;
    assert_eq!(state.document["correspondent"], Value::Null);
    assert_eq!(state.document["document_type"], Value::Null);
    assert_eq!(state.patch_count, 1);
}
