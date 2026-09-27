use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use archivist_ai::{
    AiResponse, AnthropicClient, ChatRequest, MetadataEnvelopeError, MetadataParseStatus,
    MineruClient, OllamaClient, OllamaLoadedModel, OllamaModel, OpenAiCompatibleClient,
    PromptLanguageContext, TextProvider, parse_metadata_suggestion, prompt_for_metadata,
    schema_for_metadata,
};
use archivist_apply::{
    ApplyRequest, ReviewApplyConflict, ReviewApplyPrecondition, ReviewTagOperations, apply_document,
};
use archivist_config::AppConfig;
use archivist_core::{
    AiProviderKind, AiProviderSettings, AuditEventInput, DashboardProviderCostSummary,
    DashboardRange, DashboardStats, DocumentChatSource, DocumentInventoryItem, DocumentPatch,
    EffectiveTuning, MetadataFieldFlags, Permission, ProcessingMode, ProviderTuning,
    ProviderUsageStats, Role, RuntimeSettings, Stage, WorkflowRules, build_document_chat_prompt,
    detect_document_language, document_chat_snippet, document_chat_terms,
    prefilter_allowed_list_lower, roles_have_permission, same_http_origin,
    score_document_chat_source,
};
use archivist_db::{
    AmbiguousUserIdentityLinkError, AuditRequestContext, AuthUser, DbPool, DocumentChatCandidate,
    InvalidUserIdentityError, LastEnabledAdminError, MetadataApplyAudit, MetadataArtifact,
    MetadataReviewItem, MetadataRunHeader, NotFoundError, OidcUserInput, ProviderBucketEntry,
    ReviewDecisionError, ReviewItemRecord, UserIdentityConflictError, append_audit,
    apply_security_retention, connect, consume_oidc_login_state, count_reviews,
    create_document_chat_session, create_oidc_login_state, create_run_with_jobs_with_priority,
    create_runs_for_documents, create_session, create_user_with_roles, dashboard_bucket_labels,
    dashboard_range_start, document_chat_session_visible, failed_document_ids, find_api_token,
    find_or_create_paperless_bridge_user, find_paperless_bridge_user, find_session,
    find_user_for_login, get_backlog_counts, get_dashboard_live_status, get_dashboard_stats,
    get_runtime_settings, has_any_user, hash_token, insert_document_chat_message,
    insert_document_chat_sources, latest_apply_audit_for_run, latest_metadata_artifact_for_run,
    latest_metadata_run_for_document, list_allowed_named_entities, list_allowed_tag_names,
    list_audit_events, list_custom_fields, list_document_chat_messages,
    list_document_chat_sessions, list_inventory, list_prompt_experiments, list_prompt_usage,
    list_prompts, list_reviews, list_secret_references, list_sessions, list_users,
    metadata_review_items_for_run, metrics_snapshot as db_metrics_snapshot, migrate,
    paperless_sync_cursor, provider_bucket_entries, queue_missing_pipeline, queue_missing_stage,
    read_metric_counters, record_login_failure, record_login_success, recover_stale_leases,
    recover_stuck_runs, recovery_candidates, resolve_secret, review_decision,
    revoke_session_by_admin, rotate_api_token, search_document_chat_candidates, set_user_enabled,
    set_user_roles, statistics_throughput_rows, statistics_usage_rows,
    update_paperless_sync_cursor, update_runtime_settings, update_user_password_hash,
    upsert_encrypted_secret, upsert_inventory_item, upsert_oidc_user,
    upsert_paperless_custom_field, upsert_paperless_named_entity, upsert_paperless_tag,
    verify_audit_integrity, with_audit_request_context,
};
use archivist_paperless::{PaperlessClient, PaperlessTag};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, Params};
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use cookie::{Cookie, SameSite};
use jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER as JWT_CRYPTO_PROVIDER;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use rand::RngCore;
use reqwest::Client as HttpClient;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha384, Sha512};
use sqlx::Row;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{Span, info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;
use uuid::Uuid;

mod route_policy;
use route_policy::{
    authorize_route, effective_token_scopes, require_user_session, validate_api_token_scopes,
};

mod auth;
mod error;
mod oidc;
mod routes;
mod ssrf;
mod state;

use auth::*;
use error::*;
use oidc::*;
use routes::*;
use ssrf::*;
use state::*;

#[tokio::main]
async fn main() -> Result<()> {
    init_jwt_crypto_provider();

    let config = AppConfig::from_env();
    config.validate()?;
    init_tracing(&config.log_level);

    let pool = connect(
        config.database_url.expose_secret(),
        config.db_max_connections,
    )
    .await?;
    migrate(&pool).await?;
    ensure_bootstrap_admin(&pool, &config).await?;

    if !config.cookie_secure {
        warn!(
            "ARCHIVIST_COOKIE_SECURE is false: session and CSRF cookies are not marked Secure \
             and will be sent over plain HTTP. Set ARCHIVIST_COOKIE_SECURE=true in any \
             production deployment behind TLS."
        );
    }

    let state = AppState {
        pool,
        config: Arc::new(config.clone()),
        auth_rate_limiter: Arc::new(AuthRateLimiter::new(
            config.auth_rate_limit,
            config.auth_rate_limit_window_seconds,
        )),
    };
    let app = router(state);
    let addr: SocketAddr = config
        .http_addr
        .parse()
        .context("parse ARCHIVIST_HTTP_ADDR")?;
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "paperless archivist API listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn init_jwt_crypto_provider() {
    let _ = JWT_CRYPTO_PROVIDER.install_default();
}

fn init_tracing(filter: &str) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .json()
        .init();
}

fn router(state: AppState) -> Router {
    // HSTS is only meaningful (and safe) over TLS; reuse the app's existing
    // "behind TLS" signal so local HTTP dev never emits it. #288
    let cookie_secure = state.config.cookie_secure;
    // Most API writes are tiny JSON bodies. Large payloads
    // (settings, prompts, chat) are overridden per-route below.
    const DEFAULT_BODY_LIMIT: usize = 64 * 1024;
    const LARGE_BODY_LIMIT: usize = 256 * 1024;
    const SETTINGS_BODY_LIMIT: usize = 1024 * 1024;

    let protected = Router::new()
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .route("/auth/change-password", post(change_password))
        .route("/auth/sessions", get(sessions))
        .route("/auth/sessions/{id}/revoke", post(revoke_session_endpoint))
        .route(
            "/settings",
            get(settings)
                .put(update_settings)
                .layer(DefaultBodyLimit::max(SETTINGS_BODY_LIMIT)),
        )
        .route("/settings/test-paperless", post(test_paperless))
        .route("/notifications/test", post(test_notification))
        .route("/model-providers/test", post(test_provider))
        .route(
            "/model-providers/{name}/models",
            post(model_provider_models),
        )
        .route("/ai/runtime-hints", get(ai_runtime_hints))
        .route("/secret-references", get(secret_references))
        .route(
            "/prompts",
            get(prompts)
                .post(create_prompt_endpoint)
                .layer(DefaultBodyLimit::max(LARGE_BODY_LIMIT)),
        )
        .route("/prompts/usage", get(prompt_usage))
        .route("/prompts/experiments", get(prompt_experiments))
        .route(
            "/prompts/test",
            post(test_prompt_endpoint).layer(DefaultBodyLimit::max(LARGE_BODY_LIMIT)),
        )
        .route("/prompts/{id}/activate", post(activate_prompt_endpoint))
        .route("/paperless/sync-metadata", post(sync_paperless))
        .route("/paperless/consistency", get(paperless_consistency))
        .route(
            "/paperless/correspondents",
            get(paperless_correspondent_options),
        )
        .route(
            "/paperless/document-types",
            get(paperless_document_type_options),
        )
        .route(
            "/paperless/completion-tags/reconcile",
            post(reconcile_completion_tags),
        )
        .route("/dashboard", get(dashboard))
        .route("/dashboard/live", get(dashboard_live))
        .route("/statistics", get(statistics))
        .route("/workflow/mode", put(update_workflow_mode))
        .route("/workflow/controls", patch(update_workflow_controls))
        .route("/inventory", get(inventory))
        .route("/inventory/duplicates", get(inventory_duplicates))
        .route("/inventory/export", get(inventory_export))
        .route(
            "/inventory/views",
            get(inventory_views).post(create_inventory_view_endpoint),
        )
        .route(
            "/inventory/views/{id}",
            put(update_inventory_view_endpoint).delete(delete_inventory_view_endpoint),
        )
        .route(
            "/inventory/{document_id}/metadata-trace",
            get(inventory_metadata_trace),
        )
        .route(
            "/chat/sessions",
            get(chat_sessions).post(create_chat_session),
        )
        .route(
            "/chat/sessions/{id}",
            get(chat_messages)
                .patch(rename_chat_session)
                .delete(delete_chat_session),
        )
        .route(
            "/chat/sessions/{id}/messages",
            post(post_chat_message).layer(DefaultBodyLimit::max(LARGE_BODY_LIMIT)),
        )
        .route(
            "/chat/sessions/{id}/messages/stream",
            post(post_chat_message_stream).layer(DefaultBodyLimit::max(LARGE_BODY_LIMIT)),
        )
        .route(
            "/documents/{paperless_document_id}/trigger",
            post(trigger_document),
        )
        .route("/batches/ocr", post(queue_ocr_batch))
        .route("/batches/full", post(queue_full_batch))
        .route("/batches/rerun", post(rerun_batch))
        .route("/batches/rerun-failed", post(rerun_failed_batch))
        .route("/reviews", get(reviews))
        .route("/reviews/batch", post(batch_review))
        .route("/reviews/auto-fix-preview", post(auto_fix_preview))
        .route("/reviews/auto-fix", post(auto_fix_bulk))
        .route("/reviews/{id}/approve", post(approve_review))
        .route("/reviews/{id}/reject", post(reject_review))
        .route("/reviews/{id}/edit", post(edit_review))
        .route("/reviews/{id}/auto-fix", post(auto_fix_single))
        .route("/reviews/retry-options", get(review_retry_options))
        .route("/reviews/{id}/retry", post(retry_review))
        .route("/reviews/{id}/thumbnail", get(review_thumbnail))
        .route("/reviews/{id}/preview", get(review_document_preview))
        .route("/operations/recovery", get(recovery_status))
        .route(
            "/operations/recovery/stale-leases",
            post(recover_stale_leases_endpoint),
        )
        .route(
            "/operations/recovery/stuck-runs",
            post(recover_stuck_runs_endpoint),
        )
        .route("/operations/unblock-jobs", post(unblock_jobs_endpoint))
        .route(
            "/operations/provider-cooldowns",
            get(provider_cooldowns_endpoint),
        )
        .route(
            "/operations/provider-cooldowns/clear",
            post(clear_provider_cooldowns_endpoint),
        )
        .route(
            "/operations/release-scheduled-retries",
            post(release_scheduled_retries_endpoint),
        )
        .route("/audit", get(audit_events))
        .route("/audit/export.csv", get(audit_export))
        .route("/audit/integrity", get(audit_integrity))
        .route("/audit/{id}", get(audit_event_detail))
        .route("/audit/retention/apply", post(apply_audit_retention))
        .route("/users", get(users).post(create_user))
        .route("/users/{id}/enable", post(enable_user))
        .route("/users/{id}/disable", post(disable_user))
        .route("/users/{id}/roles", post(update_user_roles_endpoint))
        .route("/users/{id}/reset-password", post(reset_user_password))
        .route("/api-tokens", get(api_tokens).post(create_api_token))
        .route("/api-tokens/{id}/rotate", post(rotate_api_token_endpoint))
        .route("/api-tokens/{id}", delete(revoke_api_token))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // Nested routers without their own fallback inherit the outer SPA
        // fallback, so `/api/typo` used to answer 200 + index.html. The
        // fallback is outside `route_layer`, so unknown paths get a JSON 404
        // without an authentication round-trip. #399
        .fallback(api_not_found)
        .layer(middleware::map_response(json_error_body))
        .layer(DefaultBodyLimit::max(DEFAULT_BODY_LIMIT));

    let static_dir = state.config.static_dir.clone();
    let spa = ServeDir::new(&static_dir)
        .not_found_service(ServeFile::new(format!("{static_dir}/index.html")));

    // Rate-limited public auth endpoints. Wrapping them in a sub-router
    // lets us scope the per-IP token bucket strictly to /api/auth/*.
    let auth_public = Router::new()
        .route("/login", post(login))
        .route("/paperless-login", post(paperless_login))
        .route("/oidc/config", get(oidc_config))
        .route("/oidc/login", get(oidc_login))
        .route("/oidc/callback", get(oidc_callback))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_rate_limit_middleware,
        ))
        .fallback(api_not_found)
        .layer(middleware::map_response(json_error_body))
        // Login/OIDC bodies are tiny; cap them so an unauthenticated caller
        // can't push axum's 2 MB default. #291
        .layer(DefaultBodyLimit::max(16 * 1024));

    // Machine-to-machine webhooks. These deliberately sit OUTSIDE the
    // `auth_middleware` layer (no user session); each handler authenticates via
    // its own shared secret. Kept on a dedicated nest so the auth layer never
    // wraps it.
    let webhooks = Router::new()
        .route(
            "/paperless/document-consumed",
            post(webhook_paperless_document_consumed),
        )
        .fallback(api_not_found)
        .layer(middleware::map_response(json_error_body))
        .layer(DefaultBodyLimit::max(DEFAULT_BODY_LIMIT));

    // Content-Security-Policy for a same-origin SPA + JSON API. `script-src
    // 'self'` (no inline scripts in the built index.html) neutralizes any
    // future XSS sink; `connect-src 'self'` matches the relative /api fetches;
    // `frame-ancestors 'none'` supersedes X-Frame-Options on modern browsers.
    // `style-src 'unsafe-inline'` is needed because React/Recharts set inline
    // styles. #288
    const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; base-uri 'none'; \
         object-src 'none'; frame-ancestors 'none'; script-src 'self'; \
         style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; \
         connect-src 'self'; form-action 'self'";

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .nest("/api/auth", auth_public)
        .nest("/api/webhooks", webhooks)
        .nest("/api", protected)
        .fallback_service(spa)
        // Every audit event written while serving a request inherits its
        // source IP and User-Agent. #441
        .layer(middleware::from_fn_with_state(
            state.clone(),
            audit_context_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static(
                "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
            ),
        ));
    // HSTS only over TLS (otherwise a browser would pin a no-TLS dev origin).
    let app = if cookie_secure {
        app.layer(SetResponseHeaderLayer::if_not_present(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=63072000; includeSubDomains"),
        ))
    } else {
        app
    };
    app.with_state(state)
}

async fn ensure_bootstrap_admin(pool: &DbPool, config: &AppConfig) -> Result<()> {
    if has_any_user(pool).await? {
        return Ok(());
    }
    let Some(password) = &config.bootstrap_admin_password else {
        if config.oidc_enabled {
            warn!(
                "no local users exist; first OIDC login will provision a user according to ARCHIVIST_OIDC_ADMIN_USERS"
            );
            return Ok(());
        }
        return Err(anyhow!(
            "no users exist and ARCHIVIST_ADMIN_PASSWORD is not set; refusing to start an unauthenticated admin UI"
        ));
    };
    validate_password_strength(password.expose_secret()).map_err(anyhow::Error::msg)?;
    let hash = hash_password(password.expose_secret())?;
    create_user_with_roles(
        pool,
        &config.bootstrap_admin_username,
        None,
        &hash,
        &[Role::Admin, Role::Operator, Role::Reviewer, Role::Auditor],
        None,
    )
    .await?;
    warn!(username = %config.bootstrap_admin_username, "created bootstrap admin user");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn provider_secret_names_are_canonicalized_before_persistence() {
        let settings = RuntimeSettings::default();
        let secrets = HashMap::from([(" OLLAMA ".to_owned(), "secret".to_owned())]);

        let canonical = canonicalize_provider_secrets(&settings, secrets)
            .expect("known provider name should canonicalize");

        assert_eq!(canonical.get("ollama").map(String::as_str), Some("secret"));
        assert_eq!(canonical.len(), 1);
    }

    #[test]
    fn provider_secret_names_reject_unknown_and_duplicate_targets() {
        let settings = RuntimeSettings::default();
        let unknown = canonicalize_provider_secrets(
            &settings,
            HashMap::from([("missing".to_owned(), "secret".to_owned())]),
        )
        .expect_err("unknown secret target must fail before any write");
        assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
        assert!(unknown.message.contains("missing"));

        let duplicate = canonicalize_provider_secrets(
            &settings,
            HashMap::from([
                ("ollama".to_owned(), "first".to_owned()),
                (" OLLAMA ".to_owned(), "second".to_owned()),
            ]),
        )
        .expect_err("two inputs resolving to one provider must fail atomically");
        assert_eq!(duplicate.status, StatusCode::BAD_REQUEST);
        assert!(duplicate.message.contains("ollama"));
    }

    #[test]
    fn settings_update_preflight_rejects_before_secret_mapping_changes() {
        let mut settings = RuntimeSettings::default();
        let mut duplicate = settings.ai.providers[0].clone();
        duplicate.name = format!(" {} ", duplicate.name.to_uppercase());
        duplicate.secret_id = Some(Uuid::new_v4());
        settings.ai.providers.push(duplicate);
        let original_secret_ids = settings
            .ai
            .providers
            .iter()
            .map(|provider| provider.secret_id)
            .collect::<Vec<_>>();
        let submitted_secrets = HashMap::from([("ollama".to_owned(), "new-secret".to_owned())]);
        let mut request = UpdateSettingsRequest {
            settings,
            paperless_token: Some("paperless-secret".to_owned()),
            notification_webhook_url: Some("https://hooks.example.test".to_owned()),
            provider_secrets: Some(submitted_secrets.clone()),
        };

        let error = prepare_settings_update(&mut request)
            .expect_err("duplicate provider names must stop the save preflight");

        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(request.provider_secrets.as_ref(), Some(&submitted_secrets));
        assert_eq!(
            request
                .settings
                .ai
                .providers
                .iter()
                .map(|provider| provider.secret_id)
                .collect::<Vec<_>>(),
            original_secret_ids
        );
    }

    #[test]
    fn settings_update_preflight_validates_defaults_added_by_normalization() {
        let mut settings = RuntimeSettings::default();
        let custom = AiProviderSettings {
            name: "custom".to_owned(),
            kind: AiProviderKind::OpenaiCompatible,
            base_url: "https://custom.example.test/v1".to_owned(),
            default_text_model: Some("custom-model".to_owned()),
            default_vision_model: None,
            cost_per_1m_input_tokens_usd: None,
            cost_per_1m_output_tokens_usd: None,
            secret_id: None,
            enabled: true,
            tuning: ProviderTuning::default(),
        };
        settings.ai.providers = vec![custom];
        settings.ai.default_provider = "custom".to_owned();
        settings.ai.ollama_base_url = "   ".to_owned();
        let mut request = UpdateSettingsRequest {
            settings,
            paperless_token: None,
            notification_webhook_url: None,
            provider_secrets: None,
        };

        let error = prepare_settings_update(&mut request)
            .expect_err("newly appended enabled defaults must be part of save validation");

        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("ollama"));
        assert!(error.message.contains("base URL"));
    }

    #[test]
    fn default_provider_rejects_empty_legacy_base_url() {
        let mut settings = RuntimeSettings::default();
        settings.ai.ollama_base_url = "  ".to_owned();

        let error = provider_for_default_text(&settings)
            .expect_err("corrupt legacy settings must not fall back to localhost");

        assert!(error.to_string().contains("empty base URL"));
        assert!(error.to_string().contains("ollama"));
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

    fn auth_context_for_session_listing(
        cookie_auth: bool,
        user_id: Uuid,
        roles: Vec<Role>,
        scopes: Vec<&str>,
    ) -> AuthContext {
        AuthContext {
            actor_type: if cookie_auth { "user" } else { "api_token" }.to_owned(),
            actor_id: Some(user_id.to_string()),
            user_id: Some(user_id),
            username: cookie_auth.then(|| "session-user".to_owned()),
            roles,
            scopes: scopes.into_iter().map(str::to_owned).collect(),
            session_id: cookie_auth.then(Uuid::new_v4),
            csrf_secret_hash: cookie_auth.then(|| "csrf-hash".to_owned()),
            cookie_auth,
        }
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
    fn ollama_cloud_detection_matches_hosted_endpoint() {
        assert!(is_ollama_cloud("https://ollama.com"));
        assert!(is_ollama_cloud("https://OLLAMA.com/"));
        assert!(!is_ollama_cloud("http://ollama:11434"));
        assert!(!is_ollama_cloud("http://localhost:11434"));
    }

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
    }

    #[test]
    fn stat_bare_date_to_covers_the_whole_day() {
        // #301: a bare `to` date is the EXCLUSIVE end of that day, a bare
        // `from` its first instant. RFC3339 inputs keep their own time.
        assert_eq!(
            parse_stat_datetime("2026-06-11", StatBound::Start),
            Some(utc(2026, 6, 11, 0, 0, 0))
        );
        assert_eq!(
            parse_stat_datetime("2026-06-11", StatBound::End),
            Some(utc(2026, 6, 12, 0, 0, 0))
        );
        assert_eq!(
            parse_stat_datetime("2026-06-11T08:30:00Z", StatBound::End),
            Some(utc(2026, 6, 11, 8, 30, 0))
        );
        assert_eq!(parse_stat_datetime("not-a-date", StatBound::End), None);
    }

    #[test]
    fn inventory_date_filters_parse_or_reject() {
        // #315: absent/blank means "no filter"; present-but-garbage is a 400
        // (same contract as the statistics range, #312) instead of silently
        // matching nothing against the typed date column.
        assert_eq!(
            parse_inventory_date_filter("date_from", None).unwrap(),
            None
        );
        assert_eq!(
            parse_inventory_date_filter("date_from", Some("  ")).unwrap(),
            None
        );
        assert_eq!(
            parse_inventory_date_filter("date_from", Some("2026-06-11")).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 11)
        );
        assert!(parse_inventory_date_filter("date_to", Some("11.06.2026")).is_err());
        assert!(parse_inventory_date_filter("date_to", Some("2026-13-40")).is_err());
    }

    #[test]
    fn statistics_default_view_includes_today() {
        // Default view (no from/to): `to` is "now", so data recorded earlier
        // today is inside the half-open [from, to) window. #301
        let now = utc(2026, 6, 11, 15, 45, 0);
        let (from, to) = resolve_stat_range(None, None, now).expect("default range");
        assert_eq!(to, now);
        assert_eq!(from, now - Duration::days(30));
        let earlier_today = utc(2026, 6, 11, 0, 5, 0);
        assert!(from <= earlier_today && earlier_today < to);

        // The UI used to send `to=<today>` as a bare date; that must also
        // cover the whole current day instead of cutting off at midnight.
        let (_, to) = resolve_stat_range(None, Some("2026-06-11"), now).expect("bare to");
        assert_eq!(to, utc(2026, 6, 12, 0, 0, 0));
        assert!(now < to);
    }

    #[test]
    fn statistics_single_day_range_is_valid() {
        // #301: `from == to` on a bare date means "exactly that day", not an
        // empty range rejected with 400.
        let now = utc(2026, 6, 11, 15, 45, 0);
        let (from, to) = resolve_stat_range(Some("2026-06-10"), Some("2026-06-10"), now)
            .expect("single-day range");
        assert_eq!(from, utc(2026, 6, 10, 0, 0, 0));
        assert_eq!(to, utc(2026, 6, 11, 0, 0, 0));

        // Inverted bounds are still rejected.
        assert!(resolve_stat_range(Some("2026-06-11"), Some("2026-06-10"), now).is_err());
    }

    #[test]
    fn statistics_unparseable_bounds_are_rejected() {
        // #312: defaults only apply to ABSENT bounds; garbage is a 400, not a
        // silent fallback to the default range.
        let now = utc(2026, 6, 11, 15, 45, 0);
        assert!(resolve_stat_range(Some("not-a-date"), None, now).is_err());
        assert!(resolve_stat_range(None, Some("2026-13-77"), now).is_err());

        // Blank values count as absent, not as garbage.
        let (from, to) = resolve_stat_range(Some(" "), Some(""), now).expect("blank = defaults");
        assert_eq!(to, now);
        assert_eq!(from, now - Duration::days(30));
    }

    #[test]
    fn statistics_bucket_floor_mirrors_date_trunc() {
        let ts = utc(2026, 6, 11, 15, 45, 7); // a Thursday
        assert_eq!(
            statistics_bucket_floor(ts, "hour"),
            utc(2026, 6, 11, 15, 0, 0)
        );
        assert_eq!(
            statistics_bucket_floor(ts, "day"),
            utc(2026, 6, 11, 0, 0, 0)
        );
        // date_trunc('week') floors to the ISO Monday.
        assert_eq!(
            statistics_bucket_floor(ts, "week"),
            utc(2026, 6, 8, 0, 0, 0)
        );
        assert_eq!(
            statistics_bucket_floor(ts, "month"),
            utc(2026, 6, 1, 0, 0, 0)
        );
    }

    #[test]
    fn statistics_bucket_next_steps_each_granularity() {
        let monday = utc(2026, 6, 8, 0, 0, 0);
        assert_eq!(
            statistics_bucket_next(monday, "hour"),
            Some(utc(2026, 6, 8, 1, 0, 0))
        );
        assert_eq!(
            statistics_bucket_next(monday, "day"),
            Some(utc(2026, 6, 9, 0, 0, 0))
        );
        assert_eq!(
            statistics_bucket_next(monday, "week"),
            Some(utc(2026, 6, 15, 0, 0, 0))
        );
        assert_eq!(
            statistics_bucket_next(utc(2026, 6, 1, 0, 0, 0), "month"),
            Some(utc(2026, 7, 1, 0, 0, 0))
        );
        // Month rollover across the year boundary.
        assert_eq!(
            statistics_bucket_next(utc(2026, 12, 1, 0, 0, 0), "month"),
            Some(utc(2027, 1, 1, 0, 0, 0))
        );
    }

    #[test]
    fn statistics_zero_fill_enumerates_the_requested_range() {
        // #312: every bucket of [from, to) appears, including empty interior /
        // trailing ones, mirroring dashboard_bucket_labels. With no data at
        // all the requested range itself is enumerated (flat zero axis).
        let from = utc(2026, 6, 9, 12, 0, 0);
        let to = utc(2026, 6, 11, 15, 45, 0);
        assert_eq!(
            statistics_bucket_starts(from, to, "day", None),
            vec![
                utc(2026, 6, 9, 0, 0, 0),
                utc(2026, 6, 10, 0, 0, 0),
                utc(2026, 6, 11, 0, 0, 0),
            ]
        );
        // The axis never starts before the first bucket holding data.
        assert_eq!(
            statistics_bucket_starts(from, to, "day", Some(utc(2026, 6, 10, 0, 0, 0))),
            vec![utc(2026, 6, 10, 0, 0, 0), utc(2026, 6, 11, 0, 0, 0)]
        );
    }

    #[test]
    fn statistics_zero_fill_clamps_all_time_to_earliest_data() {
        // "all time" (far-past from): the axis starts at the earliest bucket
        // that actually has data, mirroring the dashboard's "all" range...
        let from = utc(2000, 1, 1, 0, 0, 0);
        let to = utc(2026, 6, 11, 15, 0, 0);
        let earliest = Some(utc(2026, 6, 9, 0, 0, 0));
        assert_eq!(
            statistics_bucket_starts(from, to, "day", earliest),
            vec![
                utc(2026, 6, 9, 0, 0, 0),
                utc(2026, 6, 10, 0, 0, 0),
                utc(2026, 6, 11, 0, 0, 0),
            ]
        );
        // ...stays sparse without any data (the sentinel span blows the cap)...
        assert!(statistics_bucket_starts(from, to, "day", None).is_empty());
        // ...and stays sparse when even the data span exceeds the cap.
        let ancient = Some(utc(2000, 2, 7, 0, 0, 0));
        assert!(statistics_bucket_starts(from, to, "hour", ancient).is_empty());
    }

    #[test]
    fn oidc_email_is_only_used_when_verified() {
        let mut claims = OidcIdClaims {
            iss: "https://issuer.example.com".to_owned(),
            sub: "subject-1".to_owned(),
            aud: serde_json::Value::String("client".to_owned()),
            exp: 0,
            nonce: None,
            email: Some("admin@example.com".to_owned()),
            email_verified: Some(true),
            preferred_username: None,
            at_hash: None,
            additional: serde_json::Map::new(),
        };
        assert_eq!(oidc_verified_email(&claims), Some("admin@example.com"));

        claims.email_verified = Some(false);
        assert_eq!(oidc_verified_email(&claims), None);

        // Absent email_verified must be treated as unverified.
        claims.email_verified = None;
        assert_eq!(oidc_verified_email(&claims), None);
    }

    #[test]
    fn openai_model_filter_keeps_chat_drops_non_chat() {
        for keep in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-5.5",
            "chatgpt-4o-latest",
            "o3",
            "o4-mini",
        ] {
            assert!(openai_id_is_chat_capable(keep), "should keep {keep}");
        }
        for drop in [
            "text-embedding-3-large",
            "whisper-1",
            "tts-1",
            "dall-e-3",
            "gpt-image-1",
            "gpt-4o-audio-preview",
            "omni-moderation-latest",
        ] {
            assert!(!openai_id_is_chat_capable(drop), "should drop {drop}");
        }
    }

    #[tokio::test]
    async fn sglang_minimax_m3_is_confirmed_through_openai_compatible_models_endpoint() {
        use axum::Json as AxumJson;

        const MODEL: &str = "ressl/MiniMax-M3-uncensored-NVFP4";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/v1/models",
            get(|| async { AxumJson(json!({ "data": [{ "id": MODEL }] })) }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        let provider = ApiProvider {
            name: "sglang-minimax-m3".to_owned(),
            kind: AiProviderKind::OpenaiCompatible,
            base_url: format!("http://{address}/v1"),
            model: MODEL.to_owned(),
            secret_id: None,
            tuning: RuntimeSettings::default().effective_tuning(),
        };

        let models = discover_provider_models(&provider, None)
            .await
            .expect("SGLang model discovery succeeds");

        assert_eq!(
            models
                .iter()
                .map(|model| model.name.as_str())
                .collect::<Vec<_>>(),
            vec![MODEL]
        );
        server.abort();
    }

    #[test]
    fn validates_password_strength() {
        assert!(validate_password_strength("short").is_err());
        assert!(validate_password_strength("            ").is_err());
        assert!(validate_password_strength("long-enough-password").is_ok());
    }

    #[tokio::test]
    async fn validate_outbound_url_accepts_public_host() {
        // 8.8.8.8 is a public unicast address; no DNS needed.
        let ok = validate_outbound_url("https://8.8.8.8/healthz").await;
        assert!(ok.is_ok(), "expected public IP to be accepted: {ok:?}");
    }

    #[tokio::test]
    async fn validate_outbound_url_rejects_loopback() {
        let err = validate_outbound_url("http://127.0.0.1:8080/")
            .await
            .expect_err("loopback must be rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn validate_outbound_url_accepts_rfc1918() {
        // RFC1918 is the common case for K8s service IPs, Docker bridge
        // networks, and on-prem service meshes. Operator-trusted internal
        // targets must be allowed for the in-UI "Test" buttons to work.
        for url in [
            "http://10.0.0.5/api",
            "http://172.16.5.5/api",
            "http://192.168.1.10/api",
        ] {
            let ok = validate_outbound_url(url).await;
            assert!(ok.is_ok(), "{url} must be accepted: {ok:?}");
        }
    }

    #[tokio::test]
    async fn validate_outbound_url_accepts_rfc6598() {
        // RFC6598 shared-address space (100.64.0.0/10) is used by ISP CGN
        // and some homelab/router setups.
        let ok = validate_outbound_url("http://100.64.0.5/api").await;
        assert!(ok.is_ok(), "RFC6598 must be accepted: {ok:?}");
    }

    #[tokio::test]
    async fn validate_outbound_url_accepts_rfc4193() {
        // RFC4193 unique-local IPv6 (fc00::/7). K8s dual-stack clusters
        // and on-prem v6 deployments live here. The previous validator
        // would have rejected this with "private, loopback, or link-local";
        // the new policy must let it through.
        //
        // Use an explicit port so getaddrinfo treats the host as a literal
        // and doesn't actually try DNS (which fails for `fd00::1` in CI).
        let ok = validate_outbound_url("http://[fd00::1]:8080/api").await;
        assert!(ok.is_ok(), "RFC4193 must be accepted: {ok:?}");
    }

    #[tokio::test]
    async fn validate_outbound_url_rejects_non_http_scheme() {
        let err = validate_outbound_url("file:///etc/passwd")
            .await
            .expect_err("non-http scheme rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn validate_outbound_url_rejects_userinfo() {
        let err = validate_outbound_url("http://user:pass@8.8.8.8/")
            .await
            .expect_err("userinfo rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn validate_outbound_url_rejects_ipv6_loopback() {
        let err = validate_outbound_url("http://[::1]/")
            .await
            .expect_err("IPv6 loopback rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn validate_outbound_url_rejects_link_local() {
        // Cloud-metadata IMDS (AWS/Azure/GCP) is at 169.254.169.254.
        let err = validate_outbound_url("http://169.254.169.254/latest/meta-data/")
            .await
            .expect_err("link-local rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn validate_outbound_url_rejects_ipv6_link_local() {
        let err = validate_outbound_url("http://[fe80::1]/")
            .await
            .expect_err("IPv6 link-local rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn validate_outbound_url_rejects_v4_mapped_loopback() {
        // Make sure an attacker can't smuggle 127.0.0.1 past the v4 check
        // by encoding it as ::ffff:127.0.0.1.
        let err = validate_outbound_url("http://[::ffff:127.0.0.1]/")
            .await
            .expect_err("v4-mapped loopback must be rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn validate_outbound_url_rejects_unspecified() {
        let err = validate_outbound_url("http://0.0.0.0/")
            .await
            .expect_err("0.0.0.0 must be rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn validate_outbound_url_rejects_multicast() {
        let err = validate_outbound_url("http://224.0.0.1/")
            .await
            .expect_err("multicast must be rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
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
    fn completion_tag_reconcile_audit_records_applied_documents_on_partial_failure() {
        // #410: documents already tagged in Paperless must be audited even
        // when a later document fails.
        let auth = auth_context_for_session_listing(true, Uuid::new_v4(), vec![], vec![]);
        let error = anyhow!("Paperless returned 502");
        let audit = completion_tags_reconciled_audit(&auth, false, 3, &[7, 9], Some(&error));
        assert_eq!(audit.outcome, "partial_failure");
        assert_eq!(
            audit.error_message.as_deref(),
            Some("Paperless returned 502")
        );
        let after = audit.after.expect("after");
        assert_eq!(after["applied"], 2);
        assert_eq!(after["applied_document_ids"], json!([7, 9]));
        let audit = completion_tags_reconciled_audit(&auth, false, 1, &[7], None);
        assert_eq!(audit.outcome, "success");
        assert!(audit.error_message.is_none());
    }

    #[test]
    fn completion_tag_reconcile_requires_all_stage_tags_and_missing_full_tag() {
        let stage_tags = vec!["archivist-ocr".to_owned(), "archivist-tags".to_owned()];
        let document_tags = vec!["Archivist-OCR".to_owned(), "archivist-tags".to_owned()];
        assert!(completion_tag_reconcile_needed(
            &document_tags,
            &stage_tags,
            "ai-processed",
            false,
        ));

        let already_complete = vec![
            "archivist-ocr".to_owned(),
            "archivist-tags".to_owned(),
            "AI-PROCESSED".to_owned(),
        ];
        assert!(!completion_tag_reconcile_needed(
            &already_complete,
            &stage_tags,
            "ai-processed",
            true,
        ));

        let missing_stage = vec!["archivist-ocr".to_owned()];
        assert!(!completion_tag_reconcile_needed(
            &missing_stage,
            &stage_tags,
            "ai-processed",
            false,
        ));
        assert!(completion_tag_reconcile_needed(
            &missing_stage,
            &stage_tags,
            "ai-processed",
            true,
        ));

        assert!(!completion_tag_reconcile_needed(
            &document_tags,
            &[],
            "ai-processed",
            true,
        ));
    }

    #[test]
    fn validates_chat_document_id_filters() {
        assert_eq!(
            normalize_chat_document_ids(Some(vec![2, 2, 3]))
                .expect("valid ids")
                .expect("some ids"),
            vec![2, 3]
        );
        assert!(
            normalize_chat_document_ids(Some(vec![0]))
                .expect_err("zero is rejected")
                .status
                == StatusCode::BAD_REQUEST
        );
        assert!(
            normalize_chat_document_ids(Some(vec![1; MAX_CHAT_DOCUMENT_FILTER_IDS + 1]))
                .expect_err("oversized filter is rejected")
                .status
                == StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn normalizes_oidc_usernames_for_local_accounts() {
        assert_eq!(oidc_username(" Rressl@example.com "), "rressl@example.com");
        assert_eq!(oidc_username("René Ressl"), "ren-ressl");
        assert_eq!(oidc_username("!!"), "oidc-user");
    }

    fn oidc_test_claims() -> OidcIdClaims {
        OidcIdClaims {
            iss: "https://issuer.example.com".to_owned(),
            sub: "subject-1".to_owned(),
            aud: serde_json::Value::String("client".to_owned()),
            exp: 0,
            nonce: None,
            email: None,
            email_verified: None,
            preferred_username: None,
            at_hash: None,
            additional: serde_json::Map::new(),
        }
    }

    /// Claims carrying a roles claim `value` under claim name `claim`.
    fn oidc_test_claims_with_roles(claim: &str, value: serde_json::Value) -> OidcIdClaims {
        let mut claims = oidc_test_claims();
        claims.additional.insert(claim.to_owned(), value);
        claims
    }

    #[test]
    fn oidc_admin_allowlist_gets_admin_roles() {
        let mut config = test_config();
        config.oidc_admin_users = "oidc-admin, admin@example.com".to_owned();

        let roles = oidc_roles(
            &config,
            &oidc_test_claims(),
            "subject-1",
            "oidc-admin",
            None,
        )
        .expect("roles parse")
        .roles;
        assert!(roles.contains(&Role::Admin));
        assert!(roles.contains(&Role::Auditor));

        let email_roles = oidc_roles(
            &config,
            &oidc_test_claims(),
            "subject-2",
            "someone",
            Some("admin@example.com"),
        )
        .expect("roles parse")
        .roles;
        assert!(email_roles.contains(&Role::Admin));
    }

    #[test]
    fn oidc_admin_allowlist_matches_immutable_subject() {
        let mut config = test_config();
        config.oidc_admin_users = "327680913418715137".to_owned();

        // Degraded claims: username fell back to the raw subject, no email.
        // The allowlisted subject must still grant admin (#299).
        let roles = oidc_roles(
            &config,
            &oidc_test_claims(),
            "327680913418715137",
            "327680913418715137",
            None,
        )
        .expect("roles parse")
        .roles;
        assert!(roles.contains(&Role::Admin));

        // Subjects are matched verbatim — a different subject stays default.
        let other = oidc_roles(
            &config,
            &oidc_test_claims(),
            "999999999999999999",
            "999999999999999999",
            None,
        )
        .expect("roles parse")
        .roles;
        assert!(!other.contains(&Role::Admin));
    }

    #[test]
    fn oidc_reads_zitadel_project_roles_object_and_maps_admin() {
        // The real bug: ZITADEL asserts project roles as an OBJECT keyed by
        // role name. Previously these were dropped entirely; now archivist-admin
        // maps to Admin. #299.
        let config = test_config();
        let claims = oidc_test_claims_with_roles(
            "urn:zitadel:iam:org:project:roles",
            serde_json::json!({
                "archivist-admin": {"327680000000000000": "acme.zitadel.cloud"},
                "archivist-reviewer": {"327680000000000000": "acme.zitadel.cloud"}
            }),
        );
        let resolution = oidc_roles(
            &config,
            &claims,
            "327680913418715137",
            "327680913418715137",
            None,
        )
        .expect("roles parse");
        assert!(
            resolution.roles.contains(&Role::Admin),
            "archivist-admin maps to admin"
        );
        assert!(resolution.roles.contains(&Role::Reviewer));
        assert!(
            resolution.authoritative,
            "an asserted roles claim is authoritative"
        );
        assert!(resolution.idp_claim_present);
    }

    #[test]
    fn oidc_idp_admin_role_survives_degraded_identity() {
        // The exact production scenario: ZITADEL sends archivist-admin but the
        // token has no preferred_username and no verified email. The role claim
        // must still grant admin (and be authoritative, so it is not preserved
        // away). This is what v1.12.4 missed — it never read the roles claim.
        let config = test_config();
        let claims = oidc_test_claims_with_roles(
            "urn:zitadel:iam:org:project:roles",
            serde_json::json!({"archivist-admin": {"o": "d"}}),
        );
        assert!(
            oidc_claims_degraded(&claims),
            "no username and no verified email is degraded"
        );
        let resolution = oidc_roles(
            &config,
            &claims,
            "327680913418715137",
            "327680913418715137",
            None,
        )
        .expect("roles parse");
        assert!(resolution.roles.contains(&Role::Admin));
        assert!(resolution.authoritative);
    }

    #[test]
    fn oidc_maps_project_scoped_claim_and_array_shape() {
        let config = test_config();
        // Project-scoped claim name (…:<projectid>:roles) + array-of-strings.
        let claims = oidc_test_claims_with_roles(
            "urn:zitadel:iam:org:project:289000000000000000:roles",
            serde_json::json!(["archivist-operator"]),
        );
        let resolution = oidc_roles(&config, &claims, "s", "u", None).expect("roles parse");
        assert_eq!(resolution.roles, vec![Role::Operator]);
        assert!(resolution.idp_claim_present);
    }

    #[test]
    fn oidc_ignores_unmapped_idp_roles_no_escalation() {
        let config = test_config();
        let claims = oidc_test_claims_with_roles(
            "urn:zitadel:iam:org:project:roles",
            serde_json::json!({"some-unrelated-role": {}}),
        );
        let resolution = oidc_roles(&config, &claims, "s", "u", None).expect("roles parse");
        // Claim present but nothing maps → authoritative empty, falls back to
        // the default role, and crucially does NOT grant admin.
        assert!(resolution.idp_claim_present);
        assert!(resolution.authoritative);
        assert!(!resolution.roles.contains(&Role::Admin));
    }

    #[test]
    fn oidc_no_roles_claim_is_not_authoritative() {
        let config = test_config();
        let resolution =
            oidc_roles(&config, &oidc_test_claims(), "s", "u", None).expect("roles parse");
        assert!(!resolution.idp_claim_present);
        assert!(
            !resolution.authoritative,
            "absent roles claim + no allowlist → fallback, must not demote a returning user"
        );
        assert_eq!(resolution.roles, vec![Role::Viewer]);
    }

    #[test]
    fn merge_userinfo_fills_identity_and_roles_from_userinfo() {
        // The exact production scenario: the ID token is minimal (only `sub`),
        // while ZITADEL returns the username/email/roles from userinfo. After
        // merge the token is no longer degraded and role-based admin works.
        let config = test_config();
        let mut claims = oidc_test_claims();
        assert!(
            oidc_claims_degraded(&claims),
            "bare token (no username, no verified email) starts degraded"
        );
        claims.merge_userinfo(
            serde_json::json!({
                "sub": "100000000000000001",
                "preferred_username": "rressl",
                "email": "rr@example.com",
                "email_verified": true,
                "urn:zitadel:iam:org:project:roles": {"archivist-admin": {"o": "d"}}
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        assert!(
            !oidc_claims_degraded(&claims),
            "userinfo supplied a usable username"
        );
        assert_eq!(claims.preferred_username.as_deref(), Some("rressl"));
        assert_eq!(oidc_verified_email(&claims), Some("rr@example.com"));

        let resolution = oidc_roles(
            &config,
            &claims,
            "100000000000000001",
            "rressl",
            oidc_verified_email(&claims),
        )
        .expect("roles parse");
        assert!(
            resolution.roles.contains(&Role::Admin),
            "the roles claim merged from userinfo grants admin"
        );
        assert!(resolution.authoritative);
    }

    #[test]
    fn merge_userinfo_does_not_override_signed_id_token_fields() {
        let mut claims = oidc_test_claims();
        claims.preferred_username = Some("from-id-token".to_owned());
        claims.merge_userinfo(
            serde_json::json!({"sub": "subject-1", "preferred_username": "from-userinfo"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(
            claims.preferred_username.as_deref(),
            Some("from-id-token"),
            "the signed ID token wins; userinfo only fills gaps"
        );
    }

    #[test]
    fn merge_userinfo_username_lets_the_allowlist_match() {
        // Minimal token + allowlist by username: once userinfo fills the
        // username, the allowlist matches even though the token sub is numeric.
        let mut config = test_config();
        config.oidc_admin_users = "rressl".to_owned();
        let mut claims = oidc_test_claims();
        claims.merge_userinfo(
            serde_json::json!({"sub": "100000000000000001", "preferred_username": "rressl"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let resolution = oidc_roles(&config, &claims, "100000000000000001", "rressl", None)
            .expect("roles parse");
        assert!(
            resolution.roles.contains(&Role::Admin),
            "username allowlist matches after the userinfo merge"
        );
    }

    #[test]
    fn oidc_role_mappings_parse_case_insensitively_and_skip_junk() {
        let map = parse_oidc_role_mappings(
            "Archivist-Admin=admin, archivist-reviewer=reviewer, junk, bad=notarole",
        );
        assert_eq!(map.get("archivist-admin"), Some(&Role::Admin));
        assert_eq!(map.get("archivist-reviewer"), Some(&Role::Reviewer));
        assert!(!map.contains_key("bad"), "an unknown app role is skipped");
    }

    #[test]
    fn oidc_degraded_claims_are_detected() {
        let mut claims = OidcIdClaims {
            iss: "https://issuer.example.com".to_owned(),
            sub: "subject-1".to_owned(),
            aud: serde_json::Value::String("client".to_owned()),
            exp: 0,
            nonce: None,
            email: Some("admin@example.com".to_owned()),
            email_verified: None,
            preferred_username: None,
            at_hash: None,
            additional: serde_json::Map::new(),
        };
        // Unverified email + no preferred_username → degraded.
        assert!(oidc_claims_degraded(&claims));

        claims.email_verified = Some(true);
        assert!(!oidc_claims_degraded(&claims));

        claims.email_verified = None;
        claims.preferred_username = Some("rressl".to_owned());
        assert!(!oidc_claims_degraded(&claims));

        // A whitespace-only preferred_username carries no identity.
        claims.preferred_username = Some("  ".to_owned());
        assert!(oidc_claims_degraded(&claims));
    }

    #[test]
    fn oidc_default_roles_are_deduplicated() {
        let mut config = test_config();
        config.oidc_default_roles = "viewer reviewer viewer".to_owned();
        assert_eq!(
            oidc_roles(&config, &oidc_test_claims(), "subject-1", "user", None)
                .expect("roles parse")
                .roles,
            vec![Role::Viewer, Role::Reviewer]
        );
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

    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
    async fn consistency_inventory_decodes_typed_document_dates() {
        let Ok(database_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = connect(&database_url, 10)
            .await
            .expect("connect consistency test database");
        migrate(&pool).await.expect("apply migrations");
        sqlx::query(
            "delete from document_inventory where paperless_document_id in (386001, 386002)",
        )
        .execute(&pool)
        .await
        .expect("clear consistency fixtures");
        sqlx::query(
            r#"
            insert into document_inventory (
              paperless_document_id, title, current_tag_ids, correspondent_id,
              document_type_id, document_date
            ) values
              (386001, 'Rechnung', '{3,1}', 7, 9, date '2026-09-27'),
              (386002, null, '{}', null, null, null)
            "#,
        )
        .execute(&pool)
        .await
        .expect("insert consistency fixtures");

        let inventory = load_consistency_inventory(&pool)
            .await
            .expect("typed document_date must decode (#386)");
        sqlx::query(
            "delete from document_inventory where paperless_document_id in (386001, 386002)",
        )
        .execute(&pool)
        .await
        .expect("clean up consistency fixtures");

        let dated = inventory.get(&386001).expect("dated row");
        assert_eq!(
            dated.document_date,
            chrono::NaiveDate::from_ymd_opt(2026, 9, 27)
        );
        assert_eq!(dated.current_tag_ids, vec![3, 1]);
        assert_eq!(dated.correspondent, Some(7));
        assert_eq!(
            dated.document_date,
            archivist_db::parse_paperless_document_date(Some("2026-09-27T00:00:00+02:00"))
        );
        assert_eq!(
            inventory.get(&386002).expect("undated row").document_date,
            None
        );
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
    async fn paperless_bridge_requires_origin_mapping_and_is_concurrency_safe() {
        let Ok(database_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = connect(&database_url, 10)
            .await
            .expect("connect identity test database");
        migrate(&pool).await.expect("apply identity migration");
        sqlx::query("delete from users")
            .execute(&pool)
            .await
            .expect("delete identity fixtures");
        sqlx::query("truncate audit_events restart identity")
            .execute(&pool)
            .await
            .expect("truncate audit fixtures");

        let local_id = create_user_with_roles(
            &pool,
            "paperless-alice",
            Some("local@example.com"),
            "local-hash",
            &[Role::Admin],
            None,
        )
        .await
        .expect("create prefixed local account");
        let paperless_instance = Url::parse("https://paperless-a.example/api/").unwrap();
        let alice_subject = paperless_user_subject(&paperless_instance, 101);
        let bridge = find_or_create_paperless_bridge_user(
            &pool,
            "paperless-alice",
            &alice_subject,
            "disabled-hash",
        )
        .await
        .expect("allocate a distinct bridge-owned account");
        assert_ne!(bridge.id, local_id);
        assert_ne!(bridge.username, "paperless-alice");
        assert_eq!(
            find_user_for_login(&pool, "paperless-alice")
                .await
                .unwrap()
                .unwrap()
                .id,
            local_id,
            "generic local login still resolves the unrelated local owner"
        );
        assert!(
            find_paperless_bridge_user(&pool, &alice_subject)
                .await
                .expect("lookup bridge mapping")
                .is_some_and(|user| user.id == bridge.id),
            "only the verified Paperless subject may resolve the bridge account"
        );
        sqlx::query("update users set enabled = false where id = $1")
            .bind(bridge.id)
            .execute(&pool)
            .await
            .expect("disable bridge account");
        let after_token_rotation = find_or_create_paperless_bridge_user(
            &pool,
            "paperless-renamed-alice",
            &alice_subject,
            "different-disabled-hash",
        )
        .await
        .expect("stable Paperless user ID resolves after token rotation");
        assert_eq!(after_token_rotation.id, bridge.id);
        assert!(!after_token_rotation.enabled);
        let alice_accounts: i64 = sqlx::query_scalar(
            "select count(*)::bigint from users where external_auth_provider = 'paperless_bridge' and external_subject = $1",
        )
        .bind(&alice_subject)
        .fetch_one(&pool)
        .await
        .expect("count stable bridge mapping");
        assert_eq!(alice_accounts, 1);

        sqlx::query("delete from users")
            .execute(&pool)
            .await
            .expect("reset identity fixtures");
        sqlx::query("truncate audit_events restart identity")
            .execute(&pool)
            .await
            .expect("reset audit fixtures");
        let pool_a = connect(&database_url, 2).await.expect("connect writer A");
        let pool_b = connect(&database_url, 2).await.expect("connect writer B");
        let concurrent_subject = paperless_user_subject(&paperless_instance, 202);
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let writer_a = {
            let barrier = Arc::clone(&barrier);
            let concurrent_subject = concurrent_subject.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                find_or_create_paperless_bridge_user(
                    &pool_a,
                    "paperless-concurrent",
                    &concurrent_subject,
                    "disabled-hash-a",
                )
                .await
            })
        };
        let writer_b = {
            let barrier = Arc::clone(&barrier);
            let concurrent_subject = concurrent_subject.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                find_or_create_paperless_bridge_user(
                    &pool_b,
                    " PAPERLESS-CONCURRENT ",
                    &concurrent_subject,
                    "disabled-hash-b",
                )
                .await
            })
        };
        barrier.wait().await;

        let user_a = writer_a.await.unwrap().expect("writer A resolves user");
        let user_b = writer_b.await.unwrap().expect("writer B resolves user");
        assert_eq!(user_a.id, user_b.id);
        let users: i64 = sqlx::query_scalar("select count(*)::bigint from users")
            .fetch_one(&pool)
            .await
            .expect("count bridge users");
        assert_eq!(users, 1);
        let mapping =
            sqlx::query("select external_auth_provider, external_subject from users where id = $1")
                .bind(user_a.id)
                .fetch_one(&pool)
                .await
                .expect("read bridge origin mapping");
        assert_eq!(
            mapping
                .try_get::<Option<String>, _>("external_auth_provider")
                .unwrap()
                .as_deref(),
            Some("paperless_bridge")
        );
        assert_eq!(
            mapping
                .try_get::<Option<String>, _>("external_subject")
                .unwrap()
                .as_deref(),
            Some(concurrent_subject.as_str())
        );

        let plus_subject = paperless_user_subject(&paperless_instance, 303);
        let first = find_or_create_paperless_bridge_user(
            &pool,
            "paperless-alice-ops",
            &plus_subject,
            "disabled-hash-c",
        )
        .await
        .expect("create first lossy-name bridge account");
        let dash_subject = paperless_user_subject(&paperless_instance, 304);
        let second = find_or_create_paperless_bridge_user(
            &pool,
            "paperless-alice-ops",
            &dash_subject,
            "disabled-hash-d",
        )
        .await
        .expect("create second lossy-name bridge account");
        assert_ne!(first.id, second.id);
        assert_ne!(first.username, second.username);
        assert_eq!(
            find_paperless_bridge_user(&pool, &plus_subject)
                .await
                .unwrap()
                .unwrap()
                .id,
            first.id
        );
        assert_eq!(
            find_paperless_bridge_user(&pool, &dash_subject)
                .await
                .unwrap()
                .unwrap()
                .id,
            second.id
        );
    }

    #[test]
    fn csv_export_escapes_special_characters() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
    }

    fn test_config() -> AppConfig {
        AppConfig {
            http_addr: "127.0.0.1:0".to_owned(),
            database_url: SecretString::from("postgres://localhost/archivist".to_owned()),
            worker_concurrency: 1,
            db_max_connections: 10,
            log_level: "info".to_owned(),
            cookie_secure: false,
            session_ttl_hours: 12,
            bootstrap_admin_username: "admin".to_owned(),
            bootstrap_admin_password: None,
            oidc_enabled: true,
            oidc_issuer_url: Some("https://issuer.example.com".to_owned()),
            oidc_client_id: Some("paperless-archivist".to_owned()),
            oidc_client_secret: Some(SecretString::from("test-secret".to_owned())),
            oidc_redirect_uri: Some(
                "https://archivist.example.com/api/auth/oidc/callback".to_owned(),
            ),
            oidc_scopes: "openid profile email".to_owned(),
            oidc_admin_users: String::new(),
            oidc_default_roles: "viewer".to_owned(),
            oidc_roles_claim: "urn:zitadel:iam:org:project:roles".to_owned(),
            oidc_role_mappings: "archivist-admin=admin,archivist-operator=operator,archivist-reviewer=reviewer,archivist-auditor=auditor,archivist-viewer=viewer".to_owned(),
            oidc_allow_email_link: false,
            secret_key: SecretString::from("a 32 byte local secret for tests".to_owned()),
            static_dir: "frontend/dist".to_owned(),
            trust_proxy: false,
            auth_rate_limit: 10,
            auth_rate_limit_window_seconds: 60,
            webhook_secret: None,
            metrics_token: None,
        }
    }

    // ----- /api/ai/runtime-hints --------------------------------------
    //
    // The non-Ollama branch is a pure function — assert its shape
    // directly. The Ollama branch goes through a real OllamaClient, so
    // we spin up a one-shot axum mini-server bound to 127.0.0.1:0 to
    // mock `/api/version` and `/api/ps`.

    fn make_api_provider(kind: AiProviderKind) -> ApiProvider {
        ApiProvider {
            name: format!("{kind:?}").to_ascii_lowercase(),
            kind,
            base_url: "http://example.invalid".to_owned(),
            model: "test-model".to_owned(),
            secret_id: None,
            tuning: RuntimeSettings::default().effective_tuning(),
        }
    }

    fn api_provider_profile_settings(first_url: &str, second_url: &str) -> RuntimeSettings {
        let mut settings = RuntimeSettings::default();
        settings.ai.default_provider = "first".to_owned();
        settings.ai.default_text_model = "gpt-5-first".to_owned();
        settings.ai.providers = vec![
            AiProviderSettings {
                name: "first".to_owned(),
                kind: AiProviderKind::OpenaiCompatible,
                base_url: first_url.to_owned(),
                default_text_model: Some("gpt-5-first".to_owned()),
                default_vision_model: None,
                cost_per_1m_input_tokens_usd: None,
                cost_per_1m_output_tokens_usd: None,
                secret_id: None,
                enabled: true,
                tuning: ProviderTuning {
                    text_num_ctx: Some(11_111),
                    reasoning_effort: Some(archivist_core::ReasoningEffort::Low),
                    max_output_tokens: Some(111),
                    structured_output: Some(archivist_core::StructuredOutputMode::Off),
                    request_timeout_seconds: Some(11),
                    ..ProviderTuning::default()
                },
            },
            AiProviderSettings {
                name: "second".to_owned(),
                kind: AiProviderKind::OpenaiCompatible,
                base_url: second_url.to_owned(),
                default_text_model: Some("gpt-5-second".to_owned()),
                default_vision_model: None,
                cost_per_1m_input_tokens_usd: None,
                cost_per_1m_output_tokens_usd: None,
                secret_id: None,
                enabled: true,
                tuning: ProviderTuning {
                    text_num_ctx: Some(22_222),
                    reasoning_effort: Some(archivist_core::ReasoningEffort::High),
                    max_output_tokens: Some(222),
                    structured_output: Some(archivist_core::StructuredOutputMode::JsonObject),
                    request_timeout_seconds: Some(22),
                    ..ProviderTuning::default()
                },
            },
        ];
        settings
    }

    fn api_test_chat_request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_owned(),
            system_prompt: "system".to_owned(),
            user_prompt: "user".to_owned(),
            temperature: 0.1,
            num_ctx: None,
            response_schema: Some(json!({ "type": "object" })),
            reasoning_effort: None,
            max_output_tokens: None,
            structured_output: None,
        }
    }

    fn metadata_prompt_test_settings() -> RuntimeSettings {
        let mut settings = RuntimeSettings::default();
        settings.workflow.enabled_stages = vec![Stage::Metadata];
        settings.tagging.tag_output_language = "de".to_owned();
        settings.fields.max_fields = 1;
        settings.fields.mappings = vec![archivist_core::CustomFieldMapping {
            field_name: "HiddenField".to_owned(),
            enabled: false,
            aliases: Vec::new(),
            instructions: None,
        }];
        settings.ai.providers[0].tuning.allowed_list_max = Some(2);
        settings.ai.providers[0].tuning.max_tags = Some(3);
        settings
    }

    fn metadata_prompt_test_catalog() -> MetadataPromptTestCatalog {
        MetadataPromptTestCatalog {
            correspondents: vec![
                "Acme AG".to_owned(),
                "Beta GmbH".to_owned(),
                "Gamma AG".to_owned(),
            ],
            document_types: vec![
                "Invoice".to_owned(),
                "Letter".to_owned(),
                "Receipt".to_owned(),
            ],
            tags: vec!["Finance".to_owned(), "Tax".to_owned(), "Urgent".to_owned()],
            fields: vec![
                ("InvoiceNumber".to_owned(), Some("string".to_owned())),
                ("HiddenField".to_owned(), Some("integer".to_owned())),
            ],
        }
    }

    #[test]
    fn metadata_prompt_test_request_matches_worker_prompt_schema_and_runtime_catalog() {
        let settings = metadata_prompt_test_settings();
        let tuning = settings.effective_tuning();
        let request = build_metadata_prompt_test_chat_request(
            &settings,
            &tuning,
            "Rechnung für Beratung und Entwicklung von Acme AG. Der Betrag ist mit Datum fällig. Invoice Tax Urgent Rechnungsnummer 41",
            metadata_prompt_test_catalog(),
        )
        .expect("metadata prompt request");

        assert!(
            request
                .user_prompt
                .contains("Detected document language: de")
        );
        assert!(
            request
                .user_prompt
                .contains("Desired language for newly generated business tags: de")
        );
        assert!(request.user_prompt.contains("Acme AG"));
        assert!(!request.user_prompt.contains("Beta GmbH"));
        assert!(!request.user_prompt.contains("Gamma AG"));
        assert!(request.user_prompt.contains("Invoice"));
        assert!(!request.user_prompt.contains("Receipt"));
        assert!(request.user_prompt.contains("Tax"));
        assert!(request.user_prompt.contains("Urgent"));
        assert!(!request.user_prompt.contains("Finance"));
        assert!(request.user_prompt.contains("\"InvoiceNumber\" (text)"));
        assert!(!request.user_prompt.contains("HiddenField"));
        assert!(request.user_prompt.contains("at most 3 tags"));
        assert!(request.user_prompt.contains("at most 1 entries"));

        let schema = request.response_schema.expect("metadata response schema");
        assert_eq!(
            schema["properties"]["correspondent"]["properties"]["name"]["enum"],
            json!(["Acme AG"])
        );
        assert_eq!(
            schema["properties"]["tags"]["properties"]["tags"]["items"]["enum"],
            json!(["Tax", "Urgent"])
        );
        assert_eq!(
            schema["properties"]["fields"]["properties"]["fields"]["items"]["properties"]["name"]["enum"],
            json!(["InvoiceNumber"])
        );
        assert_eq!(
            schema["properties"]["tags"]["properties"]["tags"]["maxItems"],
            3
        );
        assert_eq!(
            schema["properties"]["fields"]["properties"]["fields"]["maxItems"],
            1
        );
    }

    #[test]
    fn metadata_prompt_test_editor_content_replaces_only_system_prompt() {
        let settings = metadata_prompt_test_settings();
        let mut request = build_metadata_prompt_test_chat_request(
            &settings,
            &settings.effective_tuning(),
            "Acme AG Invoice",
            metadata_prompt_test_catalog(),
        )
        .unwrap();
        let original_user = request.user_prompt.clone();
        let original_schema = request.response_schema.clone();

        apply_prompt_test_system_prompt(&mut request, "  operator system prompt  ");

        assert_eq!(request.system_prompt, "operator system prompt");
        assert_eq!(request.user_prompt, original_user);
        assert_eq!(request.response_schema, original_schema);
    }

    #[test]
    fn metadata_prompt_test_parser_returns_typed_valid_and_partial_results() {
        let valid = parse_prompt_test_output(
            Stage::Metadata,
            r#"{"title":{"title":"Invoice 41","confidence":0.98},"document_date":{"date":"2026-07-17","confidence":0.9,"warnings":["date inferred"]}}"#,
        );
        assert!(valid.validation_errors.is_empty());
        assert_eq!(valid.parsed["suggestion"]["title"]["title"], "Invoice 41");
        assert_eq!(valid.parsed["diagnostics"]["status"], "valid");
        assert_eq!(valid.warnings, vec!["date inferred"]);

        let partial = parse_prompt_test_output(
            Stage::Metadata,
            r#"{"title":{"title":"Retained","confidence":0.8},"tags":"wrong","extra":"redacted"}"#,
        );
        assert_eq!(partial.parsed["suggestion"]["title"]["title"], "Retained");
        assert!(partial.parsed["suggestion"].get("tags").is_none());
        assert_eq!(
            partial.parsed["diagnostics"]["status"],
            "contract_violation"
        );
        assert_eq!(
            partial.validation_errors,
            vec![
                "metadata field(s) have wrong types or unknown nested properties: tags",
                "metadata response contains 1 unknown field(s)",
            ]
        );
    }

    #[test]
    fn metadata_prompt_test_parser_rejects_malformed_non_object_and_omitted_outputs() {
        let malformed = parse_prompt_test_output(Stage::Metadata, "not json");
        assert_eq!(
            malformed.validation_errors,
            vec!["metadata response envelope is not valid JSON"]
        );
        assert_eq!(malformed.parsed["diagnostics"]["envelope_error"], "no_json");

        let non_object = parse_prompt_test_output(Stage::Metadata, "[1, 2]");
        assert_eq!(
            non_object.validation_errors,
            vec!["metadata response must be a JSON object"]
        );
        assert_eq!(
            non_object.parsed["diagnostics"]["envelope_error"],
            "non_object"
        );

        let omitted = parse_prompt_test_output(Stage::Metadata, "{}");
        assert!(omitted.validation_errors.is_empty());
        assert_eq!(
            omitted.warnings,
            vec!["metadata response omitted every requested field"]
        );
        assert_eq!(omitted.parsed["diagnostics"]["status"], "omitted");
    }

    #[test]
    fn api_provider_tuning_follows_selected_provider_without_cross_profile_leakage() {
        let settings = api_provider_profile_settings(
            "https://first.example.test/v1",
            "https://second.example.test/v1",
        );
        let first = provider_by_name(&settings, "first").unwrap();
        let mut second = provider_by_name(&settings, "second").unwrap();
        second.model = "gpt-5-second-override".to_owned();

        let mut first_request = api_test_chat_request(&first.model);
        apply_api_provider_tuning(&first, &mut first_request);
        let mut second_request = api_test_chat_request(&second.model);
        apply_api_provider_tuning(&second, &mut second_request);

        assert_eq!(first_request.model, "gpt-5-first");
        assert_eq!(first_request.num_ctx, Some(11_111));
        assert_eq!(
            first_request.reasoning_effort,
            Some(archivist_core::ReasoningEffort::Low)
        );
        assert_eq!(first_request.max_output_tokens, Some(111));
        assert_eq!(
            first_request.structured_output,
            Some(archivist_core::StructuredOutputMode::Off)
        );
        assert_eq!(first.tuning.request_timeout_seconds, 11);

        assert_eq!(second_request.model, "gpt-5-second-override");
        assert_eq!(second_request.num_ctx, Some(22_222));
        assert_eq!(
            second_request.reasoning_effort,
            Some(archivist_core::ReasoningEffort::High)
        );
        assert_eq!(second_request.max_output_tokens, Some(222));
        assert_eq!(
            second_request.structured_output,
            Some(archivist_core::StructuredOutputMode::JsonObject)
        );
        assert_eq!(second.tuning.request_timeout_seconds, 22);
    }

    #[test]
    fn prompt_tester_defaults_to_metadata_stage_provider_but_keeps_explicit_overrides() {
        let mut settings = api_provider_profile_settings(
            "https://first.example.test/v1",
            "https://second.example.test/v1",
        );
        settings.ai.stage_models = vec![archivist_core::StageModelOverride {
            stage: Stage::Metadata,
            provider: "second".to_owned(),
            model: "ressl/MiniMax-M3-uncensored-NVFP4".to_owned(),
        }];
        let mut request = TestPromptRequest {
            stage: Stage::Metadata,
            content: "metadata system".to_owned(),
            sample_text: Some("sample".to_owned()),
            paperless_document_id: None,
            provider_name: None,
            model: None,
        };

        let stage_provider = prompt_test_provider(&settings, &request).unwrap();
        assert_eq!(stage_provider.name, "second");
        assert_eq!(stage_provider.model, "ressl/MiniMax-M3-uncensored-NVFP4");
        assert_eq!(stage_provider.tuning.text_num_ctx, Some(22_222));
        assert_eq!(stage_provider.tuning.max_output_tokens, Some(222));

        request.provider_name = Some("first".to_owned());
        request.model = Some("explicit-model".to_owned());
        let explicit_provider = prompt_test_provider(&settings, &request).unwrap();
        assert_eq!(explicit_provider.name, "first");
        assert_eq!(explicit_provider.model, "explicit-model");
        assert_eq!(explicit_provider.tuning.text_num_ctx, Some(11_111));
        assert_eq!(explicit_provider.tuning.max_output_tokens, Some(111));

        settings.ai.providers[1].kind = AiProviderKind::Mineru;
        settings
            .ai
            .stage_models
            .push(archivist_core::StageModelOverride {
                stage: Stage::Ocr,
                provider: "second".to_owned(),
                model: "mineru".to_owned(),
            });
        request.stage = Stage::Ocr;
        request.provider_name = None;
        request.model = None;
        let ocr_text_provider = prompt_test_provider(&settings, &request).unwrap();
        assert_eq!(ocr_text_provider.name, "first");
        assert_eq!(ocr_text_provider.kind, AiProviderKind::OpenaiCompatible);
        assert_eq!(ocr_text_provider.model, "gpt-5-first");
    }

    #[test]
    fn document_chat_request_uses_default_text_provider_tuning() {
        let settings = api_provider_profile_settings(
            "https://first.example.test/v1",
            "https://second.example.test/v1",
        );
        let provider = provider_for_default_text(&settings).unwrap();
        let request = build_document_chat_request(
            &provider,
            "chat system".to_owned(),
            "chat user".to_owned(),
        );

        assert_eq!(request.model, "gpt-5-first");
        assert_eq!(request.temperature, 0.1);
        assert_eq!(request.num_ctx, Some(11_111));
        assert_eq!(
            request.reasoning_effort,
            Some(archivist_core::ReasoningEffort::Low)
        );
        assert_eq!(request.max_output_tokens, Some(111));
        assert_eq!(
            request.structured_output,
            Some(archivist_core::StructuredOutputMode::Off)
        );
    }

    #[test]
    fn runtime_hints_non_ollama_returns_stub_with_provider_specific_hint() {
        for (kind, expected_fragment) in [
            (AiProviderKind::Openai, "openai-specific"),
            (AiProviderKind::Anthropic, "anthropic-specific"),
            (
                AiProviderKind::OpenaiCompatible,
                "openai_compatible-specific",
            ),
        ] {
            let provider = make_api_provider(kind.clone());
            let response = non_ollama_runtime_hints(&provider);
            assert_eq!(response.provider, provider.name);
            assert!(response.reachable);
            assert!(response.version.is_none());
            assert!(response.loaded_models.is_empty());
            assert!(response.num_parallel.is_none());
            assert!(response.max_loaded_models.is_none());
            assert!(response.keep_alive.is_none());
            let hint = response
                .hint
                .as_deref()
                .expect("non-ollama hint must be populated");
            assert!(
                hint.contains(expected_fragment),
                "hint for {kind:?} must mention '{expected_fragment}', got {hint:?}"
            );
        }
    }

    async fn spawn_mock_ollama(
        version_response: Option<Value>,
        ps_response: Option<Value>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::Json as AxumJson;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let version_handler = {
            let version_response = version_response.clone();
            move || async move {
                match version_response {
                    Some(body) => AxumJson(body).into_response(),
                    None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                }
            }
        };
        let ps_handler = {
            let ps_response = ps_response.clone();
            move || async move {
                match ps_response {
                    Some(body) => AxumJson(body).into_response(),
                    None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                }
            }
        };
        let router = Router::new()
            .route("/api/version", get(version_handler))
            .route("/api/ps", get(ps_handler));
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn runtime_hints_ollama_happy_path_collects_version_and_loaded_models() {
        let (base_url, handle) = spawn_mock_ollama(
            Some(serde_json::json!({ "version": "0.5.7" })),
            Some(serde_json::json!({
                "models": [
                    {
                        "name": "qwen3-paperless:8b",
                        "size_vram": 6_396_411_904u64,
                        "expires_at": "2026-05-17T12:00:00Z"
                    }
                ]
            })),
        )
        .await;
        let client = OllamaClient::new_with_timeout(
            "ollama",
            &base_url,
            None,
            std::time::Duration::from_secs(2),
        )
        .expect("client builds");
        let response = fetch_ollama_runtime_hints_with_client("ollama", &client).await;
        assert!(response.reachable, "Ollama mock should be reachable");
        assert_eq!(response.provider, "ollama");
        assert_eq!(response.version.as_deref(), Some("0.5.7"));
        assert_eq!(response.loaded_models.len(), 1);
        let model = &response.loaded_models[0];
        assert_eq!(model.name, "qwen3-paperless:8b");
        assert_eq!(model.size_vram_bytes, Some(6_396_411_904));
        assert!(response.num_parallel.is_none());
        assert!(response.max_loaded_models.is_none());
        assert!(response.keep_alive.is_none());
        let hint = response.hint.as_deref().expect("ollama hint string");
        assert!(
            hint.contains("NUM_PARALLEL"),
            "happy-path hint must explain the env-only knobs, got {hint:?}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn runtime_hints_ollama_unreachable_falls_back_with_error_hint() {
        // Point the client at a port nothing listens on — the version
        // probe must fail fast and surface `reachable: false`.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener); // close the socket so the next connect refuses

        let client = OllamaClient::new_with_timeout(
            "ollama",
            &format!("http://{dead_addr}"),
            None,
            std::time::Duration::from_millis(500),
        )
        .expect("client builds");
        let response = fetch_ollama_runtime_hints_with_client("ollama", &client).await;
        assert!(!response.reachable);
        assert!(response.version.is_none());
        assert!(response.loaded_models.is_empty());
        let hint = response.hint.as_deref().expect("hint populated");
        assert!(
            hint.contains("Ollama unreachable"),
            "unreachable hint should explain the failure, got {hint:?}"
        );
    }

    #[tokio::test]
    async fn ollama_chat_stamps_configured_provider_name_not_kind() {
        // Regression: the OllamaClient used to hardcode provider = "ollama", so
        // two ollama-kind providers (local "ollama" vs "ollama-cloud") collapsed
        // into one label in usage metrics. It must now stamp the configured name.
        use axum::Json as AxumJson;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/api/chat",
            post(|| async { AxumJson(serde_json::json!({ "message": { "content": "ok" } })) }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });

        let client = OllamaClient::new("ollama-cloud", &format!("http://{addr}"), None)
            .expect("client builds");
        let response = client
            .chat(ChatRequest {
                model: "glm-5.1".to_owned(),
                system_prompt: "s".to_owned(),
                user_prompt: "u".to_owned(),
                temperature: 0.0,
                num_ctx: None,
                response_schema: None,
                reasoning_effort: None,
                max_output_tokens: None,
                structured_output: None,
            })
            .await
            .expect("chat succeeds");

        assert_eq!(
            response.provider, "ollama-cloud",
            "metric must carry the configured provider name, not the hardcoded kind"
        );
        assert_eq!(response.model, "glm-5.1");
        handle.abort();
    }

    #[derive(Clone, Default)]
    struct ProviderProbeCapture {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        authorization: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        body: std::sync::Arc<std::sync::Mutex<Option<Value>>>,
    }

    async fn spawn_mock_openai_probe(
        capture: ProviderProbeCapture,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::Json as AxumJson;
        use axum::extract::State as AxumState;
        use axum::http::HeaderMap;
        use std::sync::atomic::Ordering;

        async fn probe(
            AxumState(capture): AxumState<ProviderProbeCapture>,
            headers: HeaderMap,
            AxumJson(body): AxumJson<Value>,
        ) -> AxumJson<Value> {
            capture.calls.fetch_add(1, Ordering::SeqCst);
            *capture.authorization.lock().unwrap() = headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            *capture.body.lock().unwrap() = Some(body);
            AxumJson(json!({
                "id": "draft-probe",
                "model": "gpt-5-draft",
                "choices": [{ "message": { "content": "{\"status\":\"ok\"}" } }]
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/chat/completions", post(probe))
            .with_state(capture);
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        (format!("http://{address}"), handle)
    }

    fn api_text_test_state() -> AppState {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://archivist:archivist@127.0.0.1/archivist")
            .expect("lazy test pool");
        AppState {
            pool,
            config: Arc::new(test_config()),
            auth_rate_limiter: Arc::new(AuthRateLimiter::new(10, 60)),
        }
    }

    #[tokio::test]
    async fn prompt_tester_and_document_chat_send_selected_provider_tuning_on_wire() {
        let prompt_capture = ProviderProbeCapture::default();
        let document_capture = ProviderProbeCapture::default();
        let (prompt_url, prompt_handle) = spawn_mock_openai_probe(prompt_capture.clone()).await;
        let (document_url, document_handle) =
            spawn_mock_openai_probe(document_capture.clone()).await;
        let state = api_text_test_state();

        let settings = api_provider_profile_settings(&document_url, &prompt_url);
        let mut prompt_provider = provider_by_name(&settings, "second").unwrap();
        prompt_provider.model = "gpt-5-prompt-override".to_owned();
        let prompt_input = TestPromptRequest {
            stage: Stage::Ocr,
            content: "prompt system".to_owned(),
            sample_text: Some("sample".to_owned()),
            paperless_document_id: None,
            provider_name: Some("second".to_owned()),
            model: Some(prompt_provider.model.clone()),
        };
        let mut prompt_request = build_ocr_prompt_test_chat_request("sample");
        apply_prompt_test_system_prompt(&mut prompt_request, &prompt_input.content);
        prompt_request.model = prompt_provider.model.clone();
        apply_api_provider_tuning(&prompt_provider, &mut prompt_request);
        chat_with_api_provider(&state, &prompt_provider, prompt_request)
            .await
            .expect("prompt tester wire call");

        let prompt_body = prompt_capture.body.lock().unwrap().clone().unwrap();
        assert_eq!(prompt_body["model"], "gpt-5-prompt-override");
        assert_eq!(prompt_body["reasoning_effort"], "high");
        assert_eq!(prompt_body["max_tokens"], 222);

        let document_provider = provider_for_default_text(&settings).unwrap();
        let document_request = build_document_chat_request(
            &document_provider,
            "document system".to_owned(),
            "document user".to_owned(),
        );
        chat_with_api_provider(&state, &document_provider, document_request)
            .await
            .expect("document chat wire call");

        let document_body = document_capture.body.lock().unwrap().clone().unwrap();
        assert_eq!(document_body["model"], "gpt-5-first");
        assert_eq!(document_body["reasoning_effort"], "low");
        assert_eq!(document_body["max_tokens"], 111);

        prompt_handle.abort();
        document_handle.abort();
    }

    #[derive(Default)]
    struct MixedM3Capture {
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
        bodies: Mutex<Vec<Value>>,
    }

    async fn mixed_m3_handler(
        State(capture): State<Arc<MixedM3Capture>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        use std::sync::atomic::Ordering;

        let active = capture.active.fetch_add(1, Ordering::AcqRel) + 1;
        capture.max_active.fetch_max(active, Ordering::AcqRel);
        capture.bodies.lock().unwrap().push(body.clone());
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        capture.active.fetch_sub(1, Ordering::AcqRel);
        let content = if body.get("response_format").is_some() {
            r#"{"title":{"title":"Synthetic capacity document","confidence":1.0}}"#
        } else {
            "ARCHIVIST_CAPACITY_CHAT_OK"
        };
        Json(json!({ "choices": [{ "message": { "content": content } }] }))
    }

    #[tokio::test]
    async fn worker_metadata_and_document_chat_m3_paths_share_endpoint_concurrently() {
        use std::sync::atomic::Ordering;

        let capture = Arc::new(MixedM3Capture::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/chat/completions", post(mixed_m3_handler))
            .with_state(capture.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });

        let mut settings = RuntimeSettings::default();
        settings.workflow.enabled_stages = vec![Stage::Metadata];
        settings.ai.default_provider = archivist_core::SGLANG_MINIMAX_M3_PROVIDER_NAME.to_owned();
        settings.ai.default_text_model = archivist_core::MINIMAX_M3_MODEL.to_owned();
        for provider in &mut settings.ai.providers {
            provider.enabled = false;
        }
        let m3 = settings
            .ai
            .providers
            .iter_mut()
            .find(|provider| provider.name == archivist_core::SGLANG_MINIMAX_M3_PROVIDER_NAME)
            .expect("built-in MiniMax M3 provider");
        m3.enabled = true;
        m3.base_url = format!("http://{address}");

        let state = api_text_test_state();
        let provider = provider_for_default_text(&settings).expect("M3 API provider");
        let mut metadata_request = build_metadata_prompt_test_chat_request(
            &settings,
            &provider.tuning,
            "SYNTHETIC-ONLY capacity document dated 2026-01-02.",
            MetadataPromptTestCatalog {
                correspondents: Vec::new(),
                document_types: Vec::new(),
                tags: Vec::new(),
                fields: Vec::new(),
            },
        )
        .expect("Worker-equivalent Metadata request");
        metadata_request.model = provider.model.clone();
        apply_api_provider_tuning(&provider, &mut metadata_request);
        let document_request = build_document_chat_request(
            &provider,
            "Answer only from the SYNTHETIC-ONLY document.".to_owned(),
            "Reply with exactly ARCHIVIST_CAPACITY_CHAT_OK.".to_owned(),
        );

        let (metadata_result, document_result) = tokio::join!(
            chat_with_api_provider(&state, &provider, metadata_request),
            chat_with_api_provider(&state, &provider, document_request)
        );
        assert!(metadata_result.is_ok());
        assert_eq!(
            document_result.expect("Document Chat call").text,
            "ARCHIVIST_CAPACITY_CHAT_OK"
        );
        assert_eq!(capture.max_active.load(Ordering::Acquire), 2);

        let bodies = capture.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(
            bodies
                .iter()
                .all(|body| body["model"] == archivist_core::MINIMAX_M3_MODEL)
        );
        assert!(bodies.iter().all(|body| body["max_tokens"] == 4096));
        assert!(
            bodies
                .iter()
                .all(|body| { body["chat_template_kwargs"]["thinking_mode"] == "disabled" })
        );
        assert_eq!(
            bodies
                .iter()
                .filter(|body| body.get("response_format").is_some())
                .count(),
            1
        );

        handle.abort();
    }

    #[tokio::test]
    async fn api_text_chat_honors_selected_provider_request_timeout() {
        use axum::Json as AxumJson;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/chat/completions",
            post(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                AxumJson(json!({ "choices": [{ "message": { "content": "late" } }] }))
            }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });

        let base_url = format!("http://{address}");
        let mut settings = api_provider_profile_settings(&base_url, "https://unused.example/v1");
        settings.ai.providers[0].tuning.request_timeout_seconds = Some(1);
        let provider = provider_for_default_text(&settings).unwrap();
        let request = build_document_chat_request(
            &provider,
            "document system".to_owned(),
            "document user".to_owned(),
        );
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(1500),
            chat_with_api_provider(&api_text_test_state(), &provider, request),
        )
        .await
        .expect("configured one-second timeout must return before outer guard");
        assert!(result.is_err(), "slow provider must hit configured timeout");

        handle.abort();
    }

    #[tokio::test]
    async fn provider_draft_probe_uses_draft_endpoint_tuning_and_transient_secret() {
        use archivist_core::{
            AiProviderSettings, ProviderTuning, ReasoningEffort, StructuredOutputMode,
        };
        use std::sync::atomic::Ordering;

        let saved_capture = ProviderProbeCapture::default();
        let draft_capture = ProviderProbeCapture::default();
        let (saved_url, saved_handle) = spawn_mock_openai_probe(saved_capture.clone()).await;
        let (draft_url, draft_handle) = spawn_mock_openai_probe(draft_capture.clone()).await;

        let mut saved = RuntimeSettings::default();
        saved.ai.default_provider = "draft-provider".to_owned();
        saved.ai.providers = vec![AiProviderSettings {
            name: "draft-provider".to_owned(),
            kind: AiProviderKind::OpenaiCompatible,
            base_url: saved_url,
            default_text_model: Some("saved-model".to_owned()),
            default_vision_model: None,
            cost_per_1m_input_tokens_usd: None,
            cost_per_1m_output_tokens_usd: None,
            secret_id: None,
            enabled: true,
            tuning: ProviderTuning::default(),
        }];
        let request = TestProviderRequest {
            name: "draft-provider".to_owned(),
            kind: AiProviderKind::OpenaiCompatible,
            base_url: draft_url,
            model: "gpt-5-draft".to_owned(),
            tuning: ProviderTuning {
                reasoning_effort: Some(ReasoningEffort::High),
                max_output_tokens: Some(777),
                structured_output: Some(StructuredOutputMode::JsonObject),
                request_timeout_seconds: Some(2),
                ..ProviderTuning::default()
            },
            secret_id: None,
            secret: Some("draft-super-secret".to_owned()),
        };

        let provider = provider_test_target(&saved, &request).unwrap();
        let transient_secret = SecretString::from(request.secret.clone().unwrap());
        let result = test_ai_provider(&provider, Some(transient_secret.clone())).await;
        let response = provider_test_response(&provider, result, Some(&transient_secret));

        assert_eq!(saved_capture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(draft_capture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            draft_capture.authorization.lock().unwrap().as_deref(),
            Some("Bearer draft-super-secret")
        );
        let body = draft_capture.body.lock().unwrap().clone().unwrap();
        assert_eq!(body["model"], "gpt-5-draft");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 777);
        assert_eq!(body["response_format"], json!({ "type": "json_object" }));
        assert_eq!(response["ok"], true);
        assert_eq!(response["provider"], "draft-provider");
        assert_eq!(response["model"], "gpt-5-draft");
        assert!(!response.to_string().contains("draft-super-secret"));

        let echoed_error = provider_test_response(
            &provider,
            Err(anyhow!("upstream echoed draft-super-secret")),
            Some(&transient_secret),
        );
        assert_eq!(echoed_error["ok"], false);
        assert_eq!(echoed_error["provider"], "draft-provider");
        assert_eq!(echoed_error["model"], "gpt-5-draft");
        assert!(!echoed_error.to_string().contains("draft-super-secret"));

        saved_handle.abort();
        draft_handle.abort();
    }

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

    async fn security_db_state() -> Option<AppState> {
        let database_url = std::env::var("DATABASE_URL").ok()?;
        let pool = connect(&database_url, 4)
            .await
            .expect("connect security test database");
        migrate(&pool).await.expect("apply migrations");
        sqlx::query(
            "truncate paperless_apply_intents, review_items, jobs, pipeline_runs, audit_events restart identity cascade",
        )
        .execute(&pool)
        .await
        .expect("truncate review fixtures");
        Some(AppState {
            pool,
            config: Arc::new(test_config()),
            auth_rate_limiter: Arc::new(AuthRateLimiter::new(10, 60)),
        })
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
        let stream = tokio_stream::iter(events)
            .map(|event| Ok::<_, std::convert::Infallible>(event.to_sse()));
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
        sqlx::query(
            "insert into paperless_correspondents (id, name) values (4, 'ACME'), (2, 'bank')",
        )
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

    // ----- #440: HTTP-level harness: router() + tower oneshot ---------------

    use tower::ServiceExt as _;

    const HARNESS_PEER: &str = "198.51.100.7:40000";
    const HARNESS_USER_AGENT: &str = "archivist-harness/1.0";

    /// Drives the fully composed `router()` (nesting, layers, fallbacks, auth
    /// and route-policy middleware) in-process through `oneshot`, without a
    /// TCP listener. Every request carries a fixed peer address and
    /// User-Agent so audit request context can be asserted. #440
    struct TestApi {
        app: Router,
    }

    struct TestResponse {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
    }

    #[derive(Clone)]
    enum Principal {
        Anonymous,
        Session { token: String, csrf: String },
        Token(String),
    }

    impl TestApi {
        fn new(state: AppState) -> Self {
            Self { app: router(state) }
        }

        async fn send(
            &self,
            method: Method,
            path: &str,
            principal: &Principal,
            body: Option<Value>,
        ) -> TestResponse {
            let mut builder = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header(header::USER_AGENT, HARNESS_USER_AGENT)
                .extension(ConnectInfo(
                    HARNESS_PEER.parse::<SocketAddr>().expect("peer address"),
                ));
            match principal {
                Principal::Anonymous => {}
                Principal::Session { token, csrf } => {
                    builder = builder
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                        .header("x-csrf-token", csrf);
                }
                Principal::Token(token) => {
                    builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
                }
            }
            let request = match body {
                Some(body) => builder
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string())),
                None => builder.body(Body::empty()),
            }
            .expect("build request");
            let response = self
                .app
                .clone()
                .oneshot(request)
                .await
                .expect("router is infallible");
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read body");
            let body = serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
            TestResponse {
                status,
                headers,
                body,
            }
        }
    }

    async fn harness_user(pool: &DbPool, label: &str, roles: &[Role]) -> (Uuid, Principal) {
        let suffix = Uuid::now_v7().simple().to_string();
        let user_id = create_user_with_roles(
            pool,
            &format!("harness-{label}-{suffix}"),
            None,
            "hash",
            roles,
            None,
        )
        .await
        .expect("harness user");
        let token = random_token();
        let csrf = random_token();
        create_session(
            pool,
            user_id,
            &hash_token(&token),
            &hash_token(&csrf),
            Utc::now() + Duration::hours(1),
        )
        .await
        .expect("harness session");
        (user_id, Principal::Session { token, csrf })
    }

    async fn harness_token(pool: &DbPool, creator: Uuid, scopes: &[&str]) -> Principal {
        let token = format!("pa_{}", random_token());
        archivist_db::create_api_token(
            pool,
            &format!("harness-{}", Uuid::now_v7().simple()),
            &hash_token(&token),
            &scopes
                .iter()
                .map(|scope| (*scope).to_owned())
                .collect::<Vec<_>>(),
            creator,
            None,
        )
        .await
        .expect("harness token");
        Principal::Token(token)
    }

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
    fn is_auth_or_policy_denial(
        response: &TestResponse,
        policy: &route_policy::RoutePolicy,
    ) -> bool {
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

    // ----- #447 inventory filters / views / export, #448 audit log --------

    #[test]
    fn inventory_id_filters_accept_ids_and_none_only() {
        let filter = parse_inventory_id_filter("correspondent", Some("7, none,7,12".to_owned()))
            .expect("valid filter");
        assert_eq!(filter.ids, vec![7, 12]);
        assert!(filter.include_none);
        assert!(
            parse_inventory_id_filter("correspondent", None)
                .unwrap()
                .is_empty()
        );
        for bad in ["abc", "0", "-3", "7;8"] {
            let error =
                parse_inventory_id_filter("document_type", Some(bad.to_owned())).expect_err(bad);
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let too_many = (1..=101)
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_inventory_id_filter("correspondent", Some(too_many)).is_err());
    }

    #[test]
    fn inventory_filter_queries_are_validated_and_canonicalised() {
        let (canonical, query) =
            canonical_inventory_filter_query("?tag=inbox&q=&ocr_status=failed&correspondent=none")
                .expect("valid");
        assert_eq!(canonical, "ocr_status=failed&tag=inbox&correspondent=none");
        assert_eq!(query.tags_include, vec!["inbox".to_owned()]);
        assert!(query.correspondent.include_none);
        assert_eq!(canonical_inventory_filter_query("").unwrap().0, "");
        for bad in [
            "limit=10",
            "unknown=1",
            "tag=a&tag=b",
            "date_from=27.09.2026",
            "has_error=maybe",
            "document_type=x",
        ] {
            let error = canonical_inventory_filter_query(bad).expect_err(bad);
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let long = format!("q={}", "x".repeat(INVENTORY_FILTER_QUERY_MAX_BYTES));
        assert!(canonical_inventory_filter_query(&long).is_err());

        let (format, canonical, _) =
            parse_inventory_export_query("format=json&document_type=3").expect("export query");
        assert_eq!(format, InventoryExportFormat::Json);
        assert_eq!(canonical, "document_type=3");
        assert_eq!(
            parse_inventory_export_query("").unwrap().0,
            InventoryExportFormat::Csv
        );
        assert!(parse_inventory_export_query("format=xml").is_err());
        assert!(parse_inventory_export_query("offset=5").is_err());

        let view = validate_inventory_view(&InventoryViewRequest {
            name: "  Inbox  ".to_owned(),
            query: "tag=inbox".to_owned(),
        })
        .expect("valid view");
        assert_eq!(view, ("Inbox".to_owned(), "tag=inbox".to_owned()));
        for name in ["", "   ", "tab\tname", &"n".repeat(81)] {
            assert!(
                validate_inventory_view(&InventoryViewRequest {
                    name: name.to_owned(),
                    query: String::new(),
                })
                .is_err(),
                "{name:?}"
            );
        }
    }

    #[test]
    fn inventory_csv_rows_guard_formulas_and_escape() {
        let item = DocumentInventoryItem {
            paperless_document_id: 5,
            title: Some("=HYPERLINK(\"x\")".to_owned()),
            original_file_name: Some("a,b.pdf".to_owned()),
            current_tags: vec!["inbox".to_owned(), "tax".to_owned()],
            ocr_status: "succeeded".to_owned(),
            metadata_status: "queued".to_owned(),
            current_run_status: None,
            last_run_id: None,
            last_error: None,
            next_required_stage: None,
            needs_review: false,
            complete: false,
            document_date: chrono::NaiveDate::from_ymd_opt(2026, 9, 27),
            detected_language: None,
            detected_language_confidence: None,
            detected_language_source: None,
            last_seen_at: Utc::now(),
            correspondent_id: Some(7),
            correspondent_name: Some("ACME".to_owned()),
            document_type_id: None,
            document_type_name: None,
        };
        let row = inventory_csv_row(&item);
        assert!(row.starts_with(
            "5,\"'=HYPERLINK(\"\"x\"\")\",\"a,b.pdf\",7,ACME,,,2026-09-27,inbox; tax,"
        ));
        assert!(row.ends_with('\n'));
    }

    #[test]
    fn audit_cursor_and_time_bounds_round_trip() {
        let created_at = Utc::now();
        let id = Uuid::now_v7();
        let cursor = encode_audit_cursor(created_at, id);
        let (decoded_at, decoded_id) = decode_audit_cursor(&cursor).expect("decode");
        assert_eq!(decoded_id, id);
        assert_eq!(decoded_at.timestamp_micros(), created_at.timestamp_micros());
        for bad in ["", "not base64!", "Zm9v"] {
            assert_eq!(
                decode_audit_cursor(bad).expect_err(bad).status,
                StatusCode::BAD_REQUEST
            );
        }
        let from = parse_audit_time_bound("from", Some("2026-09-27"), false)
            .unwrap()
            .unwrap();
        let to = parse_audit_time_bound("to", Some("2026-09-27"), true)
            .unwrap()
            .unwrap();
        assert_eq!(to - from, Duration::days(1));
        assert_eq!(
            parse_audit_time_bound("to", Some("2026-09-27T10:00:00+02:00"), true)
                .unwrap()
                .unwrap()
                .to_rfc3339(),
            "2026-09-27T08:00:00+00:00"
        );
        assert!(parse_audit_time_bound("from", Some("yesterday"), false).is_err());
        assert!(
            parse_audit_time_bound("from", Some("  "), false)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
    async fn inventory_filters_views_and_export_through_the_router() {
        // #447
        let Some(state) = security_db_state().await else {
            return;
        };
        let pool = state.pool.clone();
        sqlx::query(
            "delete from document_inventory where paperless_document_id between 447000 and 447999",
        )
        .execute(&pool)
        .await
        .expect("clear fixtures");
        sqlx::query(
            r#"
            insert into paperless_correspondents (id, name) values (44701, 'ACME Bank')
              on conflict (id) do update set name = excluded.name;
            "#,
        )
        .execute(&pool)
        .await
        .expect("correspondent");
        sqlx::query(
            r#"
            insert into document_inventory (paperless_document_id, title, correspondent_id)
            values (447001, '=SUM(A1)', 44701), (447002, 'Letter', 44701), (447003, 'Other', null)
            "#,
        )
        .execute(&pool)
        .await
        .expect("inventory fixtures");
        let (viewer, viewer_session) = harness_user(&pool, "inv-viewer", &[Role::Viewer]).await;
        let (_, other_session) = harness_user(&pool, "inv-other", &[Role::Viewer]).await;
        let token = harness_token(&pool, viewer, &["inventory:read"]).await;
        let api = TestApi::new(state);

        let list = api
            .send(
                Method::GET,
                "/api/inventory?correspondent=44701",
                &viewer_session,
                None,
            )
            .await;
        assert_eq!(list.status, StatusCode::OK, "{}", list.body);
        assert_eq!(list.body["total"], 2);
        assert_eq!(list.body["items"][0]["correspondent_name"], "ACME Bank");
        let rejected = api
            .send(
                Method::GET,
                "/api/inventory?correspondent=acme",
                &viewer_session,
                None,
            )
            .await;
        assert_eq!(rejected.status, StatusCode::BAD_REQUEST);

        // CSV: header + the two matching rows, formula neutralised.
        let csv = api
            .send(
                Method::GET,
                "/api/inventory/export?correspondent=44701",
                &viewer_session,
                None,
            )
            .await;
        assert_eq!(csv.status, StatusCode::OK);
        assert!(
            csv.headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/csv")
        );
        let text = csv.body.as_str().expect("csv text");
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(text.contains("447001,'=SUM(A1),"), "{text}");
        assert!(!text.contains("447003"));

        // JSON via an API token with inventory:read.
        let json_export = api
            .send(
                Method::GET,
                "/api/inventory/export?format=json&correspondent=none&q=Other",
                &token,
                None,
            )
            .await;
        assert_eq!(json_export.status, StatusCode::OK);
        let rows = json_export.body.as_array().expect("json array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["paperless_document_id"], 447003);
        let bad_format = api
            .send(
                Method::GET,
                "/api/inventory/export?format=xlsx",
                &token,
                None,
            )
            .await;
        assert_eq!(bad_format.status, StatusCode::BAD_REQUEST);

        let exported: Vec<Value> = sqlx::query_scalar(
            "select metadata from audit_events where event_type = 'inventory.exported' order by created_at",
        )
        .fetch_all(&pool)
        .await
        .expect("export audit");
        assert_eq!(
            exported,
            vec![
                json!({ "format": "csv", "filters": "correspondent=44701" }),
                json!({ "format": "json", "filters": "q=Other&correspondent=none" }),
            ]
        );

        // Saved views: canonicalised, private, unique per user.
        let created = api
            .send(
                Method::POST,
                "/api/inventory/views",
                &viewer_session,
                Some(json!({ "name": "ACME", "query": "?correspondent=44701&ocr_status=failed" })),
            )
            .await;
        assert_eq!(created.status, StatusCode::OK, "{}", created.body);
        assert_eq!(
            created.body["query"],
            "ocr_status=failed&correspondent=44701"
        );
        let view_id = created.body["id"].as_str().unwrap().to_owned();
        let duplicate = api
            .send(
                Method::POST,
                "/api/inventory/views",
                &viewer_session,
                Some(json!({ "name": "acme", "query": "" })),
            )
            .await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        let invalid = api
            .send(
                Method::POST,
                "/api/inventory/views",
                &viewer_session,
                Some(json!({ "name": "Bad", "query": "limit=5" })),
            )
            .await;
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
        let others = api
            .send(Method::GET, "/api/inventory/views", &other_session, None)
            .await;
        assert_eq!(others.body["items"], json!([]));
        let foreign = api
            .send(
                Method::DELETE,
                &format!("/api/inventory/views/{view_id}"),
                &other_session,
                None,
            )
            .await;
        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
        let renamed = api
            .send(
                Method::PUT,
                &format!("/api/inventory/views/{view_id}"),
                &viewer_session,
                Some(json!({ "name": "ACME failures", "query": "correspondent=44701" })),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
        let listed = api
            .send(Method::GET, "/api/inventory/views", &viewer_session, None)
            .await;
        assert_eq!(listed.body["items"][0]["name"], "ACME failures");
        let deleted = api
            .send(
                Method::DELETE,
                &format!("/api/inventory/views/{view_id}"),
                &viewer_session,
                None,
            )
            .await;
        assert_eq!(deleted.status, StatusCode::OK);
        let token_views = api
            .send(Method::GET, "/api/inventory/views", &token, None)
            .await;
        assert_eq!(token_views.status, StatusCode::FORBIDDEN);

        sqlx::query(
            "delete from document_inventory where paperless_document_id between 447000 and 447999",
        )
        .execute(&pool)
        .await
        .expect("clean fixtures");
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
    async fn audit_log_filters_pages_and_redacts_details_through_the_router() {
        // #448
        let Some(state) = security_db_state().await else {
            return;
        };
        let pool = state.pool.clone();
        let (auditor, auditor_session) = harness_user(&pool, "auditor", &[Role::Auditor]).await;
        let (_, viewer_session) = harness_user(&pool, "audit-viewer", &[Role::Viewer]).await;
        for index in 0..3 {
            append_audit(
                &pool,
                AuditEventInput {
                    event_type: "document.patch_confirmed".to_owned(),
                    actor_type: "user".to_owned(),
                    actor_id: Some(auditor.to_string()),
                    run_id: None,
                    job_id: None,
                    paperless_document_id: Some(448000 + index),
                    before: Some(json!({ "title": "old", "password": "hunter2" })),
                    after: Some(json!({ "title": format!("new {index}") })),
                    metadata: None,
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await
            .expect("fixture");
        }
        let api = TestApi::new(state);

        let denied = api
            .send(
                Method::GET,
                "/api/audit?document_id=448001",
                &viewer_session,
                None,
            )
            .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);

        let one = api
            .send(
                Method::GET,
                "/api/audit?document_id=448001",
                &auditor_session,
                None,
            )
            .await;
        assert_eq!(one.status, StatusCode::OK, "{}", one.body);
        assert_eq!(one.body["items"].as_array().unwrap().len(), 1);
        assert_eq!(one.body["items"][0]["has_changes"], true);
        assert_eq!(one.body["next_cursor"], Value::Null);

        // Two pages of the actor's events.
        let first = api
            .send(
                Method::GET,
                &format!("/api/audit?actor={auditor}&event_type=document.patch_confirmed&limit=2"),
                &auditor_session,
                None,
            )
            .await;
        assert_eq!(first.body["items"].as_array().unwrap().len(), 2);
        let cursor = first.body["next_cursor"]
            .as_str()
            .expect("cursor")
            .to_owned();
        let second = api
            .send(
                Method::GET,
                &format!(
                    "/api/audit?actor={auditor}&event_type=document.patch_confirmed&limit=2&cursor={cursor}"
                ),
                &auditor_session,
                None,
            )
            .await;
        let second_items = second.body["items"].as_array().unwrap();
        assert_eq!(second_items.len(), 1);
        assert_eq!(second_items[0]["paperless_document_id"], 448000);
        assert_eq!(second.body["next_cursor"], Value::Null);

        let bad_cursor = api
            .send(
                Method::GET,
                "/api/audit?cursor=nope",
                &auditor_session,
                None,
            )
            .await;
        assert_eq!(bad_cursor.status, StatusCode::BAD_REQUEST);
        let bad_from = api
            .send(Method::GET, "/api/audit?from=soon", &auditor_session, None)
            .await;
        assert_eq!(bad_from.status, StatusCode::BAD_REQUEST);

        let id = one.body["items"][0]["id"].as_str().unwrap().to_owned();
        let detail = api
            .send(
                Method::GET,
                &format!("/api/audit/{id}"),
                &auditor_session,
                None,
            )
            .await;
        assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
        assert_eq!(detail.body["before"]["title"], "old");
        assert_eq!(detail.body["before"]["password"], "[REDACTED]");
        assert_eq!(detail.body["after"]["title"], "new 1");
        let missing = api
            .send(
                Method::GET,
                &format!("/api/audit/{}", Uuid::now_v7()),
                &auditor_session,
                None,
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }
}
