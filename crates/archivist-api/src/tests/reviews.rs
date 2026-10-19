use crate::*;

#[test]
fn active_apply_ownership_is_a_conflict() {
    let error = ApiError::from(anyhow::Error::new(archivist_apply::ApplySourceBusy));
    assert_eq!(error.status, StatusCode::CONFLICT);
}

#[test]
fn edited_review_retains_new_objects_alongside_document_fields() {
    let input = json!({
        "patch": {
            "title": "Edited invoice",
            "tags": [7],
            "archivist_new_objects": {
                "tags": ["New tag"],
                "correspondent": "New correspondent"
            }
        }
    });
    let request: EditReviewRequest = serde_json::from_value(input.clone()).expect("edit request");
    let stored = serde_json::to_value(request.patch).expect("persist edited patch");
    assert_eq!(stored, input["patch"]);
    let pending = archivist_apply::PendingNewObjects::from_patch_value(
        &stored,
        &archivist_core::WorkflowTags::default(),
    );
    assert_eq!(pending.tags, ["New tag"]);
    assert_eq!(pending.correspondent.as_deref(), Some("New correspondent"));
}

#[test]
fn edited_review_can_remove_pending_objects_and_clear_reference_fields() {
    for sidecar in [None, Some(json!({})), Some(Value::Null)] {
        let mut patch = json!({"title": "Edited", "correspondent": null, "document_type": null});
        if let Some(sidecar) = sidecar {
            patch[archivist_apply::PENDING_NEW_OBJECTS_KEY] = sidecar;
        }
        let request: EditReviewRequest =
            serde_json::from_value(json!({"patch": patch})).expect("edit request");
        let stored = serde_json::to_value(request.patch).expect("persist edited patch");
        assert_eq!(stored["correspondent"], Value::Null);
        assert!(
            stored.get("correspondent").is_some(),
            "the clear must not be omitted"
        );
        assert!(stored.get("document_type").is_some());
        assert!(
            archivist_apply::PendingNewObjects::from_patch_value(
                &stored,
                &archivist_core::WorkflowTags::default(),
            )
            .is_empty()
        );
    }
}

#[test]
fn edited_review_rejects_malformed_pending_object_names() {
    for sidecar in [
        json!({"tags": [42]}),
        json!({"correspondent": 42}),
        json!({"tags": "invalid"}),
    ] {
        assert!(
            serde_json::from_value::<EditReviewRequest>(json!({
                "patch": {"title": "Edited", "archivist_new_objects": sidecar}
            }))
            .is_err()
        );
    }
}

/// The real edit route must keep the review sidecar through persistence,
/// materialize it at apply time, and send explicit reference clears to
/// Paperless. No live Paperless instance or credentials are used.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn edited_review_applies_pending_objects_and_explicit_nulls() {
    use crate::test_support::*;

    #[derive(Default)]
    struct PaperlessState {
        document: Value,
        tags: Vec<Value>,
        created_tags: Vec<String>,
        created_correspondents: Vec<String>,
    }
    type MockState = Arc<tokio::sync::Mutex<PaperlessState>>;

    async fn document(State(state): State<MockState>) -> Json<Value> {
        Json(state.lock().await.document.clone())
    }
    async fn patch_document(
        State(state): State<MockState>,
        Json(patch): Json<Value>,
    ) -> Json<Value> {
        let mut state = state.lock().await;
        assert!(
            patch
                .get(archivist_apply::PENDING_NEW_OBJECTS_KEY)
                .is_none()
        );
        for (key, value) in patch.as_object().unwrap() {
            state.document[key] = value.clone();
        }
        Json(state.document.clone())
    }
    async fn tags(State(state): State<MockState>) -> Json<Value> {
        let state = state.lock().await;
        Json(json!({"count": state.tags.len(), "next": null, "results": state.tags}))
    }
    async fn create_tag(State(state): State<MockState>, Json(body): Json<Value>) -> Json<Value> {
        let mut state = state.lock().await;
        state
            .created_tags
            .push(body["name"].as_str().unwrap().to_owned());
        let tag = json!({"id": 100 + state.tags.len(), "name": body["name"]});
        state.tags.push(tag.clone());
        Json(tag)
    }
    async fn correspondents() -> Json<Value> {
        Json(json!({"count": 0, "next": null, "results": []}))
    }
    async fn create_correspondent(
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state
            .lock()
            .await
            .created_correspondents
            .push(body["name"].as_str().unwrap().to_owned());
        Json(json!({"id": 200, "name": body["name"]}))
    }

    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    let (admin, session) = harness_user(&pool, "review-editor", &[Role::Admin]).await;
    let original = get_runtime_settings(&pool)
        .await
        .expect("original settings");
    let mock = Arc::new(tokio::sync::Mutex::new(PaperlessState {
        document: json!({
            "id": 1, "title": "Old title", "tags": [], "correspondent": null,
            "document_type": 9, "content": "body", "custom_fields": []
        }),
        tags: original
            .workflow
            .tags
            .all()
            .into_iter()
            .enumerate()
            .map(|(index, name)| json!({"id": index + 1, "name": name}))
            .collect(),
        ..Default::default()
    }));
    let baseline = archivist_apply::review_apply_baseline(
        &serde_json::from_value(mock.lock().await.document.clone()).unwrap(),
    );
    let app = Router::new()
        .route("/api/documents/{id}/", get(document).patch(patch_document))
        .route("/api/tags/", get(tags).post(create_tag))
        .route(
            "/api/correspondents/",
            get(correspondents).post(create_correspondent),
        )
        .with_state(mock.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let secret_id = upsert_encrypted_secret(
        &pool,
        &state.config.secret_key,
        &format!("review-paperless-{}", Uuid::now_v7()),
        &SecretString::from("test-paperless-token".to_owned()),
        admin,
    )
    .await
    .expect("test token");
    let mut settings = original.clone();
    settings.paperless.base_url = format!("http://{address}/");
    settings.paperless.token_secret_id = Some(secret_id);
    settings.paperless.active_archive = String::new();
    settings.paperless.archive_profiles.clear();
    settings.tagging.allow_new_tags = true;
    settings.metadata.allow_new_correspondents = true;
    update_runtime_settings(&pool, &settings, admin)
        .await
        .expect("mock settings");

    let review_id: Uuid = sqlx::query_scalar(
        "insert into review_items (paperless_document_id, stage, status, suggested_patch, validation_warnings, baseline) values (1, 'metadata', 'pending', $1, '[]', $2) returning id",
    ).bind(json!({"title": "Suggested", "archivist_new_objects": {"tags": ["New tag"], "correspondent": "New correspondent"}}))
        .bind(baseline).fetch_one(&pool).await.expect("pending review");
    let api = TestApi::new(state);
    let response = api.send(Method::POST, &format!("/api/reviews/{review_id}/edit"), &session, Some(json!({
        "patch": {
            "title": "Edited title", "document_type": null,
            "archivist_new_objects": {"tags": ["New tag"], "correspondent": "New correspondent"}
        }
    }))).await;
    // Restore shared settings before any assertions so a test failure cannot
    // redirect a subsequent integration test to this stopped mock server.
    update_runtime_settings(&pool, &original, admin)
        .await
        .expect("restore settings");
    server.abort();
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let mock = mock.lock().await;
    assert_eq!(mock.document["title"], "Edited title");
    assert_eq!(mock.document["document_type"], Value::Null);
    assert_eq!(mock.document["correspondent"], 200);
    assert_eq!(mock.created_tags, ["New tag"]);
    assert_eq!(mock.created_correspondents, ["New correspondent"]);
    let row = sqlx::query("select status, edited_patch from review_items where id = $1")
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("review result");
    assert_eq!(row.get::<String, _>("status"), "applied");
    let edited: Value = row.get("edited_patch");
    assert_eq!(edited["archivist_new_objects"]["tags"], json!(["New tag"]));
    assert!(edited.get("document_type").is_some());
}
