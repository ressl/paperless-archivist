//! Authentication, session, token, password and rate-limit unit tests.

use crate::test_support::*;
use crate::*;

#[test]
fn metrics_authorization_contract_is_dedicated_and_non_disclosing() {
    let headers = HeaderMap::new();
    let disabled =
        authorize_metrics_request(None, &headers).expect_err("an unset token disables metrics");
    assert_eq!(disabled.status, StatusCode::SERVICE_UNAVAILABLE);

    let secret_value = "metrics-test-secret";
    let expected = SecretString::from(secret_value.to_owned());
    let missing = authorize_metrics_request(Some(&expected), &headers)
        .expect_err("a configured endpoint requires bearer auth");
    assert_eq!(missing.status, StatusCode::UNAUTHORIZED);

    let mut wrong_headers = HeaderMap::new();
    wrong_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer wrong-token"),
    );
    let wrong = authorize_metrics_request(Some(&expected), &wrong_headers)
        .expect_err("a wrong bearer token is rejected");
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);

    let mut valid_headers = HeaderMap::new();
    valid_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {secret_value}"))
            .expect("test authorization header"),
    );
    authorize_metrics_request(Some(&expected), &valid_headers)
        .expect("the dedicated metrics token is accepted");

    for error in [disabled, missing, wrong] {
        assert!(
            !format!("{error:?}").contains(secret_value),
            "metrics errors must never disclose the configured token"
        );
    }
}

#[test]
fn tls_mode_marks_session_and_csrf_cookies_secure() {
    let session = build_cookie(SESSION_COOKIE, "session", true, true, 12).to_string();
    let csrf = build_cookie(CSRF_COOKIE, "csrf", false, true, 12).to_string();
    let local_session = build_cookie(SESSION_COOKIE, "session", true, false, 12).to_string();

    assert!(session.contains("; Secure"));
    assert!(session.contains("; HttpOnly"));
    assert!(csrf.contains("; Secure"));
    assert!(!csrf.contains("; HttpOnly"));
    assert!(!local_session.contains("; Secure"));
}

#[tokio::test]
async fn session_listing_rejects_every_api_token_scope_without_metadata() {
    let user_id = Uuid::new_v4();
    for scopes in [
        vec!["runs:read"],
        vec!["users:manage"],
        vec!["users:manage", "audit:read", "settings:read"],
    ] {
        let auth = auth_context_for_session_listing(false, user_id, Vec::new(), scopes);
        let error = session_listing_user_filter(&auth)
            .expect_err("an API token must never list browser sessions");
        assert_eq!(error.status, StatusCode::FORBIDDEN);

        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read error response");
        let body: Value = serde_json::from_slice(&body).expect("parse error response");
        assert_eq!(
            body,
            json!({ "error": "session listing requires a user session" })
        );
    }
}

#[test]
fn session_listing_preserves_cookie_user_and_admin_visibility() {
    let user_id = Uuid::new_v4();
    let user = auth_context_for_session_listing(true, user_id, vec![Role::Viewer], Vec::new());
    assert_eq!(session_listing_user_filter(&user).unwrap(), Some(user_id));

    let admin = auth_context_for_session_listing(true, user_id, vec![Role::Admin], Vec::new());
    assert_eq!(session_listing_user_filter(&admin).unwrap(), None);
}

#[test]
fn last_enabled_admin_error_maps_to_stable_conflict() {
    let error = ApiError::from(anyhow::Error::new(LastEnabledAdminError));
    assert_eq!(error.status, StatusCode::CONFLICT);
    assert_eq!(
        error.message,
        "at least one enabled administrator is required"
    );
}

#[test]
fn user_identity_errors_map_to_stable_safe_responses() {
    let conflict = ApiError::from(anyhow::Error::new(UserIdentityConflictError));
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(conflict.message, "username or email is already assigned");

    let ambiguous = ApiError::from(anyhow::Error::new(AmbiguousUserIdentityLinkError));
    assert_eq!(ambiguous.status, StatusCode::CONFLICT);
    assert_eq!(
        ambiguous.message,
        "OIDC identity matches multiple local accounts"
    );

    let invalid = ApiError::from(anyhow::Error::new(InvalidUserIdentityError));
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.message, "username must not be blank");
}

#[test]
fn validates_password_strength() {
    assert!(validate_password_strength("short").is_err());
    assert!(validate_password_strength("            ").is_err());
    assert!(validate_password_strength("long-enough-password").is_ok());
}

#[test]
fn auth_rate_limiter_allows_within_capacity_and_blocks_burst() {
    let limiter = AuthRateLimiter::new(3, 60);
    let ip: IpAddr = "203.0.113.7".parse().unwrap();
    let start = Instant::now();
    assert!(limiter.check(ip, start).is_ok());
    assert!(limiter.check(ip, start).is_ok());
    assert!(limiter.check(ip, start).is_ok());
    // Fourth burst request inside the same instant should be rate-limited.
    let retry = limiter.check(ip, start).unwrap_err();
    assert!(retry >= 1, "retry-after seconds must be positive: {retry}");
}

#[test]
fn auth_rate_limiter_refills_over_time() {
    let limiter = AuthRateLimiter::new(2, 60);
    let ip: IpAddr = "203.0.113.7".parse().unwrap();
    let start = Instant::now();
    assert!(limiter.check(ip, start).is_ok());
    assert!(limiter.check(ip, start).is_ok());
    assert!(limiter.check(ip, start).is_err());
    // Half the window later, the bucket should be at 1 token again.
    let later = start + std::time::Duration::from_secs(31);
    assert!(limiter.check(ip, later).is_ok());
}

#[test]
fn auth_rate_limiter_buckets_are_per_ip() {
    let limiter = AuthRateLimiter::new(1, 60);
    let a: IpAddr = "203.0.113.7".parse().unwrap();
    let b: IpAddr = "203.0.113.8".parse().unwrap();
    let start = Instant::now();
    assert!(limiter.check(a, start).is_ok());
    // Different IP gets its own bucket.
    assert!(limiter.check(b, start).is_ok());
    // Same IP burst is rejected.
    assert!(limiter.check(a, start).is_err());
}

#[test]
fn auth_rate_limiter_zero_capacity_is_disabled() {
    let limiter = AuthRateLimiter::new(0, 60);
    let ip: IpAddr = "203.0.113.7".parse().unwrap();
    for _ in 0..1000 {
        assert!(limiter.check(ip, Instant::now()).is_ok());
    }
}

#[test]
fn validates_api_token_scopes() {
    assert!(
        validate_api_token_scopes(&[
            "runs:read".to_owned(),
            "reviews:write".to_owned(),
            "audit:read".to_owned()
        ])
        .is_ok()
    );

    let empty_error = validate_api_token_scopes(&[]).expect_err("empty scopes are rejected");
    assert_eq!(empty_error.status, StatusCode::BAD_REQUEST);

    let invalid_error =
        validate_api_token_scopes(&["admin:*".to_owned()]).expect_err("unknown scopes fail");
    assert_eq!(invalid_error.status, StatusCode::BAD_REQUEST);

    // #442: scopes whose every route is session-only could never
    // authorize a request, so they are no longer issued.
    for unusable in ["chat:write", "settings:write", "users:manage"] {
        let error = validate_api_token_scopes(&[unusable.to_owned()])
            .expect_err("unusable scope is rejected");
        assert_eq!(error.status, StatusCode::BAD_REQUEST, "{unusable}");
    }
}

#[test]
fn permission_scopes_are_explicit_and_accepted() {
    for permission in route_policy::ALL_PERMISSIONS {
        if let Some(scope) = route_policy::token_scope_for_permission(permission) {
            assert!(
                validate_api_token_scopes(&[scope.to_owned()]).is_ok(),
                "permission {permission:?} maps to unsupported scope"
            );
        }
    }
    assert_eq!(
        route_policy::token_scope_for_permission(Permission::ManageUsers),
        None
    );
    // Legacy scopes on existing tokens grant nothing, even for an admin.
    assert!(
        effective_token_scopes(
            &["users:manage".to_owned(), "settings:write".to_owned()],
            &[Role::Admin]
        )
        .is_empty()
    );
}

#[test]
fn api_token_expiry_policy_defaults_and_caps() {
    let settings = RuntimeSettings::default().normalized();
    let default_expiry = api_token_expiry(&settings, None).expect("default expiry");
    assert!(default_expiry.is_some());

    let too_long = api_token_expiry(&settings, Some(10_000)).expect_err("max ttl applies");
    assert_eq!(too_long.status, StatusCode::BAD_REQUEST);

    let no_expiry = api_token_expiry(
        &RuntimeSettings {
            security: archivist_core::SecuritySettings {
                api_token_expiry_required: false,
                ..Default::default()
            },
            ..Default::default()
        },
        Some(0),
    )
    .expect("optional expiry allowed");
    assert!(no_expiry.is_none());
}

#[test]
fn paperless_bridge_usernames_cannot_collide_with_local_names() {
    assert_eq!(paperless_bridge_username("rressl"), "paperless-rressl");
    assert_eq!(
        paperless_bridge_username("User.Name@example.com"),
        "paperless-user.name@example.com"
    );
}

#[test]
fn paperless_bridge_subject_scopes_user_id_to_instance() {
    let instance_a = Url::parse("https://paperless-a.example/api/").unwrap();
    let instance_b = Url::parse("https://paperless-b.example/api/").unwrap();
    assert_eq!(
        paperless_user_subject(&instance_a, 42),
        paperless_user_subject(&instance_a, 42)
    );
    assert_ne!(
        paperless_user_subject(&instance_a, 42),
        paperless_user_subject(&instance_a, 43)
    );
    assert_ne!(
        paperless_user_subject(&instance_a, 42),
        paperless_user_subject(&instance_b, 42)
    );
}
