//! #440: HTTP-level harness tests (router() + tower oneshot).

use crate::test_support::*;
use crate::*;

fn policy_method(verb: route_policy::Verb) -> Method {
    match verb {
        route_policy::Verb::Get => Method::GET,
        route_policy::Verb::Post => Method::POST,
        route_policy::Verb::Put => Method::PUT,
        route_policy::Verb::Patch => Method::PATCH,
        route_policy::Verb::Delete => Method::DELETE,
    }
}

/// Concrete request path for a route template. Unknown ids keep the
/// handlers on their cheap not-found / validation paths.
fn concrete_path(template: &str) -> String {
    template
        .replace("{id}", &Uuid::now_v7().to_string())
        .replace("{name}", "harness-missing-provider")
        .replace("{document_id}", "440440")
        .replace("{paperless_document_id}", "440440")
}

/// 401, or a 403 produced by the route policy (as opposed to a
/// handler's own resource-level 403 such as a foreign chat session).
fn is_auth_or_policy_denial(response: &TestResponse, policy: &route_policy::RoutePolicy) -> bool {
    if response.status == StatusCode::UNAUTHORIZED {
        return true;
    }
    if response.status != StatusCode::FORBIDDEN {
        return false;
    }
    let error = response.body["error"].as_str().unwrap_or_default();
    error == "insufficient permissions"
        || error == "this endpoint requires a user session"
        || matches!(
            policy.auth,
            route_policy::AuthKinds::SessionOnly(message) if message == error
        )
}

fn is_router_not_found(response: &TestResponse) -> bool {
    response.status == StatusCode::NOT_FOUND && response.body["error"] == "not found"
}

/// Exercises the real `router()` (nesting included) instead of the limiter
/// struct, so a prefix-stripping regression is caught. Invalid JSON bodies
/// are rejected by the extractor before any database access. #385, #440
#[tokio::test]
async fn auth_routes_are_rate_limited_through_the_nested_router() {
    let mut state = api_text_test_state();
    state.auth_rate_limiter = Arc::new(AuthRateLimiter::new(2, 3600));
    let api = TestApi::new(state);
    let mut statuses = Vec::new();
    for path in ["login", "paperless-login", "login"] {
        let response = api
            .send(
                Method::POST,
                &format!("/api/auth/{path}"),
                &Principal::Anonymous,
                Some(json!({})),
            )
            .await;
        statuses.push(response.status);
        if response.status == StatusCode::TOO_MANY_REQUESTS {
            assert!(response.headers.contains_key(header::RETRY_AFTER));
            assert!(response.body["error"].is_string(), "JSON 429 body");
        }
    }
    assert_ne!(statuses[0], 429, "first request must pass the limiter");
    assert_ne!(statuses[1], 429, "second request must pass the limiter");
    assert_eq!(statuses[2], 429, "third request must be rate limited");
}

/// Every declared authenticated route is mounted behind the auth
/// middleware (anonymous -> 401 JSON, never the SPA or a router 404),
/// and undeclared paths still get the JSON 404. #440, #442
#[tokio::test]
async fn every_declared_route_is_mounted_behind_authentication() {
    let api = TestApi::new(api_text_test_state());
    for policy in route_policy::ROUTE_POLICIES {
        if policy.auth == route_policy::AuthKinds::Unauthenticated {
            continue;
        }
        let path = concrete_path(policy.path);
        let response = api
            .send(
                policy_method(policy.verb),
                &path,
                &Principal::Anonymous,
                None,
            )
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{:?} {path}",
            policy.verb
        );
        assert_eq!(response.body["error"], "authentication required");
    }
    let unknown = api
        .send(
            Method::GET,
            "/api/not-a-declared-route",
            &Principal::Anonymous,
            None,
        )
        .await;
    assert!(is_router_not_found(&unknown));
}

/// Permission matrix token vs. session over the whole route table:
/// anonymous, admin session, role-less session, fully scoped token and a
/// token holding only a retired scope. #440, #442
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn harness_route_permission_matrix_token_vs_session() {
    use route_policy::{Access, AuthKinds, ROUTE_POLICIES, TOKEN_SCOPES};

    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    let (admin, admin_session) = harness_user(&pool, "admin", &[Role::Admin]).await;
    let (_, roleless_session) = harness_user(&pool, "roleless", &[]).await;
    let full_token = harness_token(&pool, admin, TOKEN_SCOPES).await;
    // Retired scope (#442): accepted before, grants nothing now.
    let legacy_token = harness_token(&pool, admin, &["users:manage"]).await;
    let api = TestApi::new(state);

    for policy in ROUTE_POLICIES {
        if policy.auth == AuthKinds::Unauthenticated {
            continue;
        }
        let method = policy_method(policy.verb);
        let label = format!("{method} {}", policy.path);
        // Logging out would revoke the shared sessions; covered below.
        let skip_session = policy.path == "/api/auth/logout";

        if !skip_session {
            let response = api
                .send(
                    method.clone(),
                    &concrete_path(policy.path),
                    &admin_session,
                    None,
                )
                .await;
            assert!(
                !is_auth_or_policy_denial(&response, policy),
                "admin session {label}: {} {}",
                response.status,
                response.body
            );
            assert!(!is_router_not_found(&response), "admin session {label}");

            let response = api
                .send(
                    method.clone(),
                    &concrete_path(policy.path),
                    &roleless_session,
                    None,
                )
                .await;
            match policy.access {
                Access::Require(_) => {
                    assert_eq!(response.status, StatusCode::FORBIDDEN, "roleless {label}");
                    assert_eq!(response.body["error"], "insufficient permissions");
                }
                Access::Authenticated => {
                    assert_ne!(response.status, StatusCode::FORBIDDEN, "roleless {label}")
                }
                Access::Public => unreachable!("public routes are skipped"),
            }
        }

        let full = api
            .send(
                method.clone(),
                &concrete_path(policy.path),
                &full_token,
                None,
            )
            .await;
        let legacy = api
            .send(method, &concrete_path(policy.path), &legacy_token, None)
            .await;
        match (policy.access, policy.auth) {
            (_, AuthKinds::SessionOnly(message)) => {
                assert_eq!(full.status, StatusCode::FORBIDDEN, "token {label}");
                let expected = match policy.access {
                    Access::Require(permission)
                        if route_policy::token_scope_for_permission(permission).is_none() =>
                    {
                        "insufficient permissions"
                    }
                    _ => message,
                };
                assert_eq!(full.body["error"], expected, "token {label}");
                assert_eq!(legacy.status, StatusCode::FORBIDDEN, "legacy {label}");
            }
            (Access::Require(_), AuthKinds::SessionOrToken) => {
                assert!(
                    !is_auth_or_policy_denial(&full, policy),
                    "token {label}: {} {}",
                    full.status,
                    full.body
                );
                assert!(!is_router_not_found(&full), "token {label}");
                assert_eq!(legacy.status, StatusCode::FORBIDDEN, "legacy {label}");
            }
            (Access::Authenticated, AuthKinds::SessionOrToken) => {
                assert!(full.status.is_success(), "token {label}: {}", full.status);
                assert!(legacy.status.is_success(), "legacy {label}");
            }
            (Access::Public, _) | (_, AuthKinds::Unauthenticated) => {
                unreachable!("public routes are skipped")
            }
        }
    }

    // Token logout is a no-op; session logout revokes the session.
    let logout = api
        .send(Method::POST, "/api/auth/logout", &roleless_session, None)
        .await;
    assert_eq!(logout.status, StatusCode::OK);
    let after = api
        .send(Method::GET, "/api/auth/me", &roleless_session, None)
        .await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);

    // CSRF is still enforced before the route policy.
    let Principal::Session { token, .. } = &admin_session else {
        unreachable!()
    };
    let no_csrf = api
        .send(
            Method::POST,
            "/api/batches/rerun-failed",
            &Principal::Session {
                token: token.clone(),
                csrf: "wrong".to_owned(),
            },
            None,
        )
        .await;
    assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    assert_eq!(no_csrf.body["error"], "invalid CSRF token");
}

/// Expected domain conditions are 4xx, and audit events written deep in
/// DB helpers inherit the request's source IP / User-Agent. #441
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn harness_domain_errors_are_4xx_and_audit_events_carry_request_context() {
    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    let (_, admin_session) = harness_user(&pool, "admin", &[Role::Admin]).await;
    let (target, _) = harness_user(&pool, "target", &[Role::Viewer]).await;
    let api = TestApi::new(state);
    let unknown = Uuid::now_v7();

    for (path, body, message) in [
        (
            format!("/api/users/{unknown}/roles"),
            json!({ "roles": ["viewer"] }),
            "user does not exist",
        ),
        (
            format!("/api/users/{unknown}/disable"),
            json!({}),
            "user does not exist",
        ),
        (
            format!("/api/api-tokens/{unknown}/rotate"),
            json!({}),
            "API token not found or already revoked",
        ),
        (
            format!("/api/prompts/{unknown}/activate"),
            json!({}),
            "prompt does not exist",
        ),
    ] {
        let response = api
            .send(Method::POST, &path, &admin_session, Some(body))
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(response.body["error"], message, "{path}");
    }

    // No Paperless token configured: operator state, not a 500.
    let consistency = api
        .send(
            Method::GET,
            "/api/paperless/consistency",
            &admin_session,
            None,
        )
        .await;
    assert_eq!(consistency.status, StatusCode::CONFLICT);
    assert_eq!(
        consistency.body["error"],
        "Paperless token is not configured for the active archive profile"
    );

    let changed = api
        .send(
            Method::POST,
            &format!("/api/users/{target}/roles"),
            &admin_session,
            Some(json!({ "roles": ["reviewer"] })),
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK);
    let row = sqlx::query(
        "select source_ip, user_agent from audit_events where event_type = 'user.roles_changed' and after->>'user_id' = $1",
    )
    .bind(target.to_string())
    .fetch_one(&pool)
    .await
    .expect("roles audit event");
    let source_ip: Option<String> = row.try_get("source_ip").unwrap();
    let user_agent: Option<String> = row.try_get("user_agent").unwrap();
    assert_eq!(source_ip.as_deref(), Some("198.51.100.7"));
    assert_eq!(user_agent.as_deref(), Some(HARNESS_USER_AGENT));
    let integrity = verify_audit_integrity(&pool)
        .await
        .expect("verify audit chain");
    assert!(
        integrity.ok,
        "request context is bound into the audit hash chain"
    );
}

async fn spawn_mock_paperless(documents: Value) -> (String, tokio::task::JoinHandle<()>) {
    async fn list(State(documents): State<Value>) -> Json<Value> {
        let count = documents.as_array().map_or(0, Vec::len);
        Json(json!({ "count": count, "next": null, "previous": null, "results": documents }))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/api/documents/", get(list))
        .with_state(documents);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{address}/"), handle)
}

/// Consistency through the router against real inventory rows and a
/// mocked Paperless: typed document dates decode (#386) and every
/// difference class is reported. #440
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn harness_consistency_compares_real_inventory_rows() {
    let Some(state) = security_db_state().await else {
        return;
    };
    let pool = state.pool.clone();
    let (admin, _) = harness_user(&pool, "admin", &[Role::Admin]).await;
    let (_, viewer_session) = harness_user(&pool, "viewer", &[Role::Viewer]).await;
    let (paperless_url, paperless_handle) = spawn_mock_paperless(json!([
        { "id": 440001, "title": "Rechnung", "created": "2026-09-27T00:00:00+02:00",
          "tags": [1, 3], "correspondent": 7, "document_type": 9 },
        { "id": 440002, "title": "Remote title", "created": "2026-09-01",
          "tags": [], "correspondent": null, "document_type": null },
        { "id": 440003, "title": "Only remote", "created": null,
          "tags": [], "correspondent": null, "document_type": null }
    ]))
    .await;
    sqlx::query("delete from document_inventory")
        .execute(&pool)
        .await
        .expect("clear inventory");
    sqlx::query(
        r#"
            insert into document_inventory (
              paperless_document_id, title, current_tag_ids, correspondent_id,
              document_type_id, document_date
            ) values
              (440001, 'Rechnung', '{3,1}', 7, 9, date '2026-09-27'),
              (440002, 'Local title', '{}', null, null, date '2026-09-01'),
              (440004, 'Only local', '{}', null, null, null)
            "#,
    )
    .execute(&pool)
    .await
    .expect("insert inventory fixtures");

    let original = get_runtime_settings(&pool).await.expect("settings");
    let secret_id = upsert_encrypted_secret(
        &pool,
        &state.config.secret_key,
        &format!("harness-paperless-{}", Uuid::now_v7().simple()),
        &SecretString::from("harness-paperless-token".to_owned()),
        admin,
    )
    .await
    .expect("paperless secret");
    let mut settings = original.clone();
    settings.paperless.base_url = paperless_url;
    settings.paperless.token_secret_id = Some(secret_id);
    settings.paperless.active_archive = String::new();
    settings.paperless.archive_profiles = Vec::new();
    update_runtime_settings(&pool, &settings, admin)
        .await
        .expect("point settings at mock Paperless");

    let api = TestApi::new(state);
    let response = api
        .send(
            Method::GET,
            "/api/paperless/consistency",
            &viewer_session,
            None,
        )
        .await;
    // Restore before asserting so a failure cannot leak the mock
    // Paperless configuration into later tests.
    update_runtime_settings(&pool, &original, admin)
        .await
        .expect("restore settings");
    sqlx::query(
        "delete from document_inventory where paperless_document_id between 440001 and 440004",
    )
    .execute(&pool)
    .await
    .expect("clean inventory fixtures");
    paperless_handle.abort();

    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["documents_checked"], 3);
    assert_eq!(response.body["missing_local"], json!([440003]));
    assert_eq!(response.body["stale_local"], json!([440004]));
    assert_eq!(
        response.body["mismatches"],
        json!([{ "paperless_document_id": 440002, "fields": ["title"] }])
    );
    assert_eq!(response.body["ok"], false);
}
