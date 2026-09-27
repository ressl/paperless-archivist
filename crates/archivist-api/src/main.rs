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
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

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
