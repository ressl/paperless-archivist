//! 2026-09 audit: API / security hardening tests.

use crate::test_support::*;
use crate::*;

// ----- 2026-09 audit: API / security hardening ----------------------

async fn spawn_api_router(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = router(state);
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .ok();
    });
    (format!("http://{address}"), handle)
}

fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn is_json(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
}

#[tokio::test]
async fn unknown_api_paths_and_body_rejections_return_json_errors() {
    // #399
    let (base, handle) = spawn_api_router(api_text_test_state()).await;
    let client = no_redirect_client();
    for path in [
        "/api/does-not-exist",
        "/api/reviews/not/a/route",
        "/api/auth/does-not-exist",
        "/api/webhooks/does-not-exist",
    ] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert!(is_json(&response), "{path} must not serve the SPA");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"], "not found");
    }

    let malformed = client
        .post(format!("{base}/api/auth/login"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert!(is_json(&malformed));
    let body: Value = malformed.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );

    let wrong_shape = client
        .post(format!("{base}/api/auth/login"))
        .json(&json!({ "username": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_shape.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(is_json(&wrong_shape));

    let no_content_type = client
        .post(format!("{base}/api/auth/login"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(no_content_type.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(is_json(&no_content_type));
    handle.abort();
}

#[tokio::test]
async fn oidc_callback_requires_the_browser_bound_state_cookie() {
    // #387: a callback URL crafted from the attacker's own login must not
    // log the victim in.
    let (base, handle) = spawn_api_router(api_text_test_state()).await;
    let client = no_redirect_client();
    let url = format!("{base}/api/auth/oidc/callback?code=attacker-code&state=attacker-state");

    let without_cookie = client.get(&url).send().await.unwrap();
    assert_eq!(without_cookie.status(), StatusCode::UNAUTHORIZED);
    let cleared = without_cookie
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.starts_with("pa_oidc_state=;") && value.contains("Max-Age=0"));
    assert!(
        cleared,
        "the state cookie is invalidated after every callback"
    );

    let other_browser = client
        .get(&url)
        .header(reqwest::header::COOKIE, "pa_oidc_state=victim-own-state")
        .send()
        .await
        .unwrap();
    assert_eq!(other_browser.status(), StatusCode::UNAUTHORIZED);
    let body: Value = other_browser.json().await.unwrap();
    assert_eq!(body["error"], "OIDC state does not match this browser");
    handle.abort();
}

#[test]
fn oidc_state_cookie_is_short_lived_http_only_and_compared_exactly() {
    // #387
    let rendered = oidc_state_cookie("abc123", true).to_string();
    for attribute in [
        "HttpOnly",
        "SameSite=Lax",
        "Secure",
        "Max-Age=600",
        "Path=/",
    ] {
        assert!(rendered.contains(attribute), "{rendered} lacks {attribute}");
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        HeaderValue::from_static("pa_session=x; pa_oidc_state=abc123"),
    );
    verify_oidc_state_binding(&headers, "abc123").expect("issued state is accepted");
    for forged in ["abc124", "abc12", ""] {
        assert_eq!(
            verify_oidc_state_binding(&headers, forged)
                .unwrap_err()
                .status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        verify_oidc_state_binding(&HeaderMap::new(), "abc123")
            .unwrap_err()
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn review_decision_races_map_to_conflict_and_not_found() {
    // #391
    let conflict = ApiError::from(anyhow::Error::new(ReviewDecisionError::NotPending));
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    let missing = ApiError::from(anyhow::Error::new(ReviewDecisionError::NotFound));
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[test]
fn rerun_batches_are_bounded_and_require_positive_ids() {
    // #390
    assert!(validate_rerun_document_ids(&[1, 2, 3]).is_ok());
    let max: Vec<i32> = (1..=MAX_RERUN_BATCH_DOCUMENTS as i32).collect();
    assert!(validate_rerun_document_ids(&max).is_ok());
    let over: Vec<i32> = (1..=MAX_RERUN_BATCH_DOCUMENTS as i32 + 1).collect();
    assert_eq!(
        validate_rerun_document_ids(&over).unwrap_err().status,
        StatusCode::BAD_REQUEST
    );
    for invalid in [vec![], vec![1, 0], vec![-5]] {
        assert_eq!(
            validate_rerun_document_ids(&invalid).unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
    }
}

#[test]
fn token_scopes_are_limited_by_the_creators_current_roles() {
    // #392
    let scopes = vec![
        "reviews:read".to_owned(),
        "reviews:write".to_owned(),
        "settings:read".to_owned(),
        "runs:read".to_owned(),
    ];
    assert_eq!(effective_token_scopes(&scopes, &[Role::Admin]), scopes);
    assert_eq!(
        effective_token_scopes(&scopes, &[Role::Reviewer]),
        vec!["reviews:read", "reviews:write", "runs:read"]
    );
    assert_eq!(
        effective_token_scopes(&scopes, &[Role::Viewer]),
        vec!["runs:read"]
    );
    assert!(effective_token_scopes(&scopes, &[]).is_empty());
}

#[test]
fn audit_exports_are_limited_per_actor_and_globally() {
    // #394
    let first = AuditExportSlot::acquire("test-export:alice".to_owned()).expect("first");
    assert!(
        AuditExportSlot::acquire("test-export:alice".to_owned()).is_none(),
        "one export per actor"
    );
    let others: Vec<_> = (1..MAX_CONCURRENT_AUDIT_EXPORTS)
        .map(|index| AuditExportSlot::acquire(format!("test-export:user-{index}")))
        .collect();
    assert!(others.iter().all(Option::is_some));
    assert!(
        AuditExportSlot::acquire("test-export:bob".to_owned()).is_none(),
        "global cap"
    );
    drop(first);
    assert!(AuditExportSlot::acquire("test-export:alice".to_owned()).is_some());
}

fn secret_binding_settings(secret_id: Uuid) -> RuntimeSettings {
    let mut settings = RuntimeSettings::default();
    settings.ai.providers = vec![AiProviderSettings {
        name: "openai".to_owned(),
        kind: AiProviderKind::OpenaiCompatible,
        base_url: "https://api.openai.example/v1".to_owned(),
        default_text_model: Some("gpt".to_owned()),
        default_vision_model: None,
        cost_per_1m_input_tokens_usd: None,
        cost_per_1m_output_tokens_usd: None,
        secret_id: Some(secret_id),
        enabled: true,
        tuning: ProviderTuning::default(),
    }];
    settings.paperless.base_url = "https://paperless-a.example".to_owned();
    settings.paperless.token_secret_id = Some(Uuid::from_u128(77));
    settings.paperless = settings.paperless.normalized();
    settings
}

#[tokio::test]
async fn provider_test_refuses_stored_secret_for_a_foreign_endpoint() {
    // #397
    let secret_id = Uuid::from_u128(42);
    let settings = secret_binding_settings(secret_id);
    assert!(saved_provider_secret_matches(
        &settings,
        "OpenAI",
        &AiProviderKind::OpenaiCompatible,
        "https://api.openai.example/v1/",
        secret_id
    ));
    let mut provider = make_api_provider(AiProviderKind::OpenaiCompatible);
    provider.name = "openai".to_owned();
    provider.base_url = "https://attacker.example/v1".to_owned();
    provider.secret_id = Some(secret_id);
    // Rejected before any secret lookup, so the lazy test pool is never used.
    let error = provider_test_secret(&api_text_test_state(), &settings, &provider, None)
        .await
        .expect_err("foreign URL + stored secret must be rejected");
    assert!(error.to_string().contains("saved provider endpoint"));

    provider.base_url = "https://api.openai.example/v1".to_owned();
    provider.kind = AiProviderKind::Anthropic;
    assert!(
        provider_test_secret(&api_text_test_state(), &settings, &provider, None)
            .await
            .is_err(),
        "a kind change cannot reuse the stored secret either"
    );
}

#[test]
fn settings_update_cannot_rebind_stored_secrets() {
    // #397 / #396
    let secret_id = Uuid::from_u128(42);
    let current = secret_binding_settings(secret_id);
    let none = HashSet::new();

    validate_secret_bindings(&current, &current, &none, false, false)
        .expect("an unchanged save is accepted");

    let mut moved_url = current.clone();
    moved_url.ai.providers[0].base_url = "https://attacker.example/v1".to_owned();
    assert_eq!(
        validate_secret_bindings(&current, &moved_url, &none, false, false)
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );
    let fresh_key = HashSet::from(["openai".to_owned()]);
    validate_secret_bindings(&current, &moved_url, &fresh_key, false, false)
        .expect("a freshly entered key may target the new URL");

    let mut stolen = current.clone();
    let mut attacker = stolen.ai.providers[0].clone();
    attacker.name = "attacker".to_owned();
    attacker.base_url = "https://attacker.example/v1".to_owned();
    stolen.ai.providers.push(attacker);
    assert!(validate_secret_bindings(&current, &stolen, &none, false, false).is_err());

    let mut moved_paperless = current.clone();
    moved_paperless.paperless.base_url = "https://paperless-b.example".to_owned();
    assert!(validate_secret_bindings(&current, &moved_paperless, &none, false, false).is_err());
    validate_secret_bindings(&current, &moved_paperless, &none, true, false)
        .expect("a new Paperless token may target the new instance");

    let mut foreign_profile = current.clone();
    foreign_profile
        .paperless
        .archive_profiles
        .push(archivist_core::PaperlessArchiveProfile {
            name: "other".to_owned(),
            base_url: "https://paperless-b.example".to_owned(),
            token_secret_id: current.paperless.token_secret_id,
            enabled: true,
        });
    assert!(
        validate_secret_bindings(&current, &foreign_profile, &none, false, false).is_err(),
        "instance A's token cannot be attached to a profile on host B"
    );
}

async fn seed_pending_review(pool: &DbPool) -> Uuid {
    let run_id: Uuid = sqlx::query_scalar(
        r#"
            insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
            values ((select coalesce(max(paperless_document_id), 0) + 1 from pipeline_runs),
                    'manual_review', 'ai-process', 'waiting_review', '["metadata"]'::jsonb)
            returning id
            "#,
    )
    .fetch_one(pool)
    .await
    .expect("insert run");
    sqlx::query_scalar(
        r#"
            insert into review_items (run_id, paperless_document_id, stage, status, suggested_patch, validation_warnings)
            values ($1, (select paperless_document_id from pipeline_runs where id = $1),
                    'metadata', 'pending', '{"title":"x"}'::jsonb, '[]'::jsonb)
            returning id
            "#,
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    .expect("insert review")
}

async fn review_status_of(pool: &DbPool, review_id: Uuid) -> String {
    sqlx::query_scalar("select status from review_items where id = $1")
        .bind(review_id)
        .fetch_one(pool)
        .await
        .expect("review status")
}

#[test]
fn cost_budget_json_reports_level_and_unknown_cost() {
    // #450
    let month_start = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
    let warning = cost_budget_json(100.0, 80, Some(85.0), month_start);
    assert_eq!(warning["level"], "warning");
    assert_eq!(warning["percent_used"], 85.0);
    assert_eq!(warning["monthly_budget_usd"], 100.0);
    assert_eq!(warning["warning_percent"], 80);
    let exceeded = cost_budget_json(10.0, 80, Some(12.5), month_start);
    assert_eq!(exceeded["level"], "exceeded");
    let unknown = cost_budget_json(10.0, 80, None, month_start);
    assert_eq!(unknown["level"], "unknown");
    assert!(unknown["percent_used"].is_null());
    assert!(unknown["month_to_date_cost_usd"].is_null());
}

#[test]
fn month_to_date_cost_ignores_unpriced_rows() {
    // #450
    let row = |cost: Option<f64>| ProviderUsageStats {
        provider: "p".to_owned(),
        model: "m".to_owned(),
        stage: "metadata".to_owned(),
        request_count: 1,
        avg_duration_ms: 0.0,
        p95_duration_ms: 0,
        input_tokens: 0,
        output_tokens: 0,
        estimated_cost_usd: cost,
        feedback_count: 0,
        positive_feedback: 0,
        negative_feedback: 0,
        acceptance_rate: None,
        latency_history: Vec::new(),
    };
    assert_eq!(month_to_date_cost(&[row(None)]), None);
    assert_eq!(month_to_date_cost(&[]), None);
    assert_eq!(
        month_to_date_cost(&[row(Some(1.5)), row(None), row(Some(2.0))]),
        Some(3.5)
    );
}

#[test]
fn cost_budget_settings_validation_rejects_out_of_range_values() {
    // #450
    let mut ui = archivist_core::UiSettings::default();
    assert!(validate_cost_budget_settings(&ui).is_ok());
    ui.monthly_cost_budget_usd = Some(-1.0);
    assert!(validate_cost_budget_settings(&ui).is_err());
    ui.monthly_cost_budget_usd = Some(25.0);
    ui.cost_budget_warning_percent = 0;
    assert!(validate_cost_budget_settings(&ui).is_err());
    ui.cost_budget_warning_percent = 100;
    assert!(validate_cost_budget_settings(&ui).is_ok());
}

#[test]
fn paperless_browser_base_prefers_public_url() {
    // #449
    let mut settings = RuntimeSettings::default();
    settings.paperless.base_url = "http://paperless:8000/".to_owned();
    assert_eq!(paperless_browser_base(&settings), "http://paperless:8000");
    settings.paperless.public_url = Some(" https://docs.example.com/ ".to_owned());
    assert_eq!(
        paperless_browser_base(&settings),
        "https://docs.example.com"
    );
}

#[tokio::test]
async fn chat_stream_events_are_single_line_json() {
    // #449: multi-line answer text must not break the SSE `data:` framing.
    use tokio_stream::StreamExt as _;
    let events = vec![
        ChatStreamEvent::Delta("line one\nline two\n\n".to_owned()),
        ChatStreamEvent::Error("boom".to_owned()),
    ];
    let stream =
        tokio_stream::iter(events).map(|event| Ok::<_, std::convert::Infallible>(event.to_sse()));
    let response = axum::response::sse::Sse::new(stream).into_response();
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        body.contains("event: delta\ndata: {\"text\":\"line one\\nline two\\n\\n\"}\n\n"),
        "{body}"
    );
    assert!(
        body.contains("event: error\ndata: {\"error\":\"boom\"}\n\n"),
        "{body}"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn chat_sessions_can_be_renamed_deleted_and_streamed_by_their_owner() {
    // #449
    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    // Default runtime settings: no Paperless token, so a streamed answer
    // fails after the stream started (source retrieval).
    sqlx::query("delete from settings where key = 'runtime'")
        .execute(&pool)
        .await
        .expect("reset runtime settings");
    let suffix = Uuid::now_v7().simple().to_string();
    let mut sessions = Vec::new();
    for (name, role) in [("owner", Role::Reviewer), ("other", Role::Operator)] {
        let user = create_user_with_roles(
            &pool,
            &format!("chat-{name}-{suffix}"),
            None,
            "hash",
            &[role],
            None,
        )
        .await
        .expect("user");
        let session_token = random_token();
        let csrf_token = random_token();
        create_session(
            &pool,
            user,
            &hash_token(&session_token),
            &hash_token(&csrf_token),
            Utc::now() + Duration::hours(1),
        )
        .await
        .expect("session");
        sessions.push((session_token, csrf_token));
    }
    let (base, handle) = spawn_api_router(state).await;
    let client = no_redirect_client();
    let call = |method: reqwest::Method, path: String, who: usize| {
        let (session_token, csrf_token) = &sessions[who];
        client
            .request(method, format!("{base}{path}"))
            .header(
                reqwest::header::COOKIE,
                format!("{SESSION_COOKIE}={session_token}"),
            )
            .header("x-csrf-token", csrf_token)
    };

    let created: Value = call(reqwest::Method::POST, "/api/chat/sessions".to_owned(), 0)
        .json(&json!({ "title": "Steuern 2025" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().expect("session id").to_owned();

    let listed: Value = call(reqwest::Method::GET, "/api/chat/sessions".to_owned(), 0)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed["paperless_base"].is_string());

    // Only the owner may rename; empty titles are rejected.
    let foreign = call(
        reqwest::Method::PATCH,
        format!("/api/chat/sessions/{id}"),
        1,
    )
    .json(&json!({ "title": "hijacked" }))
    .send()
    .await
    .unwrap();
    assert_eq!(foreign.status(), StatusCode::FORBIDDEN);
    let empty = call(
        reqwest::Method::PATCH,
        format!("/api/chat/sessions/{id}"),
        0,
    )
    .json(&json!({ "title": "   " }))
    .send()
    .await
    .unwrap();
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
    let renamed = call(
        reqwest::Method::PATCH,
        format!("/api/chat/sessions/{id}"),
        0,
    )
    .json(&json!({ "title": "  Steuern   2026 " }))
    .send()
    .await
    .unwrap();
    assert_eq!(renamed.status(), StatusCode::OK);
    let renamed: Value = renamed.json().await.unwrap();
    assert_eq!(renamed["title"], "Steuern 2026");
    let renamed_audits: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type = 'chat.session_renamed' and after->>'session_id' = $1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .expect("rename audit");
    assert_eq!(renamed_audits, 1);

    // Streamed answers validate before the stream starts ...
    let short = call(
        reqwest::Method::POST,
        format!("/api/chat/sessions/{id}/messages/stream"),
        0,
    )
    .json(&json!({ "question": "?" }))
    .send()
    .await
    .unwrap();
    assert_eq!(short.status(), StatusCode::BAD_REQUEST);
    assert!(is_json(&short));
    let foreign_stream = call(
        reqwest::Method::POST,
        format!("/api/chat/sessions/{id}/messages/stream"),
        1,
    )
    .json(&json!({ "question": "Welche Rechnungen?" }))
    .send()
    .await
    .unwrap();
    assert_eq!(foreign_stream.status(), StatusCode::FORBIDDEN);
    // ... and report later failures as an `error` event.
    let streamed = call(
        reqwest::Method::POST,
        format!("/api/chat/sessions/{id}/messages/stream"),
        0,
    )
    .json(&json!({ "question": "Welche Rechnungen?" }))
    .send()
    .await
    .unwrap();
    assert_eq!(streamed.status(), StatusCode::OK);
    assert!(
        streamed.headers()[reqwest::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert_eq!(
        streamed.headers()[reqwest::header::CACHE_CONTROL],
        "no-cache, no-transform"
    );
    let body = tokio::time::timeout(std::time::Duration::from_secs(10), streamed.text())
        .await
        .expect("stream ends after the error event")
        .unwrap();
    assert!(body.contains("event: error\ndata: {\"error\":"), "{body}");

    // Delete: foreign users cannot, the owner can, and it is gone after.
    let foreign_delete = call(
        reqwest::Method::DELETE,
        format!("/api/chat/sessions/{id}"),
        1,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(foreign_delete.status(), StatusCode::FORBIDDEN);
    let deleted = call(
        reqwest::Method::DELETE,
        format!("/api/chat/sessions/{id}"),
        0,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    let gone = call(reqwest::Method::GET, format!("/api/chat/sessions/{id}"), 0)
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::FORBIDDEN);
    let deleted_audits: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type = 'chat.session_deleted' and before->>'session_id' = $1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .expect("delete audit");
    assert_eq!(deleted_audits, 1);

    handle.abort();
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn review_endpoints_report_races_require_sessions_and_follow_token_roles() {
    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    let suffix = Uuid::now_v7().simple().to_string();
    let admin = create_user_with_roles(
        &pool,
        &format!("security-admin-{suffix}"),
        None,
        "hash",
        &[Role::Admin],
        None,
    )
    .await
    .expect("admin");
    let reviewer = create_user_with_roles(
        &pool,
        &format!("security-reviewer-{suffix}"),
        None,
        "hash",
        &[Role::Reviewer, Role::Auditor],
        None,
    )
    .await
    .expect("reviewer");
    let session_token = random_token();
    let csrf_token = random_token();
    create_session(
        &pool,
        reviewer,
        &hash_token(&session_token),
        &hash_token(&csrf_token),
        Utc::now() + Duration::hours(1),
    )
    .await
    .expect("session");
    let api_token = format!("pa_{}", random_token());
    archivist_db::create_api_token(
        &pool,
        "automation",
        &hash_token(&api_token),
        &["reviews:read".to_owned(), "reviews:write".to_owned()],
        reviewer,
        None,
    )
    .await
    .expect("token");

    let (base, handle) = spawn_api_router(state).await;
    let client = no_redirect_client();
    let session_post = |path: String| {
        client
            .post(format!("{base}{path}"))
            .header(
                reqwest::header::COOKIE,
                format!("{SESSION_COOKIE}={session_token}"),
            )
            .header("x-csrf-token", &csrf_token)
    };

    // #391: a second decision is a 409, an unknown review a 404.
    let decided = seed_pending_review(&pool).await;
    let first = session_post(format!("/api/reviews/{decided}/reject"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let again = session_post(format!("/api/reviews/{decided}/approve"))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::CONFLICT);
    let unknown = session_post(format!("/api/reviews/{}/approve", Uuid::now_v7()))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    // #388: a failing human apply (no Paperless token configured) returns
    // the review to `pending` instead of stranding it in `approved`.
    let failing = seed_pending_review(&pool).await;
    let apply_error = session_post(format!("/api/reviews/{failing}/approve"))
        .send()
        .await
        .unwrap();
    // #441: a missing Paperless token is operator state, not a 500.
    assert_eq!(apply_error.status(), StatusCode::CONFLICT);
    assert_eq!(review_status_of(&pool, failing).await, "pending");

    // #393: a token cannot make review decisions attributed to its creator.
    let token_target = seed_pending_review(&pool).await;
    for action in ["approve", "reject"] {
        let response = client
            .post(format!("{base}/api/reviews/{token_target}/{action}"))
            .bearer_auth(&api_token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{action}");
    }
    let edit = client
        .post(format!("{base}/api/reviews/{token_target}/edit"))
        .bearer_auth(&api_token)
        .json(&json!({ "patch": { "title": "token edit" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(edit.status(), StatusCode::FORBIDDEN);
    assert_eq!(review_status_of(&pool, token_target).await, "pending");
    let token_audits: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type like 'review.%' and metadata->>'review_id' = $1",
    )
    .bind(token_target.to_string())
    .fetch_one(&pool)
    .await
    .expect("token audit count");
    assert_eq!(
        token_audits, 0,
        "no decision may be attributed to the token creator"
    );

    // #392: token rights shrink with the creator's roles.
    let list = client
        .get(format!("{base}/api/reviews"))
        .bearer_auth(&api_token)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    set_user_roles(&pool, reviewer, &[Role::Viewer], admin)
        .await
        .expect("demote reviewer");
    let demoted = client
        .get(format!("{base}/api/reviews"))
        .bearer_auth(&api_token)
        .send()
        .await
        .unwrap();
    assert_eq!(demoted.status(), StatusCode::FORBIDDEN);

    handle.abort();
}

/// Session cookie + CSRF pair for a fresh user with `roles`.
async fn session_for_roles(pool: &DbPool, label: &str, roles: &[Role]) -> (String, String) {
    let suffix = Uuid::now_v7().simple().to_string();
    let user = create_user_with_roles(
        pool,
        &format!("{label}-{suffix}"),
        None,
        "hash",
        roles,
        None,
    )
    .await
    .expect("user");
    let session_token = random_token();
    let csrf_token = random_token();
    create_session(
        pool,
        user,
        &hash_token(&session_token),
        &hash_token(&csrf_token),
        Utc::now() + Duration::hours(1),
    )
    .await
    .expect("session");
    (session_token, csrf_token)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn review_metadata_options_retry_and_preview_proxy_follow_permissions() {
    // #420 / #445
    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    sqlx::query("delete from settings where key = 'runtime'")
        .execute(&pool)
        .await
        .expect("reset settings");
    sqlx::query("truncate paperless_correspondents, paperless_document_types")
        .execute(&pool)
        .await
        .expect("truncate mirrors");
    sqlx::query("insert into paperless_correspondents (id, name) values (4, 'ACME'), (2, 'bank')")
        .execute(&pool)
        .await
        .expect("seed correspondents");
    sqlx::query("insert into paperless_document_types (id, name) values (5, 'Invoice')")
        .execute(&pool)
        .await
        .expect("seed document types");

    let (reviewer, reviewer_csrf) =
        session_for_roles(&pool, "retry-reviewer", &[Role::Reviewer]).await;
    let (viewer, _) = session_for_roles(&pool, "retry-viewer", &[Role::Viewer]).await;
    let (auditor, _) = session_for_roles(&pool, "retry-auditor", &[Role::Auditor]).await;

    // Mock Paperless serving a thumbnail and a PDF preview for any id.
    let paperless = Router::new()
        .route(
            "/api/documents/{id}/thumb/",
            get(|| async { ([(header::CONTENT_TYPE, "image/webp")], "webp-bytes") }),
        )
        .route(
            "/api/documents/{id}/preview/",
            get(|| async { ([(header::CONTENT_TYPE, "application/pdf")], "%PDF-1.7") }),
        );
    let paperless_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let paperless_addr = paperless_listener.local_addr().unwrap();
    let paperless_handle = tokio::spawn(async move {
        axum::serve(paperless_listener, paperless).await.ok();
    });

    let (base, handle) = spawn_api_router(state.clone()).await;
    let client = no_redirect_client();
    let get_as = |session: &str, path: &str| {
        client.get(format!("{base}{path}")).header(
            reqwest::header::COOKIE,
            format!("{SESSION_COOKIE}={session}"),
        )
    };

    // #420: reviewers and viewers read the mirror; auditors cannot.
    for session in [&reviewer, &viewer] {
        let response = get_as(session, "/api/paperless/correspondents")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["items"],
            json!([{ "id": 4, "name": "ACME" }, { "id": 2, "name": "bank" }])
        );
        assert_eq!(body["truncated"], false);
    }
    let types: Value = get_as(&reviewer, "/api/paperless/document-types")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(types["items"], json!([{ "id": 5, "name": "Invoice" }]));
    for path in [
        "/api/paperless/correspondents",
        "/api/paperless/document-types",
    ] {
        let response = get_as(&auditor, path).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }

    // #445 preview proxy: 404 for unknown reviews, 503 while Paperless is
    // not configured, vetted bytes once it is.
    let review = seed_pending_review(&pool).await;
    let unknown = get_as(
        &reviewer,
        &format!("/api/reviews/{}/thumbnail", Uuid::now_v7()),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let unconfigured = get_as(&reviewer, &format!("/api/reviews/{review}/thumbnail"))
        .send()
        .await
        .unwrap();
    assert_eq!(unconfigured.status(), StatusCode::CONFLICT);
    let viewer_preview = get_as(&viewer, &format!("/api/reviews/{review}/preview"))
        .send()
        .await
        .unwrap();
    assert_eq!(viewer_preview.status(), StatusCode::FORBIDDEN);

    let actor = create_user_with_roles(
        &pool,
        &format!("retry-settings-{}", Uuid::now_v7().simple()),
        None,
        "hash",
        &[Role::Admin],
        None,
    )
    .await
    .expect("settings actor");
    let secret_id = upsert_encrypted_secret(
        &pool,
        &state.config.secret_key,
        "paperless-api-token",
        &SecretString::from("paperless-token".to_owned()),
        actor,
    )
    .await
    .expect("secret");
    let mut settings = RuntimeSettings::default();
    settings.paperless.base_url = format!("http://{paperless_addr}");
    settings.paperless.token_secret_id = Some(secret_id);
    let settings = settings.normalized();
    update_runtime_settings(&pool, &settings, actor)
        .await
        .expect("store settings");

    let thumb = get_as(&reviewer, &format!("/api/reviews/{review}/thumbnail"))
        .send()
        .await
        .unwrap();
    assert_eq!(thumb.status(), StatusCode::OK);
    assert_eq!(thumb.headers()[reqwest::header::CONTENT_TYPE], "image/webp");
    assert!(
        thumb.headers()[reqwest::header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .starts_with("default-src 'none'")
    );
    assert_eq!(thumb.bytes().await.unwrap().as_ref(), b"webp-bytes");
    let pdf = get_as(&reviewer, &format!("/api/reviews/{review}/preview"))
        .send()
        .await
        .unwrap();
    assert_eq!(pdf.status(), StatusCode::OK);
    assert_eq!(
        pdf.headers()[reqwest::header::CONTENT_TYPE],
        "application/pdf"
    );
    assert_eq!(
        pdf.headers()[reqwest::header::CACHE_CONTROL],
        "private, no-store"
    );

    // #445 retry: options for reviewers, validated overrides, one new run.
    let options: Value = get_as(&reviewer, "/api/reviews/retry-options")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        options["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|provider| provider["name"] == "ollama"),
        "{options}"
    );
    let viewer_options = get_as(&viewer, "/api/reviews/retry-options")
        .send()
        .await
        .unwrap();
    assert_eq!(viewer_options.status(), StatusCode::FORBIDDEN);

    // Retries need a job-backed review (the sibling aggregate closes the
    // original run before the new one is created).
    archivist_db::create_run_with_jobs_with_priority(
        &pool,
        44_501,
        &[Stage::Metadata],
        ProcessingMode::ManualReview,
        "test",
        "test",
        Some(0),
    )
    .await
    .expect("metadata run");
    let job = archivist_db::claim_jobs(&pool, 1, "retry-test-worker", 300)
        .await
        .expect("claim")
        .into_iter()
        .next()
        .expect("metadata job");
    let retry_target = archivist_db::create_review_item(
        &pool,
        &job,
        json!({ "correspondent": 4 }),
        json!([]),
        json!({}),
        "retry-test-worker",
    )
    .await
    .expect("review")
    .expect("review id");
    let post_retry = |body: Value| {
        client
            .post(format!("{base}/api/reviews/{retry_target}/retry"))
            .header(
                reqwest::header::COOKIE,
                format!("{SESSION_COOKIE}={reviewer}"),
            )
            .header("x-csrf-token", &reviewer_csrf)
            .json(&body)
    };
    let bad_provider = post_retry(json!({ "provider_name": "does-not-exist" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_provider.status(), StatusCode::BAD_REQUEST);
    let bad_prompt = post_retry(json!({ "prompt_id": Uuid::now_v7() }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_prompt.status(), StatusCode::BAD_REQUEST);
    assert_eq!(review_status_of(&pool, retry_target).await, "pending");

    let retried = post_retry(json!({ "provider_name": "ollama", "model": "qwen3:14b" }))
        .send()
        .await
        .unwrap();
    assert_eq!(retried.status(), StatusCode::OK);
    let body: Value = retried.json().await.unwrap();
    let run_id: Uuid = body["run_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(review_status_of(&pool, retry_target).await, "rejected");
    let payload: Value = sqlx::query_scalar("select payload from jobs where run_id = $1")
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("retry job");
    assert_eq!(
        payload["retry_overrides"],
        json!({ "provider_name": "ollama", "model": "qwen3:14b" })
    );
    let again = post_retry(json!({})).send().await.unwrap();
    assert_eq!(again.status(), StatusCode::CONFLICT);

    sqlx::query("delete from settings where key = 'runtime'")
        .execute(&pool)
        .await
        .expect("restore default settings");
    handle.abort();
    paperless_handle.abort();
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn audit_export_is_audited_and_its_deadline_frees_the_pool() {
    // #394
    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    for index in 0..40 {
        append_audit(
            &pool,
            AuditEventInput {
                event_type: "test.export_fixture".to_owned(),
                actor_type: "system".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: Some(index),
                before: None,
                after: None,
                metadata: None,
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await
        .expect("audit fixture");
    }

    // A client that never reads: the channel fills, the deadline fires,
    // and the task ends instead of waiting forever.
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_audit_export(&pool, tx, std::time::Duration::from_millis(300)),
    )
    .await
    .expect("export must stop at its deadline");
    let mut delivered = 0;
    while rx.recv().await.is_some() {
        delivered += 1;
    }
    assert!(delivered <= 5, "only the buffered chunks were produced");
    // Keyset pages release their connection: the whole pool is available.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(pool.acquire().await.expect("acquire"));
        }
    })
    .await
    .expect("no connection is pinned by the aborted export");

    // Full export through the route, with an audit trail entry.
    let suffix = Uuid::now_v7().simple().to_string();
    let auditor = create_user_with_roles(
        &pool,
        &format!("export-auditor-{suffix}"),
        None,
        "hash",
        &[Role::Auditor],
        None,
    )
    .await
    .expect("auditor");
    let session_token = random_token();
    create_session(
        &pool,
        auditor,
        &hash_token(&session_token),
        &hash_token(&random_token()),
        Utc::now() + Duration::hours(1),
    )
    .await
    .expect("session");
    let (base, handle) = spawn_api_router(state).await;
    let csv = no_redirect_client()
        .get(format!("{base}/api/audit/export.csv"))
        .header(
            reqwest::header::COOKIE,
            format!("{SESSION_COOKIE}={session_token}"),
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // header + 40 fixtures + the export event itself (+ login-free setup)
    assert!(csv.lines().count() >= 42, "{}", csv.lines().count());
    let exported: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where event_type = 'audit.exported' and actor_id = $1",
    )
    .bind(auditor.to_string())
    .fetch_one(&pool)
    .await
    .expect("export audit");
    assert_eq!(exported, 1);
    handle.abort();
}
