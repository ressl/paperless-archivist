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
    prefilter_allowed_list_lower, roles_have_permission, score_document_chat_source,
};
use archivist_db::{
    AmbiguousUserIdentityLinkError, AuthUser, DbPool, DocumentChatCandidate,
    InvalidUserIdentityError, LastEnabledAdminError, MetadataApplyAudit, MetadataArtifact,
    MetadataReviewItem, MetadataRunHeader, OidcUserInput, ProviderBucketEntry, ReviewItemRecord,
    UserIdentityConflictError, append_audit, apply_security_retention, connect,
    consume_oidc_login_state, count_reviews, create_document_chat_session, create_oidc_login_state,
    create_run_with_jobs_with_priority, create_runs_for_documents, create_session,
    create_user_with_roles, dashboard_bucket_labels, dashboard_range_start,
    document_chat_session_visible, failed_document_ids, find_api_token,
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
    verify_audit_integrity,
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

const SESSION_COOKIE: &str = "pa_session";
const CSRF_COOKIE: &str = "pa_csrf";
const MAX_CHAT_DOCUMENT_FILTER_IDS: usize = 50;

#[derive(Clone)]
struct AppState {
    pool: DbPool,
    config: Arc<AppConfig>,
    auth_rate_limiter: Arc<AuthRateLimiter>,
}

/// Hand-rolled per-IP token-bucket limiter used for `/api/auth/*`. We keep
/// it in-process (no external dependency) and stick with a single-instance
/// deploy assumption that matches the rest of the API.
///
/// Each IP gets `capacity` tokens that refill linearly across `window`
/// seconds. Consuming a token while empty rejects the request.
struct AuthRateLimiter {
    capacity: u32,
    window: std::time::Duration,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl AuthRateLimiter {
    fn new(capacity: u32, window_seconds: u64) -> Self {
        let window_seconds = window_seconds.max(1);
        Self {
            capacity,
            window: std::time::Duration::from_secs(window_seconds),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Returns Ok(remaining_tokens) when a request is allowed,
    /// Err(retry_after_seconds) when it is denied.
    fn check(&self, ip: IpAddr, now: Instant) -> Result<f64, u64> {
        if self.capacity == 0 {
            return Ok(0.0);
        }
        let capacity_f = f64::from(self.capacity);
        let refill_per_second = capacity_f / self.window.as_secs_f64();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        // Opportunistic cleanup: drop stale buckets so the map does not grow
        // without bound for short-lived attackers. Cap the work per call.
        if buckets.len() > 4096 {
            buckets.retain(|_, bucket| {
                now.saturating_duration_since(bucket.last_refill) < self.window * 4
            });
        }

        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: capacity_f,
            last_refill: now,
        });
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(capacity_f);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(bucket.tokens)
        } else {
            let missing = 1.0 - bucket.tokens;
            let retry_after = (missing / refill_per_second).ceil() as u64;
            Err(retry_after.max(1))
        }
    }
}

async fn auth_rate_limit_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let path = req.uri().path();
    if !path.starts_with("/api/auth/") {
        return Ok(next.run(req).await);
    }
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let trusted_proxy = state.config.trust_proxy;
    let header_ip = if trusted_proxy {
        forwarded_for_nearest_hop(req.headers())
    } else {
        None
    };
    let client_ip = header_ip.or(ip);
    let Some(client_ip) = client_ip else {
        // No address available (unit-test transport, etc.) — let it through.
        return Ok(next.run(req).await);
    };
    if let Err(retry_after) = state.auth_rate_limiter.check(client_ip, Instant::now()) {
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "too many authentication attempts" })),
        )
            .into_response();
        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return Ok(response);
    }
    Ok(next.run(req).await)
}

/// Extract the client IP from `X-Forwarded-For` using the RIGHTMOST entry.
///
/// `X-Forwarded-For` is built left-to-right: each proxy *appends* the address
/// of the peer it received the request from. The leftmost token is therefore
/// fully attacker-controlled (a client can send any value, and proxies append
/// to the right), so trusting it would allow rate-limit bypass and audit-log
/// IP spoofing. The rightmost entry is the one written by the single trusted
/// reverse proxy sitting directly in front of us, so we trust exactly that hop.
fn forwarded_for_nearest_hop(headers: &HeaderMap) -> Option<IpAddr> {
    let value = headers.get("x-forwarded-for")?.to_str().ok()?;
    let nearest = value.split(',').next_back()?.trim();
    nearest.parse::<IpAddr>().ok()
}

/// Resolve the client IP for audit/logging purposes. When `trust_proxy` is
/// enabled and `X-Forwarded-For` is present, use the nearest (rightmost) hop
/// written by the trusted proxy; otherwise fall back to the TCP peer recorded
/// by axum.
fn request_source_ip(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<String> {
    let forwarded = if state.config.trust_proxy {
        forwarded_for_nearest_hop(headers)
    } else {
        None
    };
    forwarded
        .or_else(|| peer.map(|addr| addr.ip()))
        .map(|ip| ip.to_string())
}

fn request_user_agent(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::USER_AGENT)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    // Cap the length we store so an attacker can't bloat audit rows.
    const MAX: usize = 255;
    Some(if raw.len() > MAX {
        raw.chars().take(MAX).collect()
    } else {
        raw.to_owned()
    })
}

#[derive(Debug, Clone)]
struct AuthContext {
    actor_type: String,
    actor_id: Option<String>,
    user_id: Option<Uuid>,
    username: Option<String>,
    roles: Vec<Role>,
    scopes: Vec<String>,
    session_id: Option<Uuid>,
    csrf_secret_hash: Option<String>,
    cookie_auth: bool,
}

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
        .route(
            "/inventory/{document_id}/metadata-trace",
            get(inventory_metadata_trace),
        )
        .route(
            "/chat/sessions",
            get(chat_sessions).post(create_chat_session),
        )
        .route("/chat/sessions/{id}", get(chat_messages))
        .route(
            "/chat/sessions/{id}/messages",
            post(post_chat_message).layer(DefaultBodyLimit::max(LARGE_BODY_LIMIT)),
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

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<AppState>) -> ApiResult<&'static str> {
    sqlx::query("select 1").execute(&state.pool).await?;
    Ok("ready")
}

fn authorize_metrics_request(
    expected: Option<&SecretString>,
    headers: &HeaderMap,
) -> ApiResult<()> {
    let Some(expected) = expected else {
        return Err(ApiError::service_unavailable(
            "metrics disabled: set ARCHIVIST_METRICS_TOKEN",
        ));
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let expected = expected.expose_secret();
    if expected.len() != provided.len()
        || !bool::from(expected.as_bytes().ct_eq(provided.as_bytes()))
    {
        return Err(ApiError::unauthorized("invalid metrics token"));
    }
    Ok(())
}

async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    // /metrics sits outside the session-auth middleware so Prometheus can
    // scrape it, but it discloses operational internals (queue depth, failure
    // counts, latencies) and every hit runs aggregate queries. Require the
    // dedicated scrape token; disabled (503) when unconfigured — same
    // contract as the inbound webhook.
    authorize_metrics_request(state.config.metrics_token.as_ref(), &headers)?;
    let snapshot = db_metrics_snapshot(&state.pool).await?;
    // Migrated series now live in `metrics_counters` (real monotone counters that
    // survive audit retention). Read them here and serve them as `# TYPE counter`
    // below; anything not migrated keeps its `audit_events`-derived gauge value
    // from `snapshot`. Missing keys default to 0 (the migration seeds them, so in
    // practice they are always present once `migrate()` has run at startup).
    let counters = read_metric_counters(&state.pool).await?;
    let counter = |name: &str| counters.get(name).copied().unwrap_or(0);
    let body = format!(
        concat!(
            "# HELP paperless_archivist_jobs_queued Queued jobs\n",
            "# TYPE paperless_archivist_jobs_queued gauge\n",
            "paperless_archivist_jobs_queued {}\n",
            "# HELP paperless_archivist_jobs_running Running jobs\n",
            "# TYPE paperless_archivist_jobs_running gauge\n",
            "paperless_archivist_jobs_running {}\n",
            "# HELP paperless_archivist_jobs_failed Failed jobs\n",
            "# TYPE paperless_archivist_jobs_failed gauge\n",
            "paperless_archivist_jobs_failed {}\n",
            "# HELP paperless_archivist_jobs_succeeded Succeeded jobs\n",
            "# TYPE paperless_archivist_jobs_succeeded counter\n",
            "paperless_archivist_jobs_succeeded {}\n",
            "# HELP paperless_archivist_reviews_pending Pending review items\n",
            "# TYPE paperless_archivist_reviews_pending gauge\n",
            "paperless_archivist_reviews_pending {}\n",
            "# HELP paperless_archivist_runs_active Active pipeline runs\n",
            "# TYPE paperless_archivist_runs_active gauge\n",
            "paperless_archivist_runs_active {}\n",
            // Approximate row count from planner statistics (reltuples), not a
            // live COUNT — audit_events is unbounded and this runs on every
            // scrape. Retention prunes the table, so the value can decrease:
            // it stays a gauge (a counter would make Prometheus misread
            // retention pruning as a counter reset and corrupt rate()).
            "# HELP paperless_archivist_audit_events Audit events currently retained (approximate)\n",
            "# TYPE paperless_archivist_audit_events gauge\n",
            "paperless_archivist_audit_events {}\n",
            // selector_* and job_retries_* are now real monotone counters backed
            // by `metrics_counters`, so they survive audit retention pruning and
            // are safe for rate(). See migration 0031.
            "# HELP paperless_archivist_selector_runs_total Automatic selector runs (monotone counter)\n",
            "# TYPE paperless_archivist_selector_runs_total counter\n",
            "paperless_archivist_selector_runs_total {}\n",
            "# HELP paperless_archivist_selector_documents_queued_total Documents queued by automatic selector (monotone counter)\n",
            "# TYPE paperless_archivist_selector_documents_queued_total counter\n",
            "paperless_archivist_selector_documents_queued_total {}\n",
            "# HELP paperless_archivist_job_retries_scheduled_total Job retries scheduled after transient failures (monotone counter)\n",
            "# TYPE paperless_archivist_job_retries_scheduled_total counter\n",
            "paperless_archivist_job_retries_scheduled_total {}\n",
            "# HELP paperless_archivist_job_failures_total Jobs that reached a permanent failed state (monotone counter)\n",
            "# TYPE paperless_archivist_job_failures_total counter\n",
            "paperless_archivist_job_failures_total {}\n",
            // Provider quota-exhausted events: the rate of this counter is the
            // signal the #311 quota alert targets. Incremented once per job that
            // a provider rejects with a usage-cap signal (before the cooldown is
            // recorded), so a sustained rate means a provider is capped.
            "# HELP paperless_archivist_provider_quota_total Provider quota-exhausted events (monotone counter)\n",
            "# TYPE paperless_archivist_provider_quota_total counter\n",
            "paperless_archivist_provider_quota_total {}\n",
            // model_errors_total is a live COUNT over the (non-prunable) `jobs`
            // table; it can decrease as rows are reprocessed, so it stays a gauge.
            "# HELP paperless_archivist_model_errors_total Jobs with model-stage error messages\n",
            "# TYPE paperless_archivist_model_errors_total gauge\n",
            "paperless_archivist_model_errors_total {}\n",
            // apply_* totals are now real monotone counters backed by
            // `metrics_counters`, incremented once at each apply event.
            "# HELP paperless_archivist_apply_success_total Successful Paperless apply operations (monotone counter)\n",
            "# TYPE paperless_archivist_apply_success_total counter\n",
            "paperless_archivist_apply_success_total {}\n",
            "# HELP paperless_archivist_apply_failure_total Failed Paperless apply operations (monotone counter)\n",
            "# TYPE paperless_archivist_apply_failure_total counter\n",
            "paperless_archivist_apply_failure_total {}\n",
            "# HELP paperless_archivist_apply_latency_ms_sum Sum of observed Paperless apply latency in milliseconds (retained in audit log)\n",
            "# TYPE paperless_archivist_apply_latency_ms_sum gauge\n",
            "paperless_archivist_apply_latency_ms_sum {}\n",
            "# HELP paperless_archivist_apply_latency_ms_count Count of observed Paperless apply latency samples (retained in audit log)\n",
            "# TYPE paperless_archivist_apply_latency_ms_count gauge\n",
            "paperless_archivist_apply_latency_ms_count {}\n",
            "# HELP paperless_archivist_apply_latency_ms_p95 Lifetime p95 of observed Paperless apply latency in milliseconds (over retained audit events)\n",
            "# TYPE paperless_archivist_apply_latency_ms_p95 gauge\n",
            "paperless_archivist_apply_latency_ms_p95 {}\n",
            // Per-stage latency gauges sourced from ai_artifacts.duration_ms over
            // a recent 24h window (see metrics_snapshot). They can decrease as the
            // window rolls forward, so they are gauges, not counters.
            "# HELP paperless_archivist_ocr_latency_ms_count Count of OCR-stage latency samples observed in the last 24h\n",
            "# TYPE paperless_archivist_ocr_latency_ms_count gauge\n",
            "paperless_archivist_ocr_latency_ms_count {}\n",
            "# HELP paperless_archivist_ocr_latency_ms_p95 p95 of OCR-stage latency in milliseconds over the last 24h\n",
            "# TYPE paperless_archivist_ocr_latency_ms_p95 gauge\n",
            "paperless_archivist_ocr_latency_ms_p95 {}\n",
            "# HELP paperless_archivist_metadata_latency_ms_count Count of metadata-stage latency samples observed in the last 24h\n",
            "# TYPE paperless_archivist_metadata_latency_ms_count gauge\n",
            "paperless_archivist_metadata_latency_ms_count {}\n",
            "# HELP paperless_archivist_metadata_latency_ms_p95 p95 of metadata-stage latency in milliseconds over the last 24h\n",
            "# TYPE paperless_archivist_metadata_latency_ms_p95 gauge\n",
            "paperless_archivist_metadata_latency_ms_p95 {}\n",
            "# HELP paperless_archivist_oldest_queued_age_seconds Age in seconds of the oldest queued job (now() - min(run_after) over status='queued')\n",
            "# TYPE paperless_archivist_oldest_queued_age_seconds gauge\n",
            "paperless_archivist_oldest_queued_age_seconds {}\n"
        ),
        snapshot.jobs_queued,
        snapshot.jobs_running,
        snapshot.jobs_failed,
        snapshot.jobs_succeeded,
        snapshot.reviews_pending,
        snapshot.runs_active,
        snapshot.audit_events,
        counter("selector_runs_total"),
        counter("selector_documents_queued_total"),
        counter("job_retries_scheduled_total"),
        counter("job_failures_total"),
        counter("provider_quota_total"),
        snapshot.model_errors_total,
        counter("apply_success_total"),
        counter("apply_failure_total"),
        snapshot.apply_latency_ms_sum,
        snapshot.apply_latency_ms_count,
        snapshot.apply_latency_ms_p95,
        snapshot.ocr_latency_ms_count,
        snapshot.ocr_latency_ms_p95,
        snapshot.metadata_latency_ms_count,
        snapshot.metadata_latency_ms_p95,
        snapshot.oldest_queued_age_seconds
    );
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct MeResponse {
    username: String,
    roles: Vec<Role>,
    /// Permission flags derived from `roles`, exposed so the frontend can gate
    /// fetches/actions on the same matrix the server enforces rather than on a
    /// hardcoded role name (see #98).
    permissions: PermissionFlags,
    csrf_token: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct PermissionFlags {
    read_dashboard: bool,
    read_runs: bool,
    write_runs: bool,
    read_inventory: bool,
    write_batches: bool,
    use_chat: bool,
    read_reviews: bool,
    write_reviews: bool,
    read_settings: bool,
    write_settings: bool,
    manage_users: bool,
    read_audit: bool,
}

impl PermissionFlags {
    fn from_roles(roles: &[Role]) -> Self {
        Self {
            read_dashboard: roles_have_permission(roles, Permission::ReadDashboard),
            read_runs: roles_have_permission(roles, Permission::ReadRuns),
            write_runs: roles_have_permission(roles, Permission::WriteRuns),
            read_inventory: roles_have_permission(roles, Permission::ReadInventory),
            write_batches: roles_have_permission(roles, Permission::WriteBatches),
            use_chat: roles_have_permission(roles, Permission::UseChat),
            read_reviews: roles_have_permission(roles, Permission::ReadReviews),
            write_reviews: roles_have_permission(roles, Permission::WriteReviews),
            read_settings: roles_have_permission(roles, Permission::ReadSettings),
            write_settings: roles_have_permission(roles, Permission::WriteSettings),
            manage_users: roles_have_permission(roles, Permission::ManageUsers),
            read_audit: roles_have_permission(roles, Permission::ReadAudit),
        }
    }
}

#[derive(Debug, Serialize)]
struct OidcConfigResponse {
    enabled: bool,
    login_url: Option<String>,
    provider: Option<String>,
    paperless_login_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct OidcLoginQuery {
    return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OidcCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

struct OidcValues<'a> {
    issuer_url: &'a str,
    client_id: &'a str,
    client_secret: &'a SecretString,
    redirect_uri: &'a str,
}

#[derive(Debug, Deserialize)]
struct OidcProviderMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    /// Optional per the discovery spec. Many IdPs (ZITADEL by default) return
    /// profile/email/roles claims here rather than inlining them in the ID
    /// token, so the callback fetches it to populate roles and the username/
    /// email allowlist. #299.
    #[serde(default)]
    userinfo_endpoint: Option<String>,
    #[serde(default)]
    id_token_signing_alg_values_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OidcTokenResponse {
    access_token: String,
    id_token: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct OidcIdClaims {
    iss: String,
    sub: String,
    aud: Value,
    exp: i64,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    at_hash: Option<String>,
    /// Every claim not captured by a named field above — including the IdP's
    /// roles claim, whose name is operator-configurable and not a fixed Rust
    /// field (ZITADEL uses URN-style claim names). Read via [`oidc_idp_roles`].
    #[serde(flatten)]
    additional: serde_json::Map<String, Value>,
}

impl OidcIdClaims {
    /// Merge claims fetched from the IdP userinfo endpoint. The signed ID token
    /// wins for identity fields it already carries; userinfo only FILLS gaps and
    /// contributes claims the ID token lacked (notably the roles claim, which
    /// ZITADEL returns from userinfo rather than the ID token by default). The
    /// caller must have verified the userinfo `sub` equals the ID token `sub`.
    fn merge_userinfo(&mut self, userinfo: serde_json::Map<String, Value>) {
        if self.preferred_username.is_none()
            && let Some(value) = userinfo.get("preferred_username").and_then(Value::as_str)
        {
            self.preferred_username = Some(value.to_owned());
        }
        if self.email.is_none()
            && let Some(value) = userinfo.get("email").and_then(Value::as_str)
        {
            self.email = Some(value.to_owned());
        }
        if self.email_verified.is_none()
            && let Some(value) = userinfo.get("email_verified").and_then(Value::as_bool)
        {
            self.email_verified = Some(value);
        }
        // Contribute any claim the ID token did not already carry (roles, etc.);
        // never overwrite a signed ID-token claim.
        for (key, value) in userinfo {
            self.additional.entry(key).or_insert(value);
        }
    }
}

async fn oidc_config(State(state): State<AppState>) -> Json<OidcConfigResponse> {
    let paperless_login_enabled = get_runtime_settings(&state.pool)
        .await
        .map(|settings| settings.paperless.login_bridge_enabled)
        .unwrap_or(false);
    Json(OidcConfigResponse {
        enabled: state.config.oidc_enabled,
        login_url: state
            .config
            .oidc_enabled
            .then(|| "/api/auth/oidc/login".to_owned()),
        provider: state.config.oidc_enabled.then(|| "ZITADEL".to_owned()),
        paperless_login_enabled,
    })
}

async fn oidc_login(
    State(state): State<AppState>,
    Query(query): Query<OidcLoginQuery>,
) -> ApiResult<Redirect> {
    let values = oidc_values(&state.config)?;
    let http_client = oidc_http_client()?;
    let provider_metadata = oidc_discover(&http_client, values.issuer_url).await?;
    let csrf_state = random_token();
    let nonce = random_token();
    let pkce_verifier = random_token();
    let auth_url = oidc_authorization_url(
        &provider_metadata,
        &values,
        &oidc_scopes(&state.config),
        &csrf_state,
        &nonce,
        &pkce_challenge(&pkce_verifier),
    )?;
    let return_to = safe_return_to(query.return_to.as_deref());
    create_oidc_login_state(
        &state.pool,
        &hash_token(&csrf_state),
        &nonce,
        &pkce_verifier,
        return_to.as_deref(),
        Utc::now() + Duration::minutes(10),
    )
    .await?;

    Ok(Redirect::temporary(&auth_url))
}

async fn oidc_callback(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<OidcCallbackQuery>,
) -> ApiResult<Response> {
    let source_ip = request_source_ip(&state, &headers, Some(peer));
    let user_agent = request_user_agent(&headers);
    if let Some(error) = query.error {
        let description = query.error_description.unwrap_or_default();
        return Err(ApiError::unauthorized(format!(
            "OIDC login failed: {error} {description}"
        )));
    }
    let code = query
        .code
        .ok_or_else(|| ApiError::bad_request("missing OIDC code"))?;
    let state_value = query
        .state
        .ok_or_else(|| ApiError::bad_request("missing OIDC state"))?;
    let login_state = consume_oidc_login_state(&state.pool, &hash_token(&state_value))
        .await?
        .ok_or_else(|| ApiError::unauthorized("invalid or expired OIDC state"))?;

    let values = oidc_values(&state.config)?;
    let http_client = oidc_http_client()?;
    let provider_metadata = oidc_discover(&http_client, values.issuer_url).await?;
    let token_response = oidc_exchange_code(
        &http_client,
        &provider_metadata,
        &values,
        &code,
        &login_state.pkce_verifier,
    )
    .await?;
    let mut claims = oidc_verify_id_token(
        &http_client,
        &provider_metadata,
        &values,
        &token_response.id_token,
        &login_state.nonce,
    )
    .await?;
    if let Some(expected_hash) = claims.at_hash.as_deref() {
        let header = decode_header(&token_response.id_token).map_err(|error| {
            ApiError::unauthorized(format!("OIDC ID token header error: {error}"))
        })?;
        if !oidc_access_token_hash_matches(header.alg, &token_response.access_token, expected_hash)
        {
            return Err(ApiError::unauthorized("OIDC access token hash mismatch"));
        }
    }

    // ZITADEL (and many IdPs) return profile/email/roles from the userinfo
    // endpoint rather than inlining them in the ID token — a minimal ID token
    // then carries only `sub`, which the username/email allowlist and the roles
    // claim cannot match. Fetch userinfo (sub-verified, best-effort) and merge
    // it so role-based admin and the allowlist work regardless of whether the
    // IdP inlines user info into the ID token. #299.
    if let Some(userinfo_endpoint) = provider_metadata.userinfo_endpoint.as_deref() {
        let id_sub = claims.sub.clone();
        match oidc_fetch_userinfo(
            &http_client,
            userinfo_endpoint,
            &token_response.access_token,
            &id_sub,
        )
        .await
        {
            Some(userinfo) => claims.merge_userinfo(userinfo),
            None => warn!(
                "OIDC userinfo fetch returned no usable claims; proceeding on the ID token alone"
            ),
        }
    }

    let subject = claims.sub.as_str();
    // OIDC Core §5.7: the email claim is only trustworthy when the IdP
    // asserts email_verified=true. An unverified email must not influence
    // admin-role mapping, account linking, or the derived username —
    // otherwise an attacker who can set a free-form email at the IdP could
    // escalate to the allowlisted admin or take over a local account.
    let email = oidc_verified_email(&claims);
    let claims_degraded = oidc_claims_degraded(&claims);
    let username = oidc_username(
        claims
            .preferred_username
            .as_deref()
            .or(email)
            .unwrap_or(subject),
    );
    let resolution = oidc_roles(&state.config, &claims, subject, &username, email)?;
    let roles = resolution.roles;
    // Degraded ID token (#299): without preferred_username and a verified
    // email the *identity-derived* roles (allowlist by username/email) can't be
    // matched, so a returning user must keep their existing roles instead of
    // being silently demoted. This guard only applies when the roles are NOT
    // authoritative — if the IdP asserted a roles claim (or the subject is
    // allowlisted), those roles win, including a deliberate demotion. #289/#299.
    let preserve_existing_roles =
        !resolution.authoritative && claims_degraded && !roles.contains(&Role::Admin);
    if claims_degraded && !resolution.authoritative {
        warn!(
            subject_hash = %hash_token(subject),
            email_claim_present = claims.email.is_some(),
            "OIDC ID token carries neither preferred_username nor a verified email and no IdP \
             roles claim; existing roles are preserved unless the subject is allowlisted \
             (configure the IdP to include profile/email or roles claims in the ID token)"
        );
    }
    if !resolution.idp_claim_present {
        // Self-diagnosis for the common misconfiguration where the IdP is not
        // asserting roles into the ID token (so role-based admin can't work).
        // Log only the claim *names* present (never values) so an operator can
        // see whether the roles claim is there and under what name. #299.
        let available_claims: Vec<&str> = claims.additional.keys().map(String::as_str).collect();
        warn!(
            configured_roles_claim = %state.config.oidc_roles_claim,
            available_claims = ?available_claims,
            "OIDC ID token carried no recognizable roles claim; roles fall back to the admin \
             allowlist/defaults. Enable role assertion into the ID token at the IdP (ZITADEL: \
             'Assert Roles on Authentication'), or set ARCHIVIST_OIDC_ROLES_CLAIM to one of the \
             claim names listed here."
        );
    }
    let allow_username_link = roles.contains(&Role::Admin);
    let disabled_password_hash = hash_password(&random_token())?;
    let user = upsert_oidc_user(
        &state.pool,
        OidcUserInput {
            provider: "zitadel",
            subject,
            username: &username,
            email,
            disabled_password_hash: &disabled_password_hash,
            roles: &roles,
            allow_username_link,
            allow_email_link: state.config.oidc_allow_email_link,
            preserve_existing_roles,
        },
    )
    .await?;
    if !user.enabled {
        return Err(ApiError::unauthorized("user is disabled"));
    }

    record_login_success(
        &state.pool,
        user.id,
        source_ip.as_deref(),
        user_agent.as_deref(),
    )
    .await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "auth.oidc_login_success".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(user.id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({
                "username": user.username,
                "issuer": values.issuer_url,
                "subject_hash": hash_token(subject)
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: source_ip.clone(),
            user_agent: user_agent.clone(),
        },
    )
    .await?;

    let (session_token, csrf_token) = issue_session(&state, user.id).await?;
    let mut response =
        Redirect::to(login_state.return_to.as_deref().unwrap_or("/")).into_response();
    set_session_cookies(
        response.headers_mut(),
        &state.config,
        &session_token,
        &csrf_token,
    )?;
    Ok(response)
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
    let source_ip = request_source_ip(&state, &headers, Some(peer));
    let user_agent = request_user_agent(&headers);
    let user = find_user_for_login(&state.pool, &request.username).await?;
    let Some(user) = user else {
        // Spend the same Argon2id time as a real account so an attacker can't
        // distinguish existing from non-existing usernames by response latency.
        verify_dummy_password(&request.password);
        record_login_failure(
            &state.pool,
            None,
            &request.username,
            source_ip.as_deref(),
            user_agent.as_deref(),
        )
        .await?;
        return Err(ApiError::unauthorized("invalid credentials"));
    };
    // Always run the password verification first (even for locked/disabled
    // accounts) so none of the rejection paths can be told apart from a wrong
    // password by response latency. #291
    let password_ok = verify_password(&user, &request.password)?;
    if user
        .locked_until
        .is_some_and(|locked_until| locked_until > Utc::now())
    {
        return Err(ApiError::unauthorized("invalid credentials"));
    }
    if !user.enabled || !password_ok {
        record_login_failure(
            &state.pool,
            Some(user.id),
            &request.username,
            source_ip.as_deref(),
            user_agent.as_deref(),
        )
        .await?;
        return Err(ApiError::unauthorized("invalid credentials"));
    }

    record_login_success(
        &state.pool,
        user.id,
        source_ip.as_deref(),
        user_agent.as_deref(),
    )
    .await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "auth.login_success".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(user.id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({ "username": user.username })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: source_ip.clone(),
            user_agent: user_agent.clone(),
        },
    )
    .await?;

    let (session_token, csrf_token) = issue_session(&state, user.id).await?;

    let permissions = PermissionFlags::from_roles(&user.roles);
    let body = Json(MeResponse {
        username: user.username,
        roles: user.roles,
        permissions,
        csrf_token: Some(csrf_token.clone()),
    });
    let mut response = body.into_response();
    set_session_cookies(
        response.headers_mut(),
        &state.config,
        &session_token,
        &csrf_token,
    )?;
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct PaperlessTokenResponse {
    token: String,
}

#[derive(Debug, Deserialize)]
struct PaperlessUiSettingsResponse {
    user: PaperlessUserIdentity,
}

#[derive(Debug, Deserialize)]
struct PaperlessUserIdentity {
    id: i64,
    username: String,
}

struct PaperlessBridgeIdentity {
    subject: String,
    username: String,
}

async fn paperless_login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
    let source_ip = request_source_ip(&state, &headers, Some(peer));
    let user_agent = request_user_agent(&headers);
    let settings = get_runtime_settings(&state.pool).await?;
    if !settings.paperless.login_bridge_enabled {
        return Err(ApiError::forbidden("Paperless login bridge is disabled"));
    }
    let paperless_username = request.username.trim();
    if paperless_username.is_empty() || request.password.is_empty() {
        return Err(ApiError::unauthorized("invalid credentials"));
    }

    let bridge_identity = match verify_paperless_credentials(
        &settings,
        paperless_username,
        &request.password,
    )
    .await
    {
        Ok(identity) => identity,
        Err(_) => {
            record_login_failure(
                &state.pool,
                None,
                &paperless_bridge_username(paperless_username),
                source_ip.as_deref(),
                user_agent.as_deref(),
            )
            .await?;
            return Err(ApiError::unauthorized("invalid credentials"));
        }
    };

    let username = paperless_bridge_username(&bridge_identity.username);
    let user = match find_paperless_bridge_user(&state.pool, &bridge_identity.subject).await? {
        Some(user) => user,
        None => {
            let disabled_password_hash = hash_password(&random_token())?;
            find_or_create_paperless_bridge_user(
                &state.pool,
                &username,
                &bridge_identity.subject,
                &disabled_password_hash,
            )
            .await?
        }
    };
    if !user.enabled {
        return Err(ApiError::unauthorized("user is disabled"));
    }

    record_login_success(
        &state.pool,
        user.id,
        source_ip.as_deref(),
        user_agent.as_deref(),
    )
    .await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "auth.paperless_login_success".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(user.id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({
                "username": user.username,
                "paperless_username": bridge_identity.username
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: source_ip.clone(),
            user_agent: user_agent.clone(),
        },
    )
    .await?;

    let (session_token, csrf_token) = issue_session(&state, user.id).await?;
    let permissions = PermissionFlags::from_roles(&user.roles);
    let body = Json(MeResponse {
        username: user.username,
        roles: user.roles,
        permissions,
        csrf_token: Some(csrf_token.clone()),
    });
    let mut response = body.into_response();
    set_session_cookies(
        response.headers_mut(),
        &state.config,
        &session_token,
        &csrf_token,
    )?;
    Ok(response)
}

async fn verify_paperless_credentials(
    settings: &RuntimeSettings,
    username: &str,
    password: &str,
) -> Result<PaperlessBridgeIdentity> {
    // Same up-front SSRF validation as the other outbound tester paths —
    // this endpoint forwards user credentials to the configured URL.
    let base_url = validate_outbound_url(settings.paperless.base_url.trim())
        .await
        .map_err(|error| anyhow!("Paperless base URL rejected: {}", error.message))?;
    let api_root = base_url.join("api/").context("build Paperless API root")?;
    let token_url = api_root
        .join("token/")
        .context("build Paperless token URL")?;
    let client = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(
            settings.paperless.timeout_seconds.clamp(1, 120),
        ))
        // Refuse redirects so a 3xx response can't steer this credentialed
        // request to an internal address after the SSRF guard validated only
        // the originally supplied URL.
        .redirect(reqwest::redirect::Policy::none())
        // No connect-time IP-pinning: the DNS-rebinding TOCTOU is an accepted
        // residual risk for this operator-configured Paperless host (the
        // pinning resolver was reverted, see #183).
        .build()
        .context("build Paperless login HTTP client")?;
    let response = client
        .post(token_url)
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .context("connect to Paperless token endpoint")?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("Paperless returned {status}"));
    }
    let token: PaperlessTokenResponse = response
        .json()
        .await
        .context("decode Paperless token response")?;
    let token = token.token.trim();
    if token.is_empty() {
        return Err(anyhow!("Paperless returned an empty token"));
    }
    let identity_url = api_root
        .join("ui_settings/")
        .context("build Paperless identity URL")?;
    let identity_response = client
        .get(identity_url)
        .header(reqwest::header::AUTHORIZATION, format!("Token {token}"))
        .send()
        .await
        .context("read authenticated Paperless identity")?;
    let identity_status = identity_response.status();
    if !identity_status.is_success() {
        return Err(anyhow!(
            "Paperless identity endpoint returned {identity_status}"
        ));
    }
    let identity: PaperlessUiSettingsResponse = identity_response
        .json()
        .await
        .context("decode authenticated Paperless identity")?;
    let identity_username = identity.user.username.trim();
    if identity.user.id <= 0 || identity_username.is_empty() {
        return Err(anyhow!("Paperless returned an invalid user identity"));
    }
    Ok(PaperlessBridgeIdentity {
        subject: paperless_user_subject(&api_root, identity.user.id),
        username: identity_username.to_owned(),
    })
}

fn paperless_user_subject(api_root: &Url, user_id: i64) -> String {
    let instance_hash = hex::encode(Sha256::digest(api_root.as_str().as_bytes()));
    format!("instance-sha256:{instance_hash}:user-id:{user_id}")
}

fn paperless_bridge_username(username: &str) -> String {
    let normalized = oidc_username(username);
    format!("paperless-{normalized}")
}

async fn logout(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    auth: Authenticated,
) -> ApiResult<impl IntoResponse> {
    let source_ip = request_source_ip(&state, &headers, Some(peer));
    let user_agent = request_user_agent(&headers);
    if let (Some(session_id), Some(user_id)) = (auth.0.session_id, auth.0.user_id) {
        archivist_db::revoke_session(
            &state.pool,
            session_id,
            user_id,
            source_ip.as_deref(),
            user_agent.as_deref(),
        )
        .await?;
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        header_value(expire_cookie(
            SESSION_COOKIE,
            true,
            state.config.cookie_secure,
        ))?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        header_value(expire_cookie(
            CSRF_COOKIE,
            false,
            state.config.cookie_secure,
        ))?,
    );
    Ok(response)
}

async fn me(auth: Authenticated) -> ApiResult<Json<MeResponse>> {
    let permissions = PermissionFlags::from_roles(&auth.0.roles);
    Ok(Json(MeResponse {
        username: auth.0.username.unwrap_or_else(|| "api-token".to_owned()),
        roles: auth.0.roles,
        permissions,
        csrf_token: None,
    }))
}

#[derive(Debug, Deserialize)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<ChangePasswordRequest>,
) -> ApiResult<Response> {
    let user_id = auth
        .0
        .user_id
        .ok_or_else(|| ApiError::forbidden("password changes require a user session"))?;
    let username = auth
        .0
        .username
        .as_deref()
        .ok_or_else(|| ApiError::forbidden("password changes require a user session"))?;
    let user = find_user_for_login(&state.pool, username)
        .await?
        .ok_or_else(|| ApiError::unauthorized("invalid user"))?;
    if !verify_password(&user, &request.current_password)? {
        return Err(ApiError::unauthorized("invalid current password"));
    }
    validate_password_strength(&request.new_password).map_err(ApiError::bad_request)?;
    let password_hash = hash_password(&request.new_password)?;
    update_user_password_hash(
        &state.pool,
        user_id,
        &password_hash,
        user_id,
        "auth.password_changed",
    )
    .await?;
    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        header_value(expire_cookie(
            SESSION_COOKIE,
            true,
            state.config.cookie_secure,
        ))?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        header_value(expire_cookie(
            CSRF_COOKIE,
            false,
            state.config.cookie_secure,
        ))?,
    );
    Ok(response)
}

async fn sessions(State(state): State<AppState>, auth: Authenticated) -> ApiResult<Json<Value>> {
    let user_id = session_listing_user_filter(&auth.0)?;
    Ok(Json(
        json!({ "items": list_sessions(&state.pool, user_id).await? }),
    ))
}

fn session_listing_user_filter(auth: &AuthContext) -> Result<Option<Uuid>, ApiError> {
    let user_id = require_user_session(auth, "session listing requires a user session")?;
    Ok(
        if roles_have_permission(&auth.roles, Permission::ManageUsers) {
            None
        } else {
            Some(user_id)
        },
    )
}

async fn revoke_session_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    let actor_id = require_user_session(&auth.0, "session revocation requires a user session")?;
    revoke_session_by_admin(&state.pool, id, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn settings(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<RuntimeSettings>> {
    require(&auth.0, Permission::ReadSettings)?;
    Ok(Json(get_runtime_settings(&state.pool).await?))
}

#[derive(Debug, Deserialize)]
struct UpdateSettingsRequest {
    settings: RuntimeSettings,
    paperless_token: Option<String>,
    notification_webhook_url: Option<String>,
    provider_secrets: Option<HashMap<String, String>>,
}

fn canonicalize_provider_secrets(
    settings: &RuntimeSettings,
    provider_secrets: HashMap<String, String>,
) -> ApiResult<HashMap<String, String>> {
    let mut canonical = HashMap::with_capacity(provider_secrets.len());
    for (submitted_name, secret) in provider_secrets {
        let submitted_name = submitted_name.trim();
        let provider = settings
            .ai
            .providers
            .iter()
            .find(|provider| provider.name.eq_ignore_ascii_case(submitted_name))
            .ok_or_else(|| {
                ApiError::bad_request(format!(
                    "AI provider secret target '{submitted_name}' is not configured"
                ))
            })?;
        if canonical.insert(provider.name.clone(), secret).is_some() {
            return Err(ApiError::bad_request(format!(
                "multiple AI provider secrets resolve to '{}'",
                provider.name
            )));
        }
    }
    Ok(canonical)
}

fn prepare_settings_update(request: &mut UpdateSettingsRequest) -> ApiResult<()> {
    // Runtime normalization can append provider presets, so it must happen
    // before provider validation and the enabled-URL preflight. Performing it
    // again after those checks could create unchecked persisted providers.
    request.settings = std::mem::take(&mut request.settings).normalized();
    request
        .settings
        .ai
        .normalize_and_validate_providers()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    if let Some(provider_secrets) = request.provider_secrets.take() {
        request.provider_secrets = Some(canonicalize_provider_secrets(
            &request.settings,
            provider_secrets,
        )?);
    }
    Ok(())
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty)
)]
async fn update_settings(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(mut request): Json<UpdateSettingsRequest>,
) -> ApiResult<Json<RuntimeSettings>> {
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id = require_user_session(&auth.0, "settings updates require a user session")?;
    Span::current().record("user_id", tracing::field::display(actor_id));

    // Validate and canonicalize every name-based AI reference before the
    // first secret or settings write. Any failure therefore leaves both the
    // runtime settings and all encrypted-secret mappings untouched.
    prepare_settings_update(&mut request)?;

    // Config-time SSRF guard (SECURITY_DESIGN §4.3): outbound URLs are
    // validated when they are PERSISTED, not only when the operator happens
    // to press a "test" button — the worker consumes them verbatim on its
    // hot path without re-validating.
    let paperless_url = request.settings.paperless.base_url.trim();
    if !paperless_url.is_empty() {
        validate_outbound_url_for_save(paperless_url)
            .await
            .map_err(|error| {
                ApiError::bad_request(format!("Paperless base URL: {}", error.message))
            })?;
    }
    // public_url is rendered as a clickable link in the browser (not an
    // outbound server request), so it needs http/https scheme validation — not
    // the SSRF/dangerous-IP check (an intranet public_url is legitimate) — to
    // stop a settings admin planting a `javascript:` URL that executes for
    // lower-privileged users viewing the inventory. #290
    if let Some(public_url) = request.settings.paperless.public_url.as_deref() {
        let public_url = public_url.trim();
        if !public_url.is_empty() {
            let parsed = Url::parse(public_url)
                .map_err(|_| ApiError::bad_request("Paperless public URL is not a valid URL"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(ApiError::bad_request(
                    "Paperless public URL scheme must be http or https",
                ));
            }
        }
    }
    // Disabled providers/profiles may carry placeholder URLs (the seeded
    // `openai-compatible` example points at localhost); they are not active
    // outbound targets, and enabling one is itself a settings save — the
    // guard fires then.
    for profile in &request.settings.paperless.archive_profiles {
        let base_url = profile.base_url.trim();
        if !profile.enabled || base_url.is_empty() {
            continue;
        }
        validate_outbound_url_for_save(base_url)
            .await
            .map_err(|error| {
                ApiError::bad_request(format!(
                    "archive profile '{}' base URL: {}",
                    profile.name, error.message
                ))
            })?;
    }
    for provider in &request.settings.ai.providers {
        if !provider.enabled {
            continue;
        }
        let base_url = if provider.name.eq_ignore_ascii_case("ollama") {
            request.settings.ai.ollama_base_url.trim()
        } else {
            provider.base_url.trim()
        };
        validate_outbound_url_for_save(base_url)
            .await
            .map_err(|error| {
                ApiError::bad_request(format!(
                    "AI provider '{}' base URL: {}",
                    provider.name, error.message
                ))
            })?;
    }
    if let Some(webhook_url) = request.notification_webhook_url.as_deref() {
        let webhook_url = webhook_url.trim();
        if !webhook_url.is_empty() {
            validate_outbound_url_for_save(webhook_url)
                .await
                .map_err(|error| {
                    ApiError::bad_request(format!("notification webhook URL: {}", error.message))
                })?;
        }
    }

    if let Some(token) = request
        .paperless_token
        .filter(|token| !token.trim().is_empty())
    {
        let secret_id = upsert_encrypted_secret(
            &state.pool,
            &state.config.secret_key,
            "paperless-api-token",
            &SecretString::from(token),
            actor_id,
        )
        .await?;
        request.settings.paperless.token_secret_id = Some(secret_id);
    }
    if let Some(provider_secrets) = request.provider_secrets.take() {
        for (provider_name, secret) in provider_secrets {
            if secret.trim().is_empty() {
                continue;
            }
            let secret_id = upsert_encrypted_secret(
                &state.pool,
                &state.config.secret_key,
                &format!("ai-provider-{provider_name}-api-key"),
                &SecretString::from(secret),
                actor_id,
            )
            .await?;
            if let Some(provider) = request
                .settings
                .ai
                .providers
                .iter_mut()
                .find(|provider| provider.name == provider_name)
            {
                provider.secret_id = Some(secret_id);
            }
        }
    }
    if let Some(webhook_url) = request
        .notification_webhook_url
        .take()
        .filter(|value| !value.trim().is_empty())
    {
        let secret_id = upsert_encrypted_secret(
            &state.pool,
            &state.config.secret_key,
            "notification-webhook-url",
            &SecretString::from(webhook_url),
            actor_id,
        )
        .await?;
        request.settings.notifications.webhook_url_secret_id = Some(secret_id);
    }
    // Capture the AI model identity before the save so we can detect a switch
    // and react to it below. A failed read just disables the optimization.
    let previous_models = get_runtime_settings(&state.pool)
        .await
        .ok()
        .map(|settings| {
            (
                settings.ai.default_provider,
                settings.ai.default_text_model,
                settings.ai.default_vision_model,
            )
        });

    // The preflight normalized the final provider inventory before any URL or
    // secret checks. Persist — and below, return — that same normalized object
    // so the PUT response, audit payload and next GET agree (#313).
    update_runtime_settings(&state.pool, &request.settings, actor_id).await?;
    info!(%actor_id, "runtime settings updated");

    // When the operator switches the AI model/provider, a backlog that an old
    // provider's cooldown parked (run_after pushed far into the future) would
    // otherwise keep waiting out that now-irrelevant cooldown. Operators expect
    // a model switch to take effect immediately, so drop the stale cooldowns
    // and wake the parked jobs to rerun under the new model right away. The
    // release is scoped to cooldown-parked jobs (run_after beyond the regular
    // retry-backoff horizon) so a model switch does not also collapse the
    // backoff+jitter spacing of unrelated transient retries (#313).
    let new_ai = &request.settings.ai;
    let model_changed = previous_models
        .as_ref()
        .is_some_and(|(provider, text, vision)| {
            provider != &new_ai.default_provider
                || text != &new_ai.default_text_model
                || vision != &new_ai.default_vision_model
        });
    if model_changed {
        let cleared = archivist_db::clear_all_provider_cooldowns(&state.pool)
            .await
            .unwrap_or_else(|error| {
                warn!(error = %error, "failed to clear cooldowns after model change");
                0
            });
        let released = archivist_db::release_cooldown_parked_retries(&state.pool)
            .await
            .unwrap_or_else(|error| {
                warn!(error = %error, "failed to release parked jobs after model change");
                0
            });
        info!(
            %actor_id,
            cleared,
            released,
            "AI model changed; cleared provider cooldowns and released cooldown-parked jobs"
        );
    }

    Ok(Json(request.settings))
}

async fn secret_references(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadSettings)?;
    Ok(Json(
        json!({ "items": list_secret_references(&state.pool).await? }),
    ))
}

async fn prompts(State(state): State<AppState>, auth: Authenticated) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadSettings)?;
    Ok(Json(json!({ "items": list_prompts(&state.pool).await? })))
}

async fn prompt_usage(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadSettings)?;
    Ok(Json(
        json!({ "items": list_prompt_usage(&state.pool).await? }),
    ))
}

async fn prompt_experiments(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadSettings)?;
    Ok(Json(
        json!({ "items": list_prompt_experiments(&state.pool).await? }),
    ))
}

#[derive(Debug, Deserialize)]
struct CreatePromptRequest {
    stage: Stage,
    name: String,
    content: String,
    output_schema: Option<Value>,
    activate: Option<bool>,
}

async fn create_prompt_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<CreatePromptRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id = require_user_session(&auth.0, "prompt management requires a user session")?;
    let id = archivist_db::create_prompt(
        &state.pool,
        request.stage,
        &request.name,
        &request.content,
        request.output_schema,
        actor_id,
    )
    .await?;
    if request.activate.unwrap_or(false) {
        archivist_db::activate_prompt(&state.pool, id, actor_id).await?;
    }
    Ok(Json(json!({ "id": id })))
}

async fn activate_prompt_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id = require_user_session(&auth.0, "prompt management requires a user session")?;
    archivist_db::activate_prompt(&state.pool, id, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct TestPromptRequest {
    stage: Stage,
    content: String,
    sample_text: Option<String>,
    paperless_document_id: Option<i32>,
    provider_name: Option<String>,
    model: Option<String>,
}

async fn test_prompt_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<TestPromptRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id = require_user_session(&auth.0, "prompt tests require a user session")?;
    if request.content.trim().is_empty() {
        return Err(ApiError::bad_request("prompt content must not be empty"));
    }

    let settings = get_runtime_settings(&state.pool).await?;
    let sample_text = prompt_test_sample_text(&state, &settings, &request).await?;
    let provider = prompt_test_provider(&settings, &request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    let mut chat_request = match request.stage {
        Stage::Ocr => build_ocr_prompt_test_chat_request(&sample_text),
        Stage::Metadata => {
            let enabled =
                MetadataFieldFlags::from_enabled_stages(&settings.workflow.enabled_stages);
            let catalog = load_metadata_prompt_test_catalog(&state.pool, enabled).await?;
            build_metadata_prompt_test_chat_request(
                &settings,
                &provider.tuning,
                &sample_text,
                catalog,
            )
            .map_err(|error| ApiError::bad_request(error.to_string()))?
        }
        Stage::Apply => {
            return Err(ApiError::bad_request(format!(
                "prompt testing is not supported for stage {}",
                request.stage
            )));
        }
    };
    apply_prompt_test_system_prompt(&mut chat_request, &request.content);
    chat_request.model = provider.model.clone();
    apply_api_provider_tuning(&provider, &mut chat_request);

    let response = chat_with_api_provider(&state, &provider, chat_request.clone()).await?;
    let parsed = parse_prompt_test_output(request.stage, &response.text);
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "prompt.tested".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: request.paperless_document_id,
            before: None,
            after: None,
            metadata: Some(json!({
                "stage": request.stage,
                "provider": provider.name,
                "model": provider.model,
                "sample_chars": sample_text.chars().count(),
                "duration_ms": response.duration_ms,
                "valid": parsed.validation_errors.is_empty()
            })),
            outcome: if parsed.validation_errors.is_empty() {
                "success".to_owned()
            } else {
                "validation_failed".to_owned()
            },
            error_message: parsed.validation_errors.first().cloned(),
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    Ok(Json(json!({
        "provider": response.provider,
        "model": response.model,
        "stage": request.stage,
        "raw_text": response.text,
        "parsed": parsed.parsed,
        "validation_errors": parsed.validation_errors,
        "warnings": parsed.warnings,
        "duration_ms": response.duration_ms
    })))
}

fn prompt_test_provider(
    settings: &RuntimeSettings,
    request: &TestPromptRequest,
) -> Result<ApiProvider> {
    let mut provider = if let Some(provider_name) = request
        .provider_name
        .as_deref()
        .filter(|provider_name| !provider_name.trim().is_empty())
    {
        provider_by_name(settings, provider_name)?
    } else {
        match request.stage {
            Stage::Metadata => provider_for_stage_text(settings, Stage::Metadata)?,
            Stage::Ocr | Stage::Apply => provider_for_default_text(settings)?,
        }
    };
    if let Some(model) = request
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
    {
        provider.model = model.trim().to_owned();
    }
    Ok(provider)
}

#[derive(Debug)]
struct PromptTestParsed {
    parsed: Value,
    validation_errors: Vec<String>,
    warnings: Vec<String>,
}

async fn prompt_test_sample_text(
    state: &AppState,
    settings: &RuntimeSettings,
    request: &TestPromptRequest,
) -> ApiResult<String> {
    if let Some(sample_text) = request
        .sample_text
        .as_deref()
        .filter(|sample_text| !sample_text.trim().is_empty())
    {
        return Ok(sample_text.trim().chars().take(20_000).collect());
    }
    if let Some(document_id) = request.paperless_document_id {
        if document_id <= 0 {
            return Err(ApiError::bad_request(
                "paperless_document_id must be positive",
            ));
        }
        let paperless =
            paperless_client_from_settings(&state.pool, &state.config, settings).await?;
        let document = paperless.get_document(document_id).await?;
        if let Some(content) = document
            .content
            .filter(|content| !content.trim().is_empty())
        {
            return Ok(content.trim().chars().take(20_000).collect());
        }
        return Err(ApiError::bad_request(
            "selected Paperless document has no content to test against",
        ));
    }
    Err(ApiError::bad_request(
        "provide sample_text or paperless_document_id",
    ))
}

fn build_ocr_prompt_test_chat_request(sample_text: &str) -> ChatRequest {
    ChatRequest {
        model: String::new(),
        system_prompt: String::new(),
        user_prompt: format!(
            "Test this OCR prompt against sample text. Return the best OCR text only.\n\nSample text:\n{}",
            sample_text.chars().take(12_000).collect::<String>()
        ),
        temperature: 0.0,
        num_ctx: None,
        response_schema: None,
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    }
}

#[derive(Debug)]
struct MetadataPromptTestCatalog {
    correspondents: Vec<String>,
    document_types: Vec<String>,
    tags: Vec<String>,
    fields: Vec<(String, Option<String>)>,
}

async fn load_metadata_prompt_test_catalog(
    pool: &DbPool,
    enabled: MetadataFieldFlags,
) -> ApiResult<MetadataPromptTestCatalog> {
    let correspondents = if enabled.correspondent {
        list_allowed_named_entities(pool, "paperless_correspondents").await?
    } else {
        Vec::new()
    };
    let document_types = if enabled.document_type {
        list_allowed_named_entities(pool, "paperless_document_types").await?
    } else {
        Vec::new()
    };
    let tags = if enabled.tags {
        list_allowed_tag_names(pool).await?
    } else {
        Vec::new()
    };
    let fields = if enabled.fields {
        list_custom_fields(pool)
            .await?
            .into_iter()
            .map(|field| (field.name, field.data_type))
            .collect()
    } else {
        Vec::new()
    };
    Ok(MetadataPromptTestCatalog {
        correspondents,
        document_types,
        tags,
        fields,
    })
}

fn build_metadata_prompt_test_chat_request(
    settings: &RuntimeSettings,
    tuning: &EffectiveTuning,
    sample_text: &str,
    catalog: MetadataPromptTestCatalog,
) -> Result<ChatRequest> {
    let enabled = MetadataFieldFlags::from_enabled_stages(&settings.workflow.enabled_stages);
    if !enabled.any() {
        return Err(anyhow!(
            "metadata prompt testing requires the metadata workflow stage to be enabled"
        ));
    }

    let content_lower = sample_text.to_lowercase();
    let allowed_list_max = tuning.allowed_list_max as usize;
    let correspondents =
        prefilter_allowed_list_lower(&content_lower, &catalog.correspondents, allowed_list_max);
    let document_types =
        prefilter_allowed_list_lower(&content_lower, &catalog.document_types, allowed_list_max);
    let tags = prefilter_allowed_list_lower(&content_lower, &catalog.tags, allowed_list_max);
    let fields = catalog
        .fields
        .into_iter()
        .filter(|(name, _)| settings.fields.field_enabled(name))
        .collect::<Vec<_>>();
    let field_names = fields
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let detection = detect_document_language(sample_text);
    let language = PromptLanguageContext::new(&detection, &settings.tagging.tag_output_language);
    let mut request = prompt_for_metadata(
        sample_text,
        &correspondents,
        &document_types,
        &tags,
        &fields,
        &enabled,
        &language,
        tuning.max_tags as usize,
        settings.fields.max_fields,
        "",
    );
    request.response_schema = schema_for_metadata(
        &correspondents,
        &document_types,
        &tags,
        &field_names,
        &enabled,
        tuning.max_tags as usize,
        settings.fields.max_fields,
    );
    Ok(request)
}

fn apply_prompt_test_system_prompt(request: &mut ChatRequest, content: &str) {
    request.system_prompt = content.trim().to_owned();
}

fn parse_prompt_test_output(stage: Stage, text: &str) -> PromptTestParsed {
    match stage {
        Stage::Ocr => PromptTestParsed {
            parsed: json!({ "content": text }),
            validation_errors: Vec::new(),
            warnings: Vec::new(),
        },
        Stage::Metadata => parse_metadata_prompt_test_output(text),
        Stage::Apply => PromptTestParsed {
            parsed: Value::Null,
            validation_errors: vec![format!("unsupported stage: {stage}")],
            warnings: Vec::new(),
        },
    }
}

fn parse_metadata_prompt_test_output(text: &str) -> PromptTestParsed {
    let parsed = parse_metadata_suggestion(text);
    let mut validation_errors = Vec::new();
    if let Some(envelope_error) = parsed.diagnostics.envelope_error {
        validation_errors.push(match envelope_error {
            MetadataEnvelopeError::NoJson => {
                "metadata response envelope is not valid JSON".to_owned()
            }
            MetadataEnvelopeError::NonObject => {
                "metadata response must be a JSON object".to_owned()
            }
        });
    }
    if !parsed.diagnostics.invalid_fields.is_empty() {
        validation_errors.push(format!(
            "metadata field(s) have wrong types or unknown nested properties: {}",
            parsed.diagnostics.invalid_fields.join(", ")
        ));
    }
    if parsed.diagnostics.unknown_field_count > 0 {
        validation_errors.push(format!(
            "metadata response contains {} unknown field(s)",
            parsed.diagnostics.unknown_field_count
        ));
    }

    let mut warnings = parsed
        .suggestion
        .document_date
        .as_ref()
        .map(|date| date.warnings.clone())
        .unwrap_or_default();
    if parsed.diagnostics.status == MetadataParseStatus::Omitted {
        warnings.push("metadata response omitted every requested field".to_owned());
    }

    PromptTestParsed {
        parsed: json!(parsed),
        validation_errors,
        warnings,
    }
}

async fn test_paperless(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadSettings)?;
    let result = async {
        let settings = get_runtime_settings(&state.pool).await?;
        let active_profile = settings.paperless.archive_profiles.iter().find(|profile| {
            profile.enabled
                && profile
                    .name
                    .eq_ignore_ascii_case(&settings.paperless.active_archive)
        });
        let base_url = active_profile
            .map(|profile| profile.base_url.as_str())
            .unwrap_or(&settings.paperless.base_url);
        if let Err(error) = validate_outbound_url(base_url).await {
            return Err(anyhow!("Paperless base URL rejected: {}", error.message));
        }
        let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
        client.test_connection().await
    }
    .await;
    match result {
        Ok(value) => Ok(Json(json!({ "ok": value.ok }))),
        Err(error) => Ok(Json(json!({ "ok": false, "error": error.to_string() }))),
    }
}

#[derive(Deserialize)]
struct TestProviderRequest {
    name: String,
    kind: AiProviderKind,
    base_url: String,
    model: String,
    #[serde(default)]
    tuning: ProviderTuning,
    secret_id: Option<Uuid>,
    secret: Option<String>,
}

async fn test_provider(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<TestProviderRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteSettings)?;
    require_user_session(&auth.0, "provider tests require a user session")?;
    let settings = get_runtime_settings(&state.pool).await?;
    let provider = provider_test_target(&settings, &request)?;
    let secret = provider_test_secret(&state, &settings, &provider, request.secret).await;
    let response_secret = secret.as_ref().ok().and_then(|secret| secret.clone());
    let result = async {
        if let Err(error) = validate_outbound_url(&provider.base_url).await {
            return Err(anyhow!("AI provider base URL rejected: {}", error.message));
        }
        let secret = secret?;
        test_ai_provider(&provider, secret.clone()).await
    }
    .await;
    Ok(Json(provider_test_response(
        &provider,
        result,
        response_secret.as_ref(),
    )))
}

async fn test_notification(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteSettings)?;
    require_user_session(&auth.0, "notification tests require a user session")?;
    let settings = get_runtime_settings(&state.pool).await?;
    let result = async {
        let webhook_url = notification_webhook_url(&state, &settings).await?;
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "notification.test",
                "severity": "info",
                "title": "Paperless Archivist notification test",
                "description": "Webhook delivery is configured. This payload contains no document content or secrets.",
                "metadata": {
                    "source": "settings-test"
                }
            }),
        )
        .await
    }
    .await;
    match result {
        Ok(()) => Ok(Json(json!({ "ok": true }))),
        Err(error) => Ok(Json(json!({ "ok": false, "error": error.to_string() }))),
    }
}

async fn notification_webhook_url(state: &AppState, settings: &RuntimeSettings) -> Result<String> {
    let secret_id = settings
        .notifications
        .webhook_url_secret_id
        .ok_or_else(|| anyhow!("Notification webhook URL is not configured"))?;
    let webhook_url = resolve_secret(&state.pool, &state.config.secret_key, secret_id)
        .await?
        .ok_or_else(|| anyhow!("Notification webhook secret reference does not exist"))?;
    validate_outbound_url(webhook_url.expose_secret())
        .await
        .map_err(|error| anyhow!(error.message))?;
    Ok(webhook_url.expose_secret().to_owned())
}

/// Parse a URL provided by an administrator and reject targets that would
/// allow Server-Side Request Forgery (SSRF) against the host network. The
/// caller is expected to use this on every outbound "tester" endpoint where
/// an admin can supply an arbitrary URL, for an early, friendly rejection.
///
/// This is an up-front DNS-time check. There is intentionally **no**
/// connection-time IP-pinning resolver: a custom `reqwest` DNS resolver was
/// trialled to close the DNS-rebinding TOCTOU but it replaced reqwest's
/// happy-eyeballs behaviour and caused a worker-only connectivity regression on
/// a dual-stack host (host resolved A+AAAA; curl succeeded, the worker got
/// spurious 404s), so it was reverted (v1.8.1) and removed. See #183.
///
/// Accepted residual risk: the DNS-rebinding TOCTOU between this check and the
/// actual request is **not** closed. It is acceptable here because every
/// outbound target is operator-configured (the Paperless base URL, the LLM
/// provider URLs, and the notification webhook) rather than user-supplied per
/// request, so exploiting the window requires an attacker who already controls
/// DNS for an admin-configured host. The remaining controls — this validation
/// plus `redirect::Policy::none()` on the outbound clients — cover the
/// practical vectors. Redirects are refused so a 3xx to an internal address
/// (e.g. IMDS / loopback) cannot bypass the validated origin.
///
/// Rejections:
///  * non-http/https schemes
///  * URLs containing `user:pass@` userinfo
///  * URLs whose host resolves (DNS) to a loopback, link-local, unspecified,
///    broadcast, or multicast address
///
/// Returns the parsed `Url` on success.
async fn validate_outbound_url(raw: &str) -> Result<Url, ApiError> {
    validate_outbound_url_with(raw, DnsFailure::Reject).await
}

/// Settings-save variant: scheme/userinfo/dangerous-IP rules are identical,
/// but an unresolvable hostname passes. A name that does not resolve is not
/// an SSRF target, and a transient DNS outage (or pre-configuring a host that
/// only resolves later/inside the cluster) must not block unrelated settings
/// changes. The strict variant stays on the tester endpoints, where "does it
/// resolve" is exactly the feedback the operator asked for.
async fn validate_outbound_url_for_save(raw: &str) -> Result<Url, ApiError> {
    validate_outbound_url_with(raw, DnsFailure::Allow).await
}

#[derive(Clone, Copy, PartialEq)]
enum DnsFailure {
    Reject,
    Allow,
}

async fn validate_outbound_url_with(raw: &str, dns_failure: DnsFailure) -> Result<Url, ApiError> {
    let parsed = Url::parse(raw.trim()).map_err(|_| ApiError::bad_request("invalid URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("URL scheme must be http or https"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ApiError::bad_request("URL must not contain userinfo"));
    }
    let host = parsed
        .host()
        .ok_or_else(|| ApiError::bad_request("URL is missing a host"))?;
    let port = parsed.port_or_known_default().unwrap_or(0);
    let ips: Vec<IpAddr> = match host {
        // IP literals don't go through DNS — using `lookup_host` for them
        // is both wasteful and can fail on some platforms for ULA / RFC4193
        // addresses (macOS getaddrinfo with brackets in the host string).
        url::Host::Ipv4(v4) => vec![IpAddr::V4(v4)],
        url::Host::Ipv6(v6) => vec![IpAddr::V6(v6)],
        url::Host::Domain(domain) => match tokio::net::lookup_host((domain, port)).await {
            Ok(addresses) => addresses.map(|addr| addr.ip()).collect(),
            Err(error) if dns_failure == DnsFailure::Allow => {
                tracing::debug!(
                    domain,
                    %error,
                    "skipping SSRF IP check: host does not resolve from here"
                );
                Vec::new()
            }
            Err(error) => {
                return Err(ApiError::bad_request(format!(
                    "failed to resolve host: {error}"
                )));
            }
        },
    };
    if ips.is_empty() && dns_failure == DnsFailure::Reject {
        return Err(ApiError::bad_request("host did not resolve to any address"));
    }
    for ip in &ips {
        if archivist_core::ssrf::is_ssrf_dangerous_ip(*ip) {
            return Err(ApiError::bad_request(
                "URL resolves to a loopback, link-local, or otherwise unroutable address",
            ));
        }
    }
    Ok(parsed)
}

async fn send_notification_webhook(webhook_url: &str, payload: Value) -> Result<()> {
    let response = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(10))
        // Refuse redirects so a 3xx to an internal address (e.g. IMDS at
        // 169.254.169.254 / loopback) can't bypass the SSRF guard that only
        // validated the originally supplied URL.
        .redirect(reqwest::redirect::Policy::none())
        // NB: the caller is expected to have run `validate_outbound_url` first.
        // There is no connect-time IP-pinning (see that fn's docs / #183 for
        // why the resolver was reverted); the DNS-rebinding TOCTOU is an
        // accepted residual risk for these operator-configured targets.
        .build()?
        .post(webhook_url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "Notification webhook request failed: {}",
                error.without_url()
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("Notification webhook returned {status}"));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct OllamaInstalledModelsResponse {
    provider: String,
    models: Vec<OllamaInstalledModel>,
}

#[derive(Debug, Serialize)]
struct OllamaInstalledModel {
    name: String,
    parameter_size: Option<String>,
    quantization_level: Option<String>,
    size_bytes: Option<u64>,
    size_gb: Option<f64>,
    modified_at: Option<String>,
    digest: Option<String>,
}

async fn model_provider_models(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(name): Path<String>,
) -> ApiResult<Json<OllamaInstalledModelsResponse>> {
    require(&auth.0, Permission::ReadSettings)?;
    require_user_session(&auth.0, "model discovery requires a user session")?;
    let settings = get_runtime_settings(&state.pool).await?;
    let provider = provider_by_name(&settings, &name)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    validate_outbound_url(&provider.base_url)
        .await
        .map_err(|error| {
            ApiError::bad_request(format!("provider base URL rejected: {}", error.message))
        })?;
    let secret = provider_secret(&state, &provider).await?;
    let models = discover_provider_models(&provider, secret).await?;
    Ok(Json(OllamaInstalledModelsResponse {
        provider: provider.name,
        models,
    }))
}

/// True for an Ollama provider that points at the hosted cloud (ollama.com),
/// whose model catalog is exposed through the OpenAI-compatible
/// `/v1/models` endpoint rather than the local-runner `/api/tags`.
fn is_ollama_cloud(base_url: &str) -> bool {
    base_url.to_ascii_lowercase().contains("ollama.com")
}

/// Heuristic filter for OpenAI's noisy `/models` list: keep the chat/vision
/// families (gpt-*, chatgpt-*, o-series) and drop embeddings, audio, image,
/// moderation, and search models that can't drive the metadata/OCR stages.
fn openai_id_is_chat_capable(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let keep = id.starts_with("gpt-")
        || id.starts_with("chatgpt")
        || id.starts_with("o1")
        || id.starts_with("o3")
        || id.starts_with("o4");
    let drop = id.contains("embedding")
        || id.contains("whisper")
        || id.contains("tts")
        || id.contains("audio")
        || id.contains("realtime")
        || id.contains("transcribe")
        || id.contains("image")
        || id.contains("dall-e")
        || id.contains("moderation")
        || id.contains("search");
    keep && !drop
}

/// Runs a provider model-listing future under a short timeout so the
/// interactive "sync" button never hangs the settings page.
async fn list_with_timeout<F>(fut: F) -> ApiResult<Vec<String>>
where
    F: std::future::Future<Output = anyhow::Result<Vec<String>>>,
{
    let ids = tokio::time::timeout(std::time::Duration::from_secs(12), fut)
        .await
        .map_err(|_| ApiError::bad_request("model discovery timed out"))??;
    Ok(ids)
}

/// Discovers the available models for any provider kind. Ollama local uses
/// `/api/tags`; Ollama Cloud, OpenAI, OpenAI-compatible and Anthropic all use
/// their `/v1/models`-style listing (Anthropic via its Models API). The result
/// is normalised to the `OllamaInstalledModel` shape — remote providers fill
/// only `name`, since their listings carry no size/quant metadata.
async fn discover_provider_models(
    provider: &ApiProvider,
    secret: Option<SecretString>,
) -> ApiResult<Vec<OllamaInstalledModel>> {
    let base = provider.base_url.trim_end_matches('/');
    match provider.kind {
        AiProviderKind::Ollama if !is_ollama_cloud(base) => {
            let client = OllamaClient::new_with_timeout(
                &provider.name,
                base,
                secret,
                std::time::Duration::from_secs(12),
            )?;
            let models = client.list_models().await.map_err(|error| {
                ApiError::internal(format!("Ollama model discovery failed: {error}"))
            })?;
            Ok(models.into_iter().map(OllamaInstalledModel::from).collect())
        }
        AiProviderKind::Ollama => {
            // Ollama Cloud: the catalog lives at ollama.com/v1/models.
            let client =
                OpenAiCompatibleClient::new(&provider.name, &format!("{base}/v1"), secret)?;
            let ids = list_with_timeout(client.list_models()).await?;
            Ok(ids.into_iter().map(OllamaInstalledModel::from_id).collect())
        }
        AiProviderKind::Openai => {
            if secret.is_none() {
                return Err(ApiError::bad_request(
                    "OpenAI model discovery requires an API key — enter and save the provider's API key first.",
                ));
            }
            let client = OpenAiCompatibleClient::new(&provider.name, base, secret)?;
            let ids = list_with_timeout(client.list_models()).await?;
            Ok(ids
                .into_iter()
                .filter(|id| openai_id_is_chat_capable(id))
                .map(OllamaInstalledModel::from_id)
                .collect())
        }
        AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new(&provider.name, base, secret)?;
            let ids = list_with_timeout(client.list_models()).await?;
            Ok(ids.into_iter().map(OllamaInstalledModel::from_id).collect())
        }
        AiProviderKind::Anthropic => {
            let key = secret.ok_or_else(|| {
                ApiError::bad_request("Anthropic model discovery requires an API key")
            })?;
            let client = AnthropicClient::new(&provider.name, base, key)?;
            let ids = list_with_timeout(client.list_models()).await?;
            Ok(ids.into_iter().map(OllamaInstalledModel::from_id).collect())
        }
        AiProviderKind::Mineru => {
            // MinerU serves one implicit model; there is no /models endpoint.
            Ok(vec![OllamaInstalledModel::from_id("mineru".to_owned())])
        }
    }
}

impl OllamaInstalledModel {
    /// Builds an entry from a bare model id (remote providers' listings carry
    /// no size/quantisation metadata).
    fn from_id(id: String) -> Self {
        Self {
            name: id,
            parameter_size: None,
            quantization_level: None,
            size_bytes: None,
            size_gb: None,
            modified_at: None,
            digest: None,
        }
    }
}

impl From<OllamaModel> for OllamaInstalledModel {
    fn from(model: OllamaModel) -> Self {
        let details = model.details;
        let size_gb = model.size.map(|size| size as f64 / 1024_f64.powi(3));
        Self {
            name: model.name,
            parameter_size: details
                .as_ref()
                .and_then(|details| details.parameter_size.clone()),
            quantization_level: details
                .as_ref()
                .and_then(|details| details.quantization_level.clone()),
            size_bytes: model.size,
            size_gb,
            modified_at: model.modified_at,
            digest: model.digest,
        }
    }
}

// ----- /api/ai/runtime-hints --------------------------------------------
//
// v1.6.2 issue #127: live runtime hints for the active (or queried) AI
// provider. For Ollama this hits `/api/version` and `/api/ps` and exposes
// the loaded-model VRAM footprint plus a hint about the env-only knobs
// (NUM_PARALLEL, MAX_LOADED_MODELS, KEEP_ALIVE) that Ollama doesn't surface
// in its HTTP API. For non-Ollama providers we return a stub so the
// frontend can render a uniform card.

#[derive(Debug, Deserialize)]
struct AiRuntimeHintsQuery {
    /// Optional explicit provider name. Defaults to
    /// `ai.default_provider` when omitted.
    provider: Option<String>,
}

#[derive(Debug, Serialize)]
struct AiRuntimeHintsResponse {
    provider: String,
    reachable: bool,
    version: Option<String>,
    loaded_models: Vec<AiLoadedModelResponse>,
    /// Ollama-deploy-time-only knobs (env vars on the Ollama pod). Always
    /// `None` — the `hint` field explains where to set them.
    num_parallel: Option<i64>,
    max_loaded_models: Option<i64>,
    keep_alive: Option<String>,
    hint: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct AiLoadedModelResponse {
    name: String,
    size_vram_bytes: Option<u64>,
    /// Optional last-used timestamp; pass-through of Ollama's `expires_at`.
    /// Always serialized so the frontend can show it when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_at: Option<String>,
}

const OLLAMA_RUNTIME_HINT: &str = "NUM_PARALLEL, MAX_LOADED_MODELS, KEEP_ALIVE are set on the Ollama deployment, not in Archivist. Edit the Ollama k8s manifest to change them.";

async fn ai_runtime_hints(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(query): Query<AiRuntimeHintsQuery>,
) -> ApiResult<Json<AiRuntimeHintsResponse>> {
    require(&auth.0, Permission::ReadSettings)?;
    let settings = get_runtime_settings(&state.pool).await?;
    // Pick the requested provider, falling back to the active default.
    let provider_name = query
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(settings.ai.default_provider.as_str())
        .to_owned();
    let provider = match provider_by_name(&settings, &provider_name) {
        Ok(provider) => provider,
        Err(error) => {
            // Unknown provider name → return the stub shape rather than 4xx
            // so the frontend can render an error card consistently.
            return Ok(Json(AiRuntimeHintsResponse {
                provider: provider_name,
                reachable: false,
                version: None,
                loaded_models: Vec::new(),
                num_parallel: None,
                max_loaded_models: None,
                keep_alive: None,
                hint: Some(error.to_string()),
            }));
        }
    };
    let response = match provider.kind {
        AiProviderKind::Ollama => fetch_ollama_runtime_hints(&state, &provider).await,
        _ => non_ollama_runtime_hints(&provider),
    };
    Ok(Json(response))
}

async fn fetch_ollama_runtime_hints(
    state: &AppState,
    provider: &ApiProvider,
) -> AiRuntimeHintsResponse {
    let secret = match provider_secret(state, provider).await {
        Ok(secret) => secret,
        Err(error) => {
            return AiRuntimeHintsResponse {
                provider: provider.name.clone(),
                reachable: false,
                version: None,
                loaded_models: Vec::new(),
                num_parallel: None,
                max_loaded_models: None,
                keep_alive: None,
                hint: Some(format!("Ollama secret resolution failed: {error}")),
            };
        }
    };
    if let Err(error) = validate_outbound_url(&provider.base_url).await {
        return AiRuntimeHintsResponse {
            provider: provider.name.clone(),
            reachable: false,
            version: None,
            loaded_models: Vec::new(),
            num_parallel: None,
            max_loaded_models: None,
            keep_alive: None,
            hint: Some(format!("provider base URL rejected: {}", error.message)),
        };
    }
    let client = match OllamaClient::new_with_timeout(
        &provider.name,
        &provider.base_url,
        secret,
        std::time::Duration::from_secs(5),
    ) {
        Ok(client) => client,
        Err(error) => {
            return AiRuntimeHintsResponse {
                provider: provider.name.clone(),
                reachable: false,
                version: None,
                loaded_models: Vec::new(),
                num_parallel: None,
                max_loaded_models: None,
                keep_alive: None,
                hint: Some(format!("failed to build Ollama client: {error}")),
            };
        }
    };
    fetch_ollama_runtime_hints_with_client(&provider.name, &client).await
}

/// Inner Ollama probe split out for testability. Takes an `OllamaClient`
/// already wired to the runtime URL, hits `/api/version` and `/api/ps`,
/// composes the response. Independent of `AppState` so unit tests can
/// point a real `OllamaClient` at a mock HTTP server.
async fn fetch_ollama_runtime_hints_with_client(
    provider_name: &str,
    client: &OllamaClient,
) -> AiRuntimeHintsResponse {
    let mut response = AiRuntimeHintsResponse {
        provider: provider_name.to_owned(),
        reachable: false,
        version: None,
        loaded_models: Vec::new(),
        num_parallel: None,
        max_loaded_models: None,
        keep_alive: None,
        hint: Some(OLLAMA_RUNTIME_HINT.to_owned()),
    };
    // `/api/version` first — gates `reachable`. If even the version probe
    // fails we surface the error in `hint` and skip the loaded-models call.
    match client.version().await {
        Ok(version) => {
            response.reachable = true;
            response.version = Some(version);
        }
        Err(error) => {
            response.reachable = false;
            response.hint = Some(format!("Ollama unreachable: {error}"));
            return response;
        }
    }
    match client.loaded_models().await {
        Ok(models) => {
            response.loaded_models = models
                .into_iter()
                .map(AiLoadedModelResponse::from)
                .collect();
        }
        Err(error) => {
            // Reachable, but /api/ps failed — keep `reachable: true`, surface
            // the partial failure in the hint so the UI can still show the
            // version while explaining why loaded-models is blank.
            response.hint = Some(format!("Ollama /api/ps failed: {error}"));
        }
    }
    response
}

fn non_ollama_runtime_hints(provider: &ApiProvider) -> AiRuntimeHintsResponse {
    let kind = match provider.kind {
        AiProviderKind::Openai => "openai",
        AiProviderKind::Anthropic => "anthropic",
        AiProviderKind::OpenaiCompatible => "openai_compatible",
        AiProviderKind::Ollama => "ollama",
        AiProviderKind::Mineru => "mineru",
    };
    AiRuntimeHintsResponse {
        provider: provider.name.clone(),
        reachable: true,
        version: None,
        loaded_models: Vec::new(),
        num_parallel: None,
        max_loaded_models: None,
        keep_alive: None,
        hint: Some(format!(
            "{kind}-specific tuning is not server-side observable from Archivist."
        )),
    }
}

impl From<OllamaLoadedModel> for AiLoadedModelResponse {
    fn from(model: OllamaLoadedModel) -> Self {
        Self {
            name: model.name,
            size_vram_bytes: model.size_vram,
            last_used_at: model.expires_at,
        }
    }
}

#[derive(Debug, Clone)]
struct ApiProvider {
    name: String,
    kind: AiProviderKind,
    base_url: String,
    model: String,
    secret_id: Option<Uuid>,
    tuning: EffectiveTuning,
}

fn provider_test_target(
    settings: &RuntimeSettings,
    request: &TestProviderRequest,
) -> Result<ApiProvider, ApiError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("provider name must not be blank"));
    }
    let model = request.model.trim();
    if model.is_empty() && !matches!(request.kind, AiProviderKind::Mineru) {
        return Err(ApiError::bad_request("provider model must not be blank"));
    }
    let model = if model.is_empty() { "mineru" } else { model };
    let base_url = provider_base_url(name, &request.base_url).map_err(|error| {
        ApiError::bad_request(format!("AI provider '{name}' base URL: {error}"))
    })?;
    let draft = AiProviderSettings {
        name: name.to_owned(),
        kind: request.kind.clone(),
        base_url: base_url.clone(),
        default_text_model: Some(model.to_owned()),
        default_vision_model: None,
        cost_per_1m_input_tokens_usd: None,
        cost_per_1m_output_tokens_usd: None,
        secret_id: request.secret_id,
        enabled: true,
        tuning: request.tuning.clone(),
    };
    let mut effective_settings = settings.clone();
    effective_settings.ai.default_provider = draft.name.clone();
    if let Some(index) = effective_settings
        .ai
        .providers
        .iter()
        .position(|provider| provider.name == draft.name)
    {
        effective_settings.ai.providers[index] = draft.clone();
    } else {
        effective_settings.ai.providers.push(draft.clone());
    }
    let tuning = effective_settings.effective_tuning_for_provider(&draft);
    Ok(ApiProvider {
        name: draft.name,
        kind: draft.kind,
        base_url,
        model: model.to_owned(),
        secret_id: draft.secret_id,
        tuning,
    })
}

async fn provider_test_secret(
    state: &AppState,
    settings: &RuntimeSettings,
    provider: &ApiProvider,
    transient: Option<String>,
) -> Result<Option<SecretString>> {
    if let Some(secret) = transient.filter(|secret| !secret.trim().is_empty()) {
        return Ok(Some(SecretString::from(secret)));
    }
    if let Some(secret_id) = provider.secret_id
        && !settings
            .ai
            .providers
            .iter()
            .any(|saved| saved.secret_id == Some(secret_id))
    {
        return Err(anyhow!(
            "provider secret reference is not assigned to a saved AI provider"
        ));
    }
    provider_secret(state, provider).await
}

fn provider_test_response(
    provider: &ApiProvider,
    result: Result<Value>,
    secret: Option<&SecretString>,
) -> Value {
    match result {
        Ok(_) => json!({
            "ok": true,
            "provider": provider.name,
            "model": provider.model,
        }),
        Err(error) => {
            let mut message = error.to_string();
            if let Some(secret) = secret {
                let exposed = secret.expose_secret();
                if !exposed.is_empty() {
                    message = message.replace(exposed, "[REDACTED]");
                }
            }
            json!({
                "ok": false,
                "provider": provider.name,
                "model": provider.model,
                "error": message,
            })
        }
    }
}

fn provider_by_name(settings: &RuntimeSettings, name: &str) -> Result<ApiProvider> {
    let mut provider = settings
        .ai
        .providers
        .iter()
        .find(|provider| provider.name.eq_ignore_ascii_case(name))
        .cloned()
        .or_else(|| {
            if name.eq_ignore_ascii_case("ollama") {
                Some(archivist_core::AiProviderSettings::ollama_default())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("AI provider '{name}' is not configured"))?;
    if provider.name.eq_ignore_ascii_case("ollama") {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings.ai.default_model_for_provider(&provider, false);
    let base_url = provider_base_url(&provider.name, &provider.base_url)?;
    let tuning = settings.effective_tuning_for_provider(&provider);
    Ok(ApiProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
        tuning,
    })
}

fn provider_for_default_text(settings: &RuntimeSettings) -> Result<ApiProvider> {
    let mut provider = settings
        .ai
        .providers
        .iter()
        .find(|provider| {
            provider.enabled
                && provider
                    .name
                    .eq_ignore_ascii_case(&settings.ai.default_provider)
        })
        .cloned()
        .or_else(|| {
            if settings.ai.default_provider.eq_ignore_ascii_case("ollama") {
                Some(archivist_core::AiProviderSettings::ollama_default())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            anyhow!(
                "AI provider '{}' is not configured or disabled",
                settings.ai.default_provider
            )
        })?;
    if provider.name.eq_ignore_ascii_case("ollama") {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings.ai.default_model_for_provider(&provider, false);
    let base_url = provider_base_url(&provider.name, &provider.base_url)?;
    let tuning = settings.effective_tuning_for_provider(&provider);
    Ok(ApiProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
        tuning,
    })
}

fn provider_for_stage_text(settings: &RuntimeSettings, stage: Stage) -> Result<ApiProvider> {
    let stage_override = settings
        .ai
        .stage_models
        .iter()
        .find(|override_model| override_model.stage == stage);
    let provider_name = stage_override
        .map(|override_model| override_model.provider.as_str())
        .unwrap_or(&settings.ai.default_provider);
    let mut provider = settings
        .ai
        .providers
        .iter()
        .find(|provider| provider.enabled && provider.name.eq_ignore_ascii_case(provider_name))
        .cloned()
        .or_else(|| {
            if provider_name.eq_ignore_ascii_case("ollama") {
                Some(archivist_core::AiProviderSettings::ollama_default())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("AI provider '{provider_name}' is not configured or disabled"))?;
    if provider.name.eq_ignore_ascii_case("ollama") {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings
        .ai
        .model_for_stage_provider(&provider, stage, false);
    let base_url = provider_base_url(&provider.name, &provider.base_url)?;
    let tuning = settings.effective_tuning_for_provider(&provider);
    Ok(ApiProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
        tuning,
    })
}

fn provider_base_url(provider_name: &str, configured: &str) -> Result<String> {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "AI provider '{provider_name}' has an empty base URL; repair the runtime settings"
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

async fn provider_secret(state: &AppState, provider: &ApiProvider) -> Result<Option<SecretString>> {
    let Some(secret_id) = provider.secret_id else {
        return Ok(None);
    };
    resolve_secret(&state.pool, &state.config.secret_key, secret_id).await
}

fn apply_api_provider_tuning(provider: &ApiProvider, request: &mut ChatRequest) {
    request.num_ctx = provider.tuning.text_num_ctx;
    request.reasoning_effort = Some(provider.tuning.reasoning_effort);
    request.max_output_tokens = provider.tuning.max_output_tokens;
    request.structured_output = Some(provider.tuning.structured_output);
}

fn provider_test_chat_request(provider: &ApiProvider) -> ChatRequest {
    let mut request = ChatRequest {
        model: provider.model.clone(),
        system_prompt: "Return only a short JSON provider health result.".to_owned(),
        user_prompt: "Return {\"status\":\"ok\"}.".to_owned(),
        temperature: 0.0,
        num_ctx: None,
        response_schema: Some(json!({
            "type": "object",
            "properties": { "status": { "type": "string" } },
            "required": ["status"],
            "additionalProperties": false
        })),
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    };
    apply_api_provider_tuning(provider, &mut request);
    request
}

async fn test_ai_provider(provider: &ApiProvider, secret: Option<SecretString>) -> Result<Value> {
    let timeout =
        std::time::Duration::from_secs(u64::from(provider.tuning.request_timeout_seconds));
    match provider.kind {
        AiProviderKind::Ollama => {
            let client = OllamaClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            let response = client.chat(provider_test_chat_request(provider)).await?;
            Ok(json!({
                "provider": response.provider,
                "model": response.model,
                "text": response.text,
            }))
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            let response = client.chat(provider_test_chat_request(provider)).await?;
            Ok(
                json!({ "provider": response.provider, "model": response.model, "text": response.text }),
            )
        }
        AiProviderKind::Anthropic => {
            let secret = secret.ok_or_else(|| {
                anyhow!("AI provider '{}' requires an API key secret", provider.name)
            })?;
            let client = AnthropicClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            let response = client.chat(provider_test_chat_request(provider)).await?;
            Ok(
                json!({ "provider": response.provider, "model": response.model, "text": response.text }),
            )
        }
        AiProviderKind::Mineru => {
            let client = MineruClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            client.test_connection().await
        }
    }
}

async fn chat_with_api_provider(
    state: &AppState,
    provider: &ApiProvider,
    request: ChatRequest,
) -> Result<AiResponse> {
    let timeout =
        std::time::Duration::from_secs(u64::from(provider.tuning.request_timeout_seconds));
    match provider.kind {
        AiProviderKind::Ollama => {
            let client = OllamaClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(state, provider).await?,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(state, provider).await?,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Anthropic => {
            let secret = provider_secret(state, provider).await?.ok_or_else(|| {
                anyhow!("AI provider '{}' requires an API key secret", provider.name)
            })?;
            let client = AnthropicClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                secret,
                timeout,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Mineru => Err(anyhow!(
            "AI provider '{}' uses kind \"mineru\" which is vision-only (OCR); \
             select a text-capable provider for this stage",
            provider.name
        )),
    }
}

fn build_document_chat_request(
    provider: &ApiProvider,
    system_prompt: String,
    user_prompt: String,
) -> ChatRequest {
    let mut request = ChatRequest {
        model: provider.model.clone(),
        system_prompt,
        user_prompt,
        temperature: 0.1,
        num_ctx: None,
        response_schema: None,
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    };
    apply_api_provider_tuning(provider, &mut request);
    request
}

async fn ensure_chat_visible(
    pool: &DbPool,
    session_id: Uuid,
    user_id: Option<Uuid>,
    include_all: bool,
) -> ApiResult<()> {
    if document_chat_session_visible(pool, session_id, user_id, include_all).await? {
        Ok(())
    } else {
        Err(ApiError::forbidden("chat session is not available"))
    }
}

fn chat_title(title: &str) -> String {
    let mut title = title.trim().replace(char::is_whitespace, " ");
    while title.contains("  ") {
        title = title.replace("  ", " ");
    }
    if title.chars().count() > 80 {
        title = title.chars().take(77).collect::<String>();
        title.push_str("...");
    }
    if title.is_empty() {
        "New document chat".to_owned()
    } else {
        title
    }
}

async fn retrieve_document_chat_sources(
    state: &AppState,
    settings: &RuntimeSettings,
    question: &str,
    document_ids: Option<&[i32]>,
    max_sources: usize,
) -> Result<Vec<DocumentChatSource>> {
    let max_sources = max_sources.clamp(1, 10);
    let candidates = search_document_chat_candidates(
        &state.pool,
        question,
        document_ids,
        (max_sources as i64 * 5).max(20),
    )
    .await?;
    let paperless = paperless_client_from_settings(&state.pool, &state.config, settings).await?;
    let terms = document_chat_terms(question);
    let mut sources = Vec::new();

    for candidate in candidates {
        match paperless
            .get_document(candidate.paperless_document_id)
            .await
        {
            Ok(document) => {
                let content = document.content.unwrap_or_default();
                let metadata = chat_candidate_metadata(&candidate);
                let combined = if content.trim().is_empty() {
                    metadata
                } else {
                    format!("{metadata}\n\n{content}")
                };
                let score = score_document_chat_source(&terms, candidate.metadata_score, &combined);
                let snippet = document_chat_snippet(&combined, &terms, 1800);
                if snippet.is_empty() {
                    continue;
                }
                sources.push(DocumentChatSource {
                    paperless_document_id: candidate.paperless_document_id,
                    title: document.title.or(candidate.title),
                    snippet,
                    score,
                    source_kind: "paperless_content".to_owned(),
                });
            }
            Err(error) => {
                warn!(
                    document_id = candidate.paperless_document_id,
                    error = %error,
                    "skipping document chat source because Paperless document fetch failed"
                );
            }
        }
    }

    sources.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sources.truncate(max_sources);
    Ok(sources)
}

fn chat_candidate_metadata(candidate: &DocumentChatCandidate) -> String {
    let mut parts = vec![format!("Document ID: {}", candidate.paperless_document_id)];
    if let Some(title) = candidate.title.as_deref().filter(|title| !title.is_empty()) {
        parts.push(format!("Title: {title}"));
    }
    if let Some(file_name) = candidate
        .original_file_name
        .as_deref()
        .filter(|file_name| !file_name.is_empty())
    {
        parts.push(format!("Original file: {file_name}"));
    }
    if !candidate.current_tags.is_empty() {
        parts.push(format!("Tags: {}", candidate.current_tags.join(", ")));
    }
    parts.join("\n")
}

#[tracing::instrument(
    skip(state, auth),
    fields(user_id = tracing::field::Empty)
)]
async fn sync_paperless(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    let summary = sync_paperless_inventory(&state.pool, &client, &settings).await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "paperless.sync".to_owned(),
            actor_type: auth.0.actor_type,
            actor_id: auth.0.actor_id,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(summary.clone()),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    info!(?summary, "paperless sync completed");
    Ok(Json(summary))
}

#[tracing::instrument(
    skip(state, auth),
    fields(user_id = tracing::field::Empty, documents_checked = tracing::field::Empty)
)]
async fn paperless_consistency(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadInventory)?;
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    let documents = client.list_documents().await?;
    let rows = sqlx::query(
        r#"
        select paperless_document_id, title, current_tag_ids, correspondent_id,
               document_type_id, document_date
          from document_inventory
        "#,
    )
    .fetch_all(&state.pool)
    .await?;
    let mut inventory = HashMap::new();
    for row in rows {
        inventory.insert(
            row.try_get::<i32, _>("paperless_document_id")?,
            json!({
                "title": row.try_get::<Option<String>, _>("title")?,
                "current_tag_ids": row.try_get::<Vec<i32>, _>("current_tag_ids")?,
                "correspondent": row.try_get::<Option<i32>, _>("correspondent_id")?,
                "document_type": row.try_get::<Option<i32>, _>("document_type_id")?,
                "created": row.try_get::<Option<String>, _>("document_date")?
            }),
        );
    }

    let mut missing_local = Vec::new();
    let mut mismatches = Vec::new();
    let seen_remote = documents
        .iter()
        .map(|document| document.id)
        .collect::<HashSet<_>>();
    for document in &documents {
        let Some(local) = inventory.get(&document.id) else {
            missing_local.push(document.id);
            continue;
        };
        let mut fields = Vec::new();
        if local.get("title").and_then(Value::as_str) != document.title.as_deref() {
            fields.push("title");
        }
        if local
            .get("correspondent")
            .and_then(Value::as_i64)
            .map(|value| value as i32)
            != document.correspondent
        {
            fields.push("correspondent");
        }
        if local
            .get("document_type")
            .and_then(Value::as_i64)
            .map(|value| value as i32)
            != document.document_type
        {
            fields.push("document_type");
        }
        if local.get("created").and_then(Value::as_str) != document.created.as_deref() {
            fields.push("document_date");
        }
        let mut local_tags = local
            .get("current_tag_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_i64().map(|value| value as i32))
            .collect::<Vec<_>>();
        let mut remote_tags = document.tags.clone();
        local_tags.sort_unstable();
        remote_tags.sort_unstable();
        if local_tags != remote_tags {
            fields.push("tags");
        }
        if !fields.is_empty() {
            mismatches.push(json!({ "paperless_document_id": document.id, "fields": fields }));
        }
    }
    let stale_local = inventory
        .keys()
        .filter(|id| !seen_remote.contains(id))
        .copied()
        .collect::<Vec<_>>();

    let documents_checked = documents.len();
    Span::current().record("documents_checked", documents_checked);
    info!(
        documents_checked,
        missing_local = missing_local.len(),
        stale_local = stale_local.len(),
        mismatches = mismatches.len(),
        "paperless consistency check completed"
    );
    Ok(Json(json!({
        "documents_checked": documents_checked,
        "missing_local": missing_local,
        "stale_local": stale_local,
        "mismatches": mismatches,
        "ok": missing_local.is_empty() && stale_local.is_empty() && mismatches.is_empty()
    })))
}

#[derive(Debug, Default, Deserialize)]
struct ReconcileCompletionTagsRequest {
    dry_run: Option<bool>,
    document_ids: Option<Vec<i32>>,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, dry_run = tracing::field::Empty)
)]
async fn reconcile_completion_tags(
    State(state): State<AppState>,
    auth: Authenticated,
    request: Option<Json<ReconcileCompletionTagsRequest>>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let request = request.map(|Json(request)| request).unwrap_or_default();
    let settings = get_runtime_settings(&state.pool).await?;
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    let dry_run = request.dry_run.unwrap_or(true);
    Span::current().record("dry_run", dry_run);
    let mut tags = client.list_tags().await?;
    let mut full_tag: Option<PaperlessTag> = None;
    let stage_completion_tags = settings
        .workflow
        .enabled_stages
        .iter()
        .filter_map(|stage| settings.workflow.tags.completion_tag_for_stage(*stage))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let inventory_complete_ids = archivist_db::completed_document_ids_missing_full_tag(
        &state.pool,
        &settings.workflow.enabled_stages,
    )
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let documents = client.list_documents().await?;
    let allowed_ids = request
        .document_ids
        .map(|ids| ids.into_iter().collect::<HashSet<_>>());
    let mut planned = Vec::new();
    let mut applied = Vec::new();
    for document in documents {
        if let Some(allowed_ids) = &allowed_ids
            && !allowed_ids.contains(&document.id)
        {
            continue;
        }
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tags.iter().find(|tag| tag.id == *id))
            .map(|tag| tag.name.clone())
            .collect::<Vec<_>>();
        let stage_tags_complete = !stage_completion_tags.is_empty()
            && stage_completion_tags
                .iter()
                .all(|tag| tag_names.iter().any(|name| name.eq_ignore_ascii_case(tag)));
        let inventory_stages_complete = inventory_complete_ids.contains(&document.id);
        if completion_tag_reconcile_needed(
            &tag_names,
            &stage_completion_tags,
            &settings.workflow.tags.completion_processed,
            inventory_stages_complete,
        ) {
            let status_guard = if !dry_run && inventory_stages_complete && !stage_tags_complete {
                let Some(guard) = archivist_db::begin_completion_tag_reconcile_guard(
                    &state.pool,
                    document.id,
                    &settings.workflow.enabled_stages,
                )
                .await?
                else {
                    continue;
                };
                Some(guard)
            } else {
                None
            };
            planned.push(json!({ "paperless_document_id": document.id, "add": [settings.workflow.tags.completion_processed.clone()] }));
            if !dry_run {
                let tag = match &full_tag {
                    Some(tag) => tag.clone(),
                    None => {
                        let tag = ensure_workflow_tag_cached(
                            &client,
                            &mut tags,
                            &settings.workflow.tags.completion_processed,
                        )
                        .await?;
                        full_tag = Some(tag.clone());
                        tag
                    }
                };
                client
                    .add_and_remove_tags(document.id, &[tag.id], &[])
                    .await?;
                if let Some(guard) = status_guard {
                    guard.commit().await?;
                }
                applied.push(document.id);
            }
        }
    }
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "paperless.completion_tags_reconciled".to_owned(),
            actor_type: auth.0.actor_type,
            actor_id: auth.0.actor_id,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(
                json!({ "planned": planned.len(), "applied": applied.len(), "dry_run": dry_run }),
            ),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    info!(
        dry_run,
        planned = planned.len(),
        applied = applied.len(),
        "completion tag reconciliation completed"
    );
    Ok(Json(
        json!({ "dry_run": dry_run, "planned": planned, "applied": applied }),
    ))
}

fn completion_tag_reconcile_needed(
    tag_names: &[String],
    stage_completion_tags: &[String],
    full_completion_tag: &str,
    inventory_stages_complete: bool,
) -> bool {
    !stage_completion_tags.is_empty()
        && (inventory_stages_complete
            || stage_completion_tags
                .iter()
                .all(|tag| tag_names.iter().any(|name| name.eq_ignore_ascii_case(tag))))
        && !tag_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(full_completion_tag))
}

async fn ensure_workflow_tag_cached(
    client: &PaperlessClient,
    tags: &mut Vec<PaperlessTag>,
    name: &str,
) -> Result<PaperlessTag> {
    if let Some(tag) = tags.iter().find(|tag| tag.name.eq_ignore_ascii_case(name)) {
        return Ok(tag.clone());
    }
    let tag = client.ensure_tag(name).await?;
    tags.push(tag.clone());
    Ok(tag)
}

#[derive(Debug, Deserialize)]
struct DashboardQuery {
    range: Option<String>,
}

async fn dashboard(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(query): Query<DashboardQuery>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadDashboard)?;
    let range = query
        .range
        .as_deref()
        .unwrap_or(DashboardRange::default().key())
        .parse::<DashboardRange>()
        .unwrap_or_default();
    let counts = get_backlog_counts(&state.pool).await?;
    let settings = get_runtime_settings(&state.pool).await?;
    let now = Utc::now();
    let start = dashboard_range_start(&state.pool, range, now).await?;
    let mut stats = get_dashboard_stats(&state.pool, range, &counts, now, start).await?;
    enrich_dashboard_costs(&mut stats, &settings);

    let bucket_entries = provider_bucket_entries(&state.pool, start, now, range).await?;
    let bucket_labels = dashboard_bucket_labels(start, now, range);
    enrich_provider_sparklines(&mut stats, &bucket_entries, &bucket_labels, &settings);

    Ok(Json(json!({ "counts": counts, "stats": stats })))
}

async fn dashboard_live(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadDashboard)?;
    let settings = get_runtime_settings(&state.pool).await?;
    Ok(Json(json!(
        get_dashboard_live_status(&state.pool, &settings).await?
    )))
}

#[derive(Debug, Deserialize)]
struct StatisticsQuery {
    /// RFC3339 / `YYYY-MM-DD` start (inclusive). Defaults to `to - 30 days`.
    from: Option<String>,
    /// RFC3339 / `YYYY-MM-DD` end (exclusive). A bare date means the END of
    /// that day (next UTC midnight), so the named day is fully covered (#301).
    /// Defaults to now.
    to: Option<String>,
    /// Bucket granularity: hour | day | week | month. Defaults to day.
    bucket: Option<String>,
}

/// How a bare `YYYY-MM-DD` statistics bound anchors within its day. RFC3339
/// inputs carry their own time and are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatBound {
    /// Inclusive range start: the named day's first instant (00:00:00 UTC).
    Start,
    /// Exclusive range end: the NEXT day's first instant, so the named day is
    /// fully covered. Anchoring `to` at its own midnight made the current day
    /// invisible (`to=<today>` excluded everything after 00:00) and turned
    /// `from == to` into an empty range. #301
    End,
}

fn parse_stat_datetime(raw: &str, bound: StatBound) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    // Accept a bare date (YYYY-MM-DD) interpreted as UTC midnight.
    let mut date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    if bound == StatBound::End {
        date = date.succ_opt()?;
    }
    Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?))
}

/// Resolve the statistics range from the raw query parameters. Defaults:
/// `to` = `now`, `from` = `to` - 30 days — so the default view always ends at
/// the current instant and includes today's data.
///
/// Defaults apply only when a bound is absent (or blank); a value that is
/// present but unparseable is rejected with 400 instead of being silently
/// swapped for the default, which hid typos behind a wrong-looking range. #312
fn resolve_stat_range(
    from: Option<&str>,
    to: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), ApiError> {
    let to = match to.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => now,
        Some(raw) => parse_stat_datetime(raw, StatBound::End).ok_or_else(|| {
            ApiError::bad_request("'to' must be an RFC3339 timestamp or a YYYY-MM-DD date")
        })?,
    };
    let from = match from.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => to - Duration::days(30),
        Some(raw) => parse_stat_datetime(raw, StatBound::Start).ok_or_else(|| {
            ApiError::bad_request("'from' must be an RFC3339 timestamp or a YYYY-MM-DD date")
        })?,
    };
    if from >= to {
        return Err(ApiError::bad_request("'from' must be before 'to'"));
    }
    Ok((from, to))
}

/// Hard ceiling for the zero-filled statistics axis (#312). Covers 90 days of
/// hour buckets (2160) with headroom and ~6.8 years of day buckets; past the
/// cap (e.g. hour buckets over a multi-year span) the series simply stay
/// sparse.
const MAX_STATISTICS_BUCKETS: usize = 2500;

/// Floor `ts` to the statistics bucket unit, mirroring Postgres
/// `date_trunc(unit, ts)` under a UTC session: weeks start on the ISO Monday,
/// months on the 1st.
fn statistics_bucket_floor(ts: DateTime<Utc>, bucket: &str) -> DateTime<Utc> {
    let date = ts.date_naive();
    let (date, hour) = match bucket {
        "hour" => (date, ts.hour()),
        "week" => (
            date - Duration::days(i64::from(date.weekday().num_days_from_monday())),
            0,
        ),
        "month" => (date.with_day(1).unwrap_or(date), 0),
        // "day" (the only other validated unit)
        _ => (date, 0),
    };
    date.and_hms_opt(hour, 0, 0)
        .map(|naive| Utc.from_utc_datetime(&naive))
        .unwrap_or(ts)
}

/// The start of the bucket after `cursor`: hour/day/week step by a fixed
/// span, month advances to the 1st of the next month (mirroring
/// `dashboard_bucket_labels`).
fn statistics_bucket_next(cursor: DateTime<Utc>, bucket: &str) -> Option<DateTime<Utc>> {
    match bucket {
        "hour" => Some(cursor + Duration::hours(1)),
        "week" => Some(cursor + Duration::days(7)),
        "month" => {
            let (year, month) = if cursor.month() == 12 {
                (cursor.year() + 1, 1)
            } else {
                (cursor.year(), cursor.month() + 1)
            };
            Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).single()
        }
        _ => Some(cursor + Duration::days(1)),
    }
}

/// Every bucket start covering `[from, to)`, used to zero-fill the statistics
/// time series so quiet periods chart as 0 instead of being skipped (the SQL
/// GROUP BY only yields non-empty buckets). #312
///
/// The axis never extends before the first bucket that actually holds data:
/// the "all time" preset sends a far-past sentinel `from`, and mirroring the
/// dashboard — whose "all" range starts at the earliest record — keeps that
/// meaning "the recorded span", not decades of empty buckets. Without any
/// data the requested range itself is enumerated, so the default view still
/// renders a flat zero axis. Spans past `MAX_STATISTICS_BUCKETS` return an
/// empty list and the series simply stay sparse.
fn statistics_bucket_starts(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    bucket: &str,
    earliest_data: Option<DateTime<Utc>>,
) -> Vec<DateTime<Utc>> {
    let mut cursor = statistics_bucket_floor(from, bucket);
    if let Some(earliest) = earliest_data {
        cursor = cursor.max(earliest);
    }
    let mut starts = Vec::new();
    while cursor < to {
        if starts.len() >= MAX_STATISTICS_BUCKETS {
            return Vec::new();
        }
        starts.push(cursor);
        match statistics_bucket_next(cursor, bucket) {
            Some(next) if next > cursor => cursor = next,
            _ => break,
        }
    }
    starts
}

#[derive(Default, Clone)]
struct UsageAgg {
    request_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    duration_sum: f64,
    duration_n: f64,
}

impl UsageAgg {
    fn add(&mut self, row: &archivist_db::StatisticsUsageRow) {
        self.request_count += row.request_count;
        self.input_tokens += row.input_tokens;
        self.output_tokens += row.output_tokens;
        if let Some(avg) = row.avg_duration_ms {
            // Weight the per-cell average by its request count to recover a
            // correct overall mean.
            self.duration_sum += avg * row.request_count as f64;
            self.duration_n += row.request_count as f64;
        }
    }
    fn avg_ms(&self) -> Option<f64> {
        (self.duration_n > 0.0).then(|| self.duration_sum / self.duration_n)
    }
    fn cost(&self, costs: Option<&(Option<f64>, Option<f64>)>) -> Option<f64> {
        let (Some(ci), Some(co)) = *costs? else {
            return None;
        };
        Some(
            (self.input_tokens as f64 / 1_000_000.0 * ci)
                + (self.output_tokens as f64 / 1_000_000.0 * co),
        )
    }
    fn to_json(&self, key_field: &str, key: &str, cost: Option<f64>) -> Value {
        json!({
            key_field: key,
            "request_count": self.request_count,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "avg_duration_ms": self.avg_ms(),
            "estimated_cost_usd": cost,
        })
    }
}

/// Comprehensive Statistics page data: summary + time-series + per-provider /
/// per-model / per-stage breakdowns + pipeline throughput, over a custom range.
async fn statistics(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(query): Query<StatisticsQuery>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadDashboard)?;
    let now = Utc::now();
    let (from, to) = resolve_stat_range(query.from.as_deref(), query.to.as_deref(), now)?;
    let bucket = match query.bucket.as_deref().unwrap_or("day") {
        b @ ("hour" | "day" | "week" | "month") => b.to_owned(),
        _ => {
            return Err(ApiError::bad_request(
                "bucket must be hour, day, week or month",
            ));
        }
    };

    let usage = statistics_usage_rows(&state.pool, from, to, &bucket).await?;
    let throughput = statistics_throughput_rows(&state.pool, from, to, &bucket).await?;
    let settings = get_runtime_settings(&state.pool).await?;

    // provider name -> (input cost / 1M, output cost / 1M)
    let cost_map: HashMap<String, (Option<f64>, Option<f64>)> = settings
        .ai
        .providers
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                (
                    p.cost_per_1m_input_tokens_usd,
                    p.cost_per_1m_output_tokens_usd,
                ),
            )
        })
        .collect();

    let mut total = UsageAgg::default();
    let mut by_provider: std::collections::BTreeMap<String, UsageAgg> = Default::default();
    let mut by_model: std::collections::BTreeMap<String, UsageAgg> = Default::default();
    let mut by_stage: std::collections::BTreeMap<String, UsageAgg> = Default::default();
    let mut series: std::collections::BTreeMap<DateTime<Utc>, UsageAgg> = Default::default();

    for row in &usage {
        total.add(row);
        by_provider
            .entry(row.provider.clone())
            .or_default()
            .add(row);
        by_model.entry(row.model.clone()).or_default().add(row);
        by_stage.entry(row.stage.clone()).or_default().add(row);
        series.entry(row.bucket).or_default().add(row);
    }

    // Pipeline throughput per bucket: succeeded / failed / cancelled.
    let mut throughput_series: std::collections::BTreeMap<DateTime<Utc>, (i64, i64, i64)> =
        Default::default();
    let (mut tot_ok, mut tot_fail, mut tot_cancel) = (0_i64, 0_i64, 0_i64);
    for row in &throughput {
        let entry = throughput_series.entry(row.bucket).or_default();
        match row.status.as_str() {
            "succeeded" => {
                entry.0 += row.job_count;
                tot_ok += row.job_count;
            }
            "failed" => {
                entry.1 += row.job_count;
                tot_fail += row.job_count;
            }
            _ => {
                entry.2 += row.job_count;
                tot_cancel += row.job_count;
            }
        }
    }

    // Zero-fill the shared bucket axis so both charts plot every bucket of
    // the range and quiet periods show as 0 instead of being compressed
    // away. #312
    let earliest_data = series.keys().chain(throughput_series.keys()).min().copied();
    for bucket_start in statistics_bucket_starts(from, to, &bucket, earliest_data) {
        series.entry(bucket_start).or_default();
        throughput_series.entry(bucket_start).or_default();
    }

    let total_cost: Option<f64> = {
        let mut any = false;
        let mut sum = 0.0;
        for (name, agg) in &by_provider {
            if let Some(c) = agg.cost(cost_map.get(name)) {
                any = true;
                sum += c;
            }
        }
        any.then_some(sum)
    };

    let to_series = |s: &std::collections::BTreeMap<DateTime<Utc>, UsageAgg>| -> Vec<Value> {
        s.iter()
            .map(|(bucket, agg)| {
                json!({
                    "bucket": bucket.to_rfc3339(),
                    "request_count": agg.request_count,
                    "input_tokens": agg.input_tokens,
                    "output_tokens": agg.output_tokens,
                    "avg_duration_ms": agg.avg_ms(),
                })
            })
            .collect()
    };

    Ok(Json(json!({
        "from": from.to_rfc3339(),
        "to": to.to_rfc3339(),
        "bucket": bucket,
        "summary": {
            "request_count": total.request_count,
            "input_tokens": total.input_tokens,
            "output_tokens": total.output_tokens,
            "avg_duration_ms": total.avg_ms(),
            "estimated_cost_usd": total_cost,
            "jobs_succeeded": tot_ok,
            "jobs_failed": tot_fail,
            "jobs_cancelled": tot_cancel,
        },
        "time_series": to_series(&series),
        "throughput_series": throughput_series.iter().map(|(bucket, (ok, fail, cancel))| json!({
            "bucket": bucket.to_rfc3339(),
            "succeeded": ok,
            "failed": fail,
            "cancelled": cancel,
        })).collect::<Vec<_>>(),
        "by_provider": by_provider.iter().map(|(name, agg)| {
            agg.to_json("provider", name, agg.cost(cost_map.get(name)))
        }).collect::<Vec<_>>(),
        "by_model": by_model.iter().map(|(name, agg)| agg.to_json("model", name, None)).collect::<Vec<_>>(),
        "by_stage": by_stage.iter().map(|(name, agg)| agg.to_json("stage", name, None)).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
struct UpdateWorkflowModeRequest {
    mode: ProcessingMode,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, mode = tracing::field::Empty)
)]
async fn update_workflow_mode(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<UpdateWorkflowModeRequest>,
) -> ApiResult<Json<RuntimeSettings>> {
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id = require_user_session(&auth.0, "workflow mode updates require a user session")?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    Span::current().record("mode", tracing::field::debug(request.mode));
    let mut settings = get_runtime_settings(&state.pool).await?;
    settings.workflow.mode = request.mode;
    update_runtime_settings(&state.pool, &settings, actor_id).await?;
    info!(%actor_id, mode = ?request.mode, "workflow mode updated");
    Ok(Json(settings))
}

#[derive(Debug, Deserialize)]
struct UpdateWorkflowControlsRequest {
    paused: Option<bool>,
    dry_run: Option<bool>,
    hourly_document_limit: Option<Option<i64>>,
    daily_document_limit: Option<Option<i64>>,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty)
)]
async fn update_workflow_controls(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<UpdateWorkflowControlsRequest>,
) -> ApiResult<Json<RuntimeSettings>> {
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id =
        require_user_session(&auth.0, "workflow control updates require a user session")?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let before = get_runtime_settings(&state.pool).await?;
    let mut settings = before.clone();
    if let Some(paused) = request.paused {
        settings.workflow.paused = paused;
    }
    if let Some(dry_run) = request.dry_run {
        settings.workflow.dry_run = dry_run;
    }
    if let Some(limit) = request.hourly_document_limit {
        settings.workflow.hourly_document_limit = limit;
    }
    if let Some(limit) = request.daily_document_limit {
        settings.workflow.daily_document_limit = limit;
    }
    settings = settings.normalized();
    update_runtime_settings(&state.pool, &settings, actor_id).await?;

    let event_type = match (before.workflow.paused, settings.workflow.paused) {
        (false, true) => "workflow.paused",
        (true, false) => "workflow.resumed",
        _ => "workflow.controls_updated",
    };
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: event_type.to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: Some(json!({
                "paused": before.workflow.paused,
                "dry_run": before.workflow.dry_run,
                "hourly_document_limit": before.workflow.hourly_document_limit,
                "daily_document_limit": before.workflow.daily_document_limit
            })),
            after: Some(json!({
                "paused": settings.workflow.paused,
                "dry_run": settings.workflow.dry_run,
                "hourly_document_limit": settings.workflow.hourly_document_limit,
                "daily_document_limit": settings.workflow.daily_document_limit
            })),
            metadata: Some(json!({ "source": "workflow_controls" })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    info!(
        %actor_id,
        event = event_type,
        paused = settings.workflow.paused,
        dry_run = settings.workflow.dry_run,
        "workflow controls updated"
    );
    Ok(Json(settings))
}

fn enrich_provider_usage_costs(usage: &mut [ProviderUsageStats], settings: &RuntimeSettings) {
    for item in usage {
        let Some(provider) = settings
            .ai
            .providers
            .iter()
            .find(|provider| provider.name == item.provider)
        else {
            continue;
        };
        let input_cost = provider.cost_per_1m_input_tokens_usd;
        let output_cost = provider.cost_per_1m_output_tokens_usd;
        item.estimated_cost_usd = match (input_cost, output_cost) {
            (Some(input), Some(output)) => Some(
                (item.input_tokens as f64 / 1_000_000.0 * input)
                    + (item.output_tokens as f64 / 1_000_000.0 * output),
            ),
            _ => None,
        };
    }
}

fn enrich_dashboard_costs(stats: &mut DashboardStats, settings: &RuntimeSettings) {
    enrich_provider_usage_costs(&mut stats.provider_usage, settings);

    let total_cost: f64 = stats
        .provider_usage
        .iter()
        .filter_map(|item| item.estimated_cost_usd)
        .sum();
    stats.kpis.cost_in_range_usd = if stats
        .provider_usage
        .iter()
        .any(|p| p.estimated_cost_usd.is_some())
    {
        Some(total_cost)
    } else {
        None
    };

    let mut weighted_input_cost = 0.0_f64;
    let mut weighted_input_tokens = 0_i64;
    let mut weighted_output_cost = 0.0_f64;
    let mut weighted_output_tokens = 0_i64;
    for item in &stats.provider_usage {
        let Some(provider) = settings
            .ai
            .providers
            .iter()
            .find(|provider| provider.name == item.provider)
        else {
            continue;
        };
        if let Some(rate) = provider.cost_per_1m_input_tokens_usd {
            weighted_input_cost += item.input_tokens as f64 / 1_000_000.0 * rate;
            weighted_input_tokens += item.input_tokens;
        }
        if let Some(rate) = provider.cost_per_1m_output_tokens_usd {
            weighted_output_cost += item.output_tokens as f64 / 1_000_000.0 * rate;
            weighted_output_tokens += item.output_tokens;
        }
    }
    let input_rate_per_token = if weighted_input_tokens > 0 {
        weighted_input_cost / weighted_input_tokens as f64
    } else {
        0.0
    };
    let output_rate_per_token = if weighted_output_tokens > 0 {
        weighted_output_cost / weighted_output_tokens as f64
    } else {
        0.0
    };
    let cost_known = weighted_input_tokens > 0 || weighted_output_tokens > 0;
    for bucket in &mut stats.cost_series {
        if !cost_known {
            bucket.cost_usd = None;
            continue;
        }
        if bucket.input_tokens + bucket.output_tokens == 0 {
            bucket.cost_usd = Some(0.0);
            continue;
        }
        bucket.cost_usd = Some(
            bucket.input_tokens as f64 * input_rate_per_token
                + bucket.output_tokens as f64 * output_rate_per_token,
        );
    }

    stats.cost_breakdown_by_provider = stats
        .provider_usage
        .iter()
        .map(|item| DashboardProviderCostSummary {
            provider: item.provider.clone(),
            model: item.model.clone(),
            cost_usd: item.estimated_cost_usd,
            request_count: item.request_count,
            input_tokens: item.input_tokens,
            output_tokens: item.output_tokens,
            sparkline: Vec::new(),
        })
        .collect();
}

fn enrich_provider_sparklines(
    stats: &mut archivist_core::DashboardStats,
    entries: &[ProviderBucketEntry],
    labels: &[(DateTime<Utc>, String)],
    settings: &RuntimeSettings,
) {
    let bucket_count = labels.len();
    if bucket_count == 0 {
        return;
    }
    // Build the bucket -> index map once; the hot loops below iterate `entries`
    // many times and previously rescanned `labels` linearly for every entry.
    let bucket_index: HashMap<DateTime<Utc>, usize> = labels
        .iter()
        .enumerate()
        .map(|(idx, (bucket, _))| (*bucket, idx))
        .collect();
    let bucket_index_of =
        |bucket: DateTime<Utc>| -> Option<usize> { bucket_index.get(&bucket).copied() };
    let rate_for = |provider_name: &str| -> Option<(f64, f64)> {
        let provider = settings
            .ai
            .providers
            .iter()
            .find(|p| p.name == provider_name)?;
        match (
            provider.cost_per_1m_input_tokens_usd,
            provider.cost_per_1m_output_tokens_usd,
        ) {
            (Some(input), Some(output)) => Some((input, output)),
            _ => None,
        }
    };

    for summary in stats.cost_breakdown_by_provider.iter_mut() {
        let mut buckets: Vec<Option<f64>> = vec![None; bucket_count];
        let rate = rate_for(&summary.provider);
        for entry in entries
            .iter()
            .filter(|e| e.provider == summary.provider && e.model == summary.model)
        {
            let Some(idx) = bucket_index_of(entry.bucket) else {
                continue;
            };
            if let Some((input_rate, output_rate)) = rate {
                let cost = entry.input_tokens as f64 / 1_000_000.0 * input_rate
                    + entry.output_tokens as f64 / 1_000_000.0 * output_rate;
                let slot = &mut buckets[idx];
                *slot = Some(slot.unwrap_or(0.0) + cost);
            }
        }
        summary.sparkline = buckets;
    }

    for usage in stats.provider_usage.iter_mut() {
        let mut buckets: Vec<Option<f64>> = vec![None; bucket_count];
        for entry in entries.iter().filter(|e| {
            e.provider == usage.provider && e.model == usage.model && e.stage == usage.stage
        }) {
            let Some(idx) = bucket_index_of(entry.bucket) else {
                continue;
            };
            buckets[idx] = entry.avg_duration_ms;
        }
        usage.latency_history = buckets;
    }
}

#[derive(Debug, Deserialize)]
struct InventoryQueryParams {
    limit: Option<i64>,
    offset: Option<i64>,
    id: Option<i32>,
    q: Option<String>,
    ocr_status: Option<String>,
    metadata_status: Option<String>,
    run_status: Option<String>,
    tag: Option<String>,
    not_tag: Option<String>,
    lang: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
    has_error: Option<bool>,
    needs_review: Option<bool>,
}

fn split_csv(value: Option<String>) -> Vec<String> {
    value
        .map(|s| {
            s.split(',')
                .map(|part| part.trim().to_owned())
                .filter(|part| !part.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an inventory `date_from`/`date_to` filter. Absent or blank means "no
/// filter"; a present but unparseable value is rejected with 400 instead of
/// being silently ignored (same contract as the statistics range, #312). The
/// column is a real `date` since migration 0043, so only `YYYY-MM-DD` is
/// meaningful here.
fn parse_inventory_date_filter(
    name: &str,
    raw: Option<&str>,
) -> Result<Option<chrono::NaiveDate>, ApiError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| ApiError::bad_request(format!("'{name}' must be a YYYY-MM-DD date"))),
    }
}

async fn inventory(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(query): Query<InventoryQueryParams>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadInventory)?;
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let offset = query.offset.unwrap_or(0).max(0);
    let inventory_query = archivist_db::InventoryQuery {
        id: query.id,
        q: query.q,
        ocr_status: split_csv(query.ocr_status),
        metadata_status: split_csv(query.metadata_status),
        run_status: split_csv(query.run_status),
        tags_include: split_csv(query.tag),
        tags_exclude: split_csv(query.not_tag),
        language: query.lang.filter(|s| !s.is_empty()),
        date_from: parse_inventory_date_filter("date_from", query.date_from.as_deref())?,
        date_to: parse_inventory_date_filter("date_to", query.date_to.as_deref())?,
        has_error: query.has_error,
        needs_review: query.needs_review,
    };
    let settings = get_runtime_settings(&state.pool).await?;
    let (items, total) = tokio::try_join!(
        async {
            list_inventory(&state.pool, &inventory_query, limit, offset)
                .await?
                .into_iter()
                .map(|item| inventory_item_with_debug(item, &settings))
                .collect::<Result<Vec<_>>>()
        },
        async { archivist_db::count_inventory(&state.pool, &inventory_query).await }
    )?;
    Ok(Json(json!({
        "items": items,
        "total": total,
        "offset": offset,
        "limit": limit,
    })))
}

// `GET /api/inventory/duplicates`
//
// Read-only dedup view (#216): groups `document_inventory` by the already
// persisted `ocr_content_hash`, returning every hash shared by more than one
// document. Capped at `DUPLICATE_GROUP_LIMIT` groups; logs a warning when the
// result is truncated so operators know the view is incomplete.
async fn inventory_duplicates(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadInventory)?;
    let settings = get_runtime_settings(&state.pool).await?;
    let groups = archivist_db::list_inventory_duplicates(&state.pool).await?;
    if groups.len() as i64 >= archivist_db::DUPLICATE_GROUP_LIMIT {
        warn!(
            cap = archivist_db::DUPLICATE_GROUP_LIMIT,
            "inventory duplicate groups truncated at cap; some duplicates not shown"
        );
    }
    // Externally reachable Paperless base for browser deep-links: prefer the
    // configured public_url, fall back to the internal base_url. Trailing slash
    // trimmed so the frontend can append `/documents/{id}/details`. Returned
    // here (rather than read from /api/settings) because the Inventory view is
    // available to users without the ReadSettings permission.
    let paperless_base = settings
        .paperless
        .public_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(settings.paperless.base_url.trim())
        .trim_end_matches('/')
        .to_owned();
    Ok(Json(json!({
        "groups": groups,
        "paperless_base": paperless_base,
    })))
}

fn inventory_item_with_debug(
    item: DocumentInventoryItem,
    settings: &RuntimeSettings,
) -> Result<Value> {
    let debug_context = inventory_debug_context(&item, settings);
    let mut value = serde_json::to_value(item)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("debug_context".to_owned(), debug_context);
    }
    Ok(value)
}

fn inventory_debug_context(item: &DocumentInventoryItem, settings: &RuntimeSettings) -> Value {
    let include_tags = WorkflowRules::normalized_tags(&settings.workflow.rules.include_tags);
    let exclude_tags = WorkflowRules::normalized_tags(&settings.workflow.rules.exclude_tags);
    let current_tags = item
        .current_tags
        .iter()
        .map(|tag| tag.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let reason = if settings.workflow.paused {
        "workflow_paused"
    } else if item.complete {
        "complete"
    } else if item.current_run_status.as_deref().is_some_and(|status| {
        matches!(status, "queued" | "running" | "waiting_review" | "applying")
    }) {
        "already_active"
    } else if !exclude_tags.is_empty() && exclude_tags.iter().any(|tag| current_tags.contains(tag))
    {
        "excluded_by_tag"
    } else if !include_tags.is_empty() && !include_tags.iter().any(|tag| current_tags.contains(tag))
    {
        "missing_include_tag"
    } else if item.needs_review {
        "waiting_review"
    } else if item.next_required_stage.is_some() {
        "missing_enabled_stage"
    } else {
        "no_missing_enabled_stage"
    };
    json!({
        "selector_reason": reason,
        "workflow_mode": settings.workflow.mode,
        "workflow_paused": settings.workflow.paused,
        "dry_run": settings.workflow.dry_run,
        "prompt_language": item.detected_language.as_deref().unwrap_or("und"),
        "tag_output_language": settings.tagging.tag_output_language,
        "detected_language": item.detected_language.clone(),
        "detected_language_confidence": item.detected_language_confidence,
        "detected_language_source": item.detected_language_source.clone(),
        "next_required_stage": item.next_required_stage.clone(),
        "last_error": item.last_error.clone()
    })
}

// ---------------------------------------------------------------------------
// Metadata-trace diagnostic endpoint (v1.5.21).
//
// `GET /api/inventory/{document_id}/metadata-trace`
//
// Returns the most recent metadata-stage `pipeline_runs` row for a document
// together with the LLM artifact, all review items, the apply-time audit
// event, and a six-entry `per_field_outcomes` array computed via the
// `compute_field_outcome` decision tree (see `docs/METADATA_TRACE_CONTRACT.md`).
//
// 404s when no metadata run exists yet; the frontend hides the diagnose drawer
// in that case.

/// One of the six metadata-stage fields the diagnostic UI surfaces. Order is
/// load-bearing — the frontend renders the six cards in this exact order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataField {
    Title,
    Correspondent,
    DocumentType,
    DocumentDate,
    Tags,
    Fields,
}

impl MetadataField {
    /// Canonical field name used in `MetadataFieldOutcome.field` AND in
    /// `review_items.suggested_patch.standard_metadata.field`.
    fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Correspondent => "correspondent",
            Self::DocumentType => "document_type",
            Self::DocumentDate => "document_date",
            Self::Tags => "tags",
            Self::Fields => "fields",
        }
    }

    /// All six fields in the contract-specified order.
    fn all() -> [Self; 6] {
        [
            Self::Title,
            Self::Correspondent,
            Self::DocumentType,
            Self::DocumentDate,
            Self::Tags,
            Self::Fields,
        ]
    }

    /// Key under which durable patch-intent audit events carry
    /// this field's value. `document_date` is keyed as `created` because the
    /// worker writes to Paperless's `created` field. `fields` is keyed as
    /// `custom_fields` (the Paperless API name).
    fn audit_key(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Correspondent => "correspondent",
            Self::DocumentType => "document_type",
            Self::DocumentDate => "created",
            Self::Tags => "tags",
            Self::Fields => "custom_fields",
        }
    }

    /// Returns whether this field currently has a value set on the document
    /// inventory snapshot. Used by the "skipped because overwrite disabled"
    /// branch.
    fn current_value_set(self, current: &CurrentState) -> bool {
        match self {
            Self::Title => current.title.as_deref().is_some_and(|v| !v.is_empty()),
            Self::Correspondent => current
                .correspondent
                .as_deref()
                .is_some_and(|v| !v.is_empty()),
            Self::DocumentType => current
                .document_type
                .as_deref()
                .is_some_and(|v| !v.is_empty()),
            Self::DocumentDate => current.document_date.is_some(),
            // Tags & custom-fields don't gate on overwrite. The metadata
            // stage ALWAYS merges into the existing tag set (see
            // OldTagStrategy::KeepExisting) and custom-fields stage just
            // adds entries, so "skipped because overwrite disabled" never
            // applies to either. Returning false here keeps the decision
            // tree from triggering branch 3 for these fields.
            Self::Tags | Self::Fields => false,
        }
    }

    /// Returns whether the metadata settings disallow overwriting an
    /// existing value for this field. Mirrors the four
    /// `overwrite_existing_*` flags. `title` and `tags`/`fields` are not
    /// gated by settings — overwrite is the only sensible behaviour — so we
    /// return `false` so branch 3 never triggers for them.
    fn overwrite_disabled(self, settings: &RuntimeSettings) -> bool {
        match self {
            Self::Correspondent => !settings.metadata.overwrite_existing_correspondent,
            Self::DocumentType => !settings.metadata.overwrite_existing_document_type,
            Self::DocumentDate => !settings.metadata.overwrite_existing_document_date,
            Self::Title | Self::Tags | Self::Fields => false,
        }
    }
}

/// Snapshot of `document_inventory` joined with `paperless_*` lookup tables,
/// reduced to the names the diagnostic UI needs. The IDs in
/// `document_inventory` are resolved to names so the frontend can render the
/// "current state" block without a second round-trip.
#[derive(Debug, Clone, Default)]
struct CurrentState {
    title: Option<String>,
    correspondent: Option<String>,
    document_type: Option<String>,
    /// Typed since migration 0043; serialized as "YYYY-MM-DD" in to_json.
    document_date: Option<chrono::NaiveDate>,
    tags: Vec<String>,
}

impl CurrentState {
    fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "correspondent": self.correspondent,
            "document_type": self.document_type,
            "document_date": self.document_date,
            "tags": self.tags,
        })
    }
}

/// Outcome of the metadata stage for a single field, ready to serialise as a
/// `MetadataFieldOutcome` object (see openapi/openapi.yaml).
#[derive(Debug, Clone)]
struct FieldOutcome {
    field: MetadataField,
    outcome: &'static str,
    value: Value,
    confidence: Option<f64>,
    reason: Option<&'static str>,
    warnings: Vec<Value>,
}

impl FieldOutcome {
    fn to_json(&self) -> Value {
        json!({
            "field": self.field.as_str(),
            "outcome": self.outcome,
            "value": self.value,
            "confidence": self.confidence,
            "reason": self.reason,
            "warnings": self.warnings,
        })
    }
}

/// Pure decision tree from `docs/METADATA_TRACE_CONTRACT.md` §"Outcome
/// composition rules (backend)". Kept side-effect-free so it is table-test
/// friendly — see the `metadata_trace_tests` module below.
///
/// Branch order matters: a review item ALWAYS wins over a bare audit row
/// because the worker creates review items before applying for the manual
/// review path. For full-auto, the worker writes the audit row directly and
/// skips review_items, so branch 2 takes over.
fn compute_field_outcome(
    field: MetadataField,
    review_items: &[&MetadataReviewItem],
    audit: Option<&MetadataApplyAudit>,
    current: &CurrentState,
    settings: &RuntimeSettings,
    llm_suggestion: Option<&Value>,
) -> FieldOutcome {
    // Filter review_items down to entries whose suggested_patch carries
    // `standard_metadata.field == <field>`. Worker code always sets this
    // attribute when creating per-field review items in the consolidated
    // metadata stage; legacy v1.3 per-field review items don't, but those
    // use a per-stage suggested_patch shape that the diagnostic frontend
    // already cannot render — so dropping them here is the correct fallback.
    let matching: Vec<&MetadataReviewItem> = review_items
        .iter()
        .copied()
        .filter(|item| review_item_targets_field(item, field))
        .collect();

    // Confidence + value extraction from the LLM suggestion. Used by
    // multiple branches so we compute them once up front.
    let suggestion_for_field = llm_suggestion
        .and_then(Value::as_object)
        .and_then(|object| object.get(field.as_str()));
    let suggestion_value = field_value_from_suggestion(field, suggestion_for_field);
    let suggestion_confidence = suggestion_for_field
        .and_then(Value::as_object)
        .and_then(|object| object.get("confidence"))
        .and_then(Value::as_f64);

    // ---- Branch 1: a review_item exists for this field. ------------------
    // Order of statuses matters: when multiple review_items exist for the
    // same field (the worker writes one per validation outcome plus an
    // `auto_validated` companion for the operator-review path), prefer the
    // most informative status: rejected > approved > applied > pending >
    // edited. This keeps the diagnostic surfacing the operator's final
    // decision even when the apply-time row lingers.
    if !matching.is_empty() {
        let chosen = pick_review_item(&matching);
        let warnings = warnings_from_value(&chosen.validation_warnings);
        return match chosen.status.as_str() {
            "pending" => FieldOutcome {
                field,
                outcome: "review",
                value: review_item_value(field, chosen).unwrap_or_else(|| suggestion_value.clone()),
                confidence: review_item_confidence(chosen).or(suggestion_confidence),
                reason: classify_review_reason(&warnings),
                warnings,
            },
            "approved" | "applied" | "edited" => FieldOutcome {
                field,
                outcome: "applied",
                value: review_item_value(field, chosen).unwrap_or_else(|| suggestion_value.clone()),
                confidence: review_item_confidence(chosen).or(suggestion_confidence),
                reason: None,
                warnings,
            },
            "rejected" => FieldOutcome {
                field,
                outcome: "rejected",
                value: review_item_value(field, chosen).unwrap_or_else(|| suggestion_value.clone()),
                confidence: review_item_confidence(chosen).or(suggestion_confidence),
                reason: Some("rejected_by_operator"),
                warnings,
            },
            // Unknown statuses fall through to the conservative "review"
            // outcome so the UI still surfaces something rather than 500.
            _ => FieldOutcome {
                field,
                outcome: "review",
                value: review_item_value(field, chosen).unwrap_or_else(|| suggestion_value.clone()),
                confidence: review_item_confidence(chosen).or(suggestion_confidence),
                reason: classify_review_reason(&warnings),
                warnings,
            },
        };
    }

    // ---- Branch 2: audit `after` payload carries this field. -------------
    if let Some(audit) = audit
        && let Some(after) = audit.after.as_ref().and_then(Value::as_object)
        && after.contains_key(field.audit_key())
    {
        return FieldOutcome {
            field,
            outcome: "applied",
            value: suggestion_value,
            confidence: suggestion_confidence,
            reason: None,
            warnings: Vec::new(),
        };
    }

    // ---- Branch 3: current value present + overwrite disabled. -----------
    if field.current_value_set(current) && field.overwrite_disabled(settings) {
        return FieldOutcome {
            field,
            outcome: "skipped",
            value: Value::Null,
            confidence: None,
            reason: Some("overwrite_disabled"),
            warnings: Vec::new(),
        };
    }

    // ---- Branch 4: LLM omitted the field entirely. -----------------------
    if suggestion_for_field.is_none() || suggestion_for_field == Some(&Value::Null) {
        return FieldOutcome {
            field,
            outcome: "dropped",
            value: Value::Null,
            confidence: None,
            reason: Some("no_proposal"),
            warnings: Vec::new(),
        };
    }

    // ---- Branch 5: LLM proposed but entity didn't resolve. ---------------
    //
    // Distinguishes correspondent / document_type "entity_not_found" from
    // generic "below_threshold" by looking at the confidence. If the
    // suggestion was confident enough that validation would have passed
    // but no review_item exists AND nothing was applied, the only way to
    // arrive here for those two fields is the worker's "named_entity_id_for_name
    // returned None" path — captured by `skipped_fields.push(<field>)` in the
    // worker — which is the entity-not-found case. For other fields (title,
    // document_date, tags, fields) the fallback reason is parse_failure.
    let reason: &'static str = match field {
        MetadataField::Correspondent | MetadataField::DocumentType => "entity_not_found",
        _ => "parse_failure",
    };
    FieldOutcome {
        field,
        outcome: "skipped",
        value: suggestion_value,
        confidence: suggestion_confidence,
        reason: Some(reason),
        warnings: Vec::new(),
    }
}

/// Returns whether the review_item's `suggested_patch.standard_metadata.field`
/// matches the requested metadata field.
fn review_item_targets_field(item: &MetadataReviewItem, field: MetadataField) -> bool {
    item.suggested_patch
        .as_object()
        .and_then(|object| object.get("standard_metadata"))
        .and_then(Value::as_object)
        .and_then(|object| object.get("field"))
        .and_then(Value::as_str)
        == Some(field.as_str())
}

/// Pick the most informative review_item from a set keyed to the same field.
/// See `compute_field_outcome` for the priority order.
fn pick_review_item<'a>(items: &[&'a MetadataReviewItem]) -> &'a MetadataReviewItem {
    let priority = |status: &str| -> u8 {
        match status {
            "rejected" => 5,
            "approved" => 4,
            "applied" => 3,
            "pending" => 2,
            "edited" => 1,
            _ => 0,
        }
    };
    items
        .iter()
        .copied()
        .max_by_key(|item| {
            (
                priority(&item.status),
                // Within the same status pick the most recent.
                item.created_at,
            )
        })
        .expect("matching is non-empty by caller invariant")
}

/// Extracts the LLM-suggested confidence for this review_item, if its
/// `suggested_patch.standard_metadata.confidence` carries one.
fn review_item_confidence(item: &MetadataReviewItem) -> Option<f64> {
    item.suggested_patch
        .as_object()
        .and_then(|object| object.get("standard_metadata"))
        .and_then(Value::as_object)
        .and_then(|object| object.get("confidence"))
        .and_then(Value::as_f64)
}

/// Best-effort value extraction from a review_item. Returns `None` when the
/// shape doesn't match what the worker writes; callers fall back to the LLM
/// suggestion in that case.
fn review_item_value(field: MetadataField, item: &MetadataReviewItem) -> Option<Value> {
    let object = item.suggested_patch.as_object()?;
    // Prefer the human-readable `suggested_name` / `suggested_date` /
    // `suggested_names` from `standard_metadata` so the UI shows names
    // rather than ID integers. Falls back to the raw patch fields for
    // legacy worker code that didn't set the friendly hints.
    let standard = object.get("standard_metadata").and_then(Value::as_object);
    match field {
        MetadataField::Title => object.get("title").cloned(),
        MetadataField::Correspondent => standard
            .and_then(|s| s.get("suggested_name"))
            .cloned()
            .or_else(|| object.get("correspondent").cloned()),
        MetadataField::DocumentType => standard
            .and_then(|s| s.get("suggested_name"))
            .cloned()
            .or_else(|| object.get("document_type").cloned()),
        MetadataField::DocumentDate => standard
            .and_then(|s| s.get("suggested_date"))
            .cloned()
            .or_else(|| object.get("created").cloned()),
        MetadataField::Tags => standard
            .and_then(|s| s.get("suggested_names"))
            .cloned()
            .or_else(|| object.get("tags").cloned()),
        MetadataField::Fields => standard
            .and_then(|s| s.get("suggested_names"))
            .cloned()
            .or_else(|| object.get("custom_fields").cloned()),
    }
}

/// Returns the canonical `reason` string for a review_item by inspecting its
/// validation_warnings array. The worker serialises each warning as a
/// `ValidationError` JSON tag — we pick the first recognised one and map to
/// the contract's canonical reason word.
fn classify_review_reason(warnings: &[Value]) -> Option<&'static str> {
    for warning in warnings {
        match warning {
            Value::String(text) if text.eq_ignore_ascii_case("LowConfidence") => {
                return Some("below_threshold");
            }
            Value::Object(map) => {
                if map.contains_key("LowConfidence") {
                    return Some("below_threshold");
                }
                if map.contains_key("UnknownChoice") {
                    return Some("entity_not_found");
                }
                if map.contains_key("UnknownTag") {
                    return Some("entity_not_found");
                }
                if map.contains_key("InvalidDate") {
                    return Some("parse_failure");
                }
                if map.contains_key("TooManyTags") {
                    return Some("over_max_tags");
                }
                if let Some(quality) = map.get("DataQuality").and_then(Value::as_str)
                    && quality.contains("anchor")
                {
                    return Some("anchor_missing");
                }
            }
            _ => {}
        }
    }
    None
}

/// `review_items.validation_warnings` is stored as JSON. Coerce it to a Vec
/// for the response while preserving the per-warning shape so the frontend
/// can render details.
fn warnings_from_value(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

/// Pull the field's canonical value out of `ai_artifacts.normalized_output`.
/// Returns `Value::Null` when the LLM omitted the field.
fn field_value_from_suggestion(field: MetadataField, suggestion: Option<&Value>) -> Value {
    let Some(object) = suggestion.and_then(Value::as_object) else {
        return Value::Null;
    };
    let value = match field {
        MetadataField::Title => object.get("title"),
        MetadataField::Correspondent | MetadataField::DocumentType => object.get("name"),
        MetadataField::DocumentDate => object.get("date"),
        MetadataField::Tags => object.get("tags"),
        MetadataField::Fields => object.get("fields"),
    };
    value.cloned().unwrap_or(Value::Null)
}

async fn load_current_state(pool: &DbPool, paperless_document_id: i32) -> Result<CurrentState> {
    // `document_inventory` only carries the correspondent / document_type
    // INTEGER IDs and the raw `current_tags` text array. The diagnostic
    // surfaces names — resolve via the cached `paperless_*` lookup tables.
    let row = sqlx::query(
        r#"
        select di.title,
               di.current_tags,
               di.document_date,
               c.name as correspondent_name,
               t.name as document_type_name
          from document_inventory di
          left join paperless_correspondents c on c.id = di.correspondent_id
          left join paperless_document_types t on t.id = di.document_type_id
         where di.paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = row {
        let current_tags: Vec<String> = row.try_get("current_tags")?;
        Ok(CurrentState {
            title: row.try_get("title")?,
            correspondent: row.try_get("correspondent_name")?,
            document_type: row.try_get("document_type_name")?,
            document_date: row.try_get("document_date")?,
            tags: current_tags,
        })
    } else {
        // Document not in the local inventory yet (e.g. the run was queued
        // before the first paperless sync settled). Return a blank snapshot
        // so the diagnostic still renders the run header + per-field
        // outcomes.
        Ok(CurrentState::default())
    }
}

#[tracing::instrument(skip(state, auth), fields(user_id = tracing::field::Empty))]
async fn inventory_metadata_trace(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(paperless_document_id): Path<i32>,
) -> ApiResult<Response> {
    require(&auth.0, Permission::ReadInventory)?;
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }

    let Some(header) = latest_metadata_run_for_document(&state.pool, paperless_document_id).await?
    else {
        // No metadata run yet — frontend hides the Diagnose button until the
        // first run completes, but this is also the canonical 404 shape.
        let body = Json(json!({ "error": "no metadata run for this document" }));
        return Ok((StatusCode::NOT_FOUND, body).into_response());
    };

    let (artifact, review_items, audit, current, settings) = tokio::try_join!(
        latest_metadata_artifact_for_run(&state.pool, header.run_id),
        metadata_review_items_for_run(&state.pool, header.run_id),
        latest_apply_audit_for_run(&state.pool, header.run_id),
        load_current_state(&state.pool, paperless_document_id),
        get_runtime_settings(&state.pool),
    )?;

    let llm_suggestion: Option<Value> = artifact
        .as_ref()
        .and_then(|artifact| artifact.normalized_output.clone());
    let review_refs: Vec<&MetadataReviewItem> = review_items.iter().collect();
    let audit_ref = audit.as_ref();
    let outcomes: Vec<Value> = MetadataField::all()
        .iter()
        .map(|field| {
            compute_field_outcome(
                *field,
                &review_refs,
                audit_ref,
                &current,
                &settings,
                llm_suggestion.as_ref(),
            )
            .to_json()
        })
        .collect();

    let response = metadata_trace_response_body(
        paperless_document_id,
        &header,
        artifact.as_ref(),
        audit_ref,
        &current,
        outcomes,
        llm_suggestion.clone(),
    );
    Ok((StatusCode::OK, Json(response)).into_response())
}

fn metadata_trace_response_body(
    paperless_document_id: i32,
    header: &MetadataRunHeader,
    artifact: Option<&MetadataArtifact>,
    audit: Option<&MetadataApplyAudit>,
    current: &CurrentState,
    outcomes: Vec<Value>,
    llm_suggestion: Option<Value>,
) -> Value {
    let applied_at = audit
        .filter(|audit| audit.outcome == "success")
        .map(|audit| audit.created_at);

    json!({
        "paperless_document_id": paperless_document_id,
        "current_state": current.to_json(),
        "latest_run": {
            "run_id": header.run_id,
            "stage": "metadata",
            "status": header.status,
            "created_at": header.created_at,
            "applied_at": applied_at,
            "model": artifact.map(|a| a.model.clone()),
            "provider": artifact.map(|a| a.provider.clone()),
            "llm_suggestion": llm_suggestion,
            "per_field_outcomes": outcomes,
        }
    })
}

#[derive(Debug, Deserialize)]
struct CreateChatSessionRequest {
    title: Option<String>,
}

async fn chat_sessions(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::UseChat)?;
    let user_id = require_user_session(&auth.0, "document chat requires a user session")?;
    let include_all = roles_have_permission(&auth.0.roles, Permission::ManageUsers);
    Ok(Json(json!({
        "items": list_document_chat_sessions(&state.pool, Some(user_id), include_all, 100).await?
    })))
}

async fn create_chat_session(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<CreateChatSessionRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::UseChat)?;
    let user_id = require_user_session(&auth.0, "document chat requires a user session")?;
    let title = chat_title(request.title.as_deref().unwrap_or("New document chat"));
    let id = create_document_chat_session(&state.pool, &title, Some(user_id)).await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "chat.session_created".to_owned(),
            actor_type: auth.0.actor_type,
            actor_id: auth.0.actor_id,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "session_id": id, "title": title })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    Ok(Json(json!({ "id": id, "title": title })))
}

async fn chat_messages(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(session_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::UseChat)?;
    let user_id = require_user_session(&auth.0, "document chat requires a user session")?;
    let include_all = roles_have_permission(&auth.0.roles, Permission::ManageUsers);
    ensure_chat_visible(&state.pool, session_id, Some(user_id), include_all).await?;
    Ok(Json(json!({
        "items": list_document_chat_messages(&state.pool, session_id).await?
    })))
}

#[derive(Debug, Deserialize)]
struct PostChatMessageRequest {
    question: String,
    document_ids: Option<Vec<i32>>,
    max_sources: Option<usize>,
}

async fn post_chat_message(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(session_id): Path<Uuid>,
    Json(request): Json<PostChatMessageRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::UseChat)?;
    let user_id = require_user_session(&auth.0, "document chat requires a user session")?;
    let include_all = roles_have_permission(&auth.0.roles, Permission::ManageUsers);
    ensure_chat_visible(&state.pool, session_id, Some(user_id), include_all).await?;

    let question = request.question.trim();
    if question.chars().count() < 3 {
        return Err(ApiError::bad_request(
            "question must be at least 3 characters",
        ));
    }
    if question.chars().count() > 4000 {
        return Err(ApiError::bad_request(
            "question must be at most 4000 characters",
        ));
    }
    let document_ids = normalize_chat_document_ids(request.document_ids)?;

    let settings = get_runtime_settings(&state.pool).await?;
    let provider = provider_for_default_text(&settings)?;
    let sources = retrieve_document_chat_sources(
        &state,
        &settings,
        question,
        document_ids.as_deref(),
        request.max_sources.unwrap_or(6),
    )
    .await?;
    let prompt = build_document_chat_prompt(question, &sources);
    let response = chat_with_api_provider(
        &state,
        &provider,
        build_document_chat_request(&provider, prompt.system_prompt, prompt.user_prompt),
    )
    .await?;
    let answer = response.text.clone();
    let provider_name = response.provider.clone();
    let model = response.model.clone();
    let user_message_id = insert_document_chat_message(
        &state.pool,
        session_id,
        "user",
        question,
        None,
        None,
        Some(json!({ "document_ids": document_ids })),
    )
    .await?;
    let assistant_message_id = insert_document_chat_message(
        &state.pool,
        session_id,
        "assistant",
        &answer,
        Some(&provider_name),
        Some(&model),
        Some(json!({
            "duration_ms": response.duration_ms,
            "source_count": sources.len(),
            "user_message_id": user_message_id
        })),
    )
    .await?;
    insert_document_chat_sources(&state.pool, assistant_message_id, &sources).await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "chat.message_created".to_owned(),
            actor_type: auth.0.actor_type,
            actor_id: auth.0.actor_id,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "session_id": session_id,
                "user_message_id": user_message_id,
                "assistant_message_id": assistant_message_id,
                "provider": provider_name,
                "model": model,
                "source_documents": sources.iter().map(|source| source.paperless_document_id).collect::<Vec<_>>()
            })),
            metadata: Some(json!({ "question_hash": hash_token(question) })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    Ok(Json(json!({
        "session_id": session_id,
        "user_message_id": user_message_id,
        "assistant_message_id": assistant_message_id,
        "answer": answer,
        "sources": sources
    })))
}

fn normalize_chat_document_ids(document_ids: Option<Vec<i32>>) -> ApiResult<Option<Vec<i32>>> {
    let Some(document_ids) = document_ids else {
        return Ok(None);
    };
    if document_ids.len() > MAX_CHAT_DOCUMENT_FILTER_IDS {
        return Err(ApiError::bad_request(format!(
            "document_ids may contain at most {MAX_CHAT_DOCUMENT_FILTER_IDS} entries"
        )));
    }

    let mut normalized = Vec::with_capacity(document_ids.len());
    for document_id in document_ids {
        if document_id <= 0 {
            return Err(ApiError::bad_request(
                "document_ids must contain positive Paperless document IDs",
            ));
        }
        if !normalized.contains(&document_id) {
            normalized.push(document_id);
        }
    }

    if normalized.is_empty() {
        Ok(None)
    } else {
        Ok(Some(normalized))
    }
}

#[derive(Debug, Deserialize)]
struct TriggerRequest {
    stages: Option<Vec<Stage>>,
    mode: Option<ProcessingMode>,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(
        paperless_document_id = document_id,
        user_id = tracing::field::Empty,
        run_id = tracing::field::Empty
    )
)]
async fn trigger_document(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(document_id): Path<i32>,
    Json(request): Json<TriggerRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteRuns)?;
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    let stages = request
        .stages
        .unwrap_or_else(|| settings.workflow.enabled_stages.clone());
    let mode = request.mode.unwrap_or(settings.workflow.mode);
    // v1.4.0 priority scheduling: manual triggers carry priority 0 so an operator-initiated
    // run jumps ahead of every queued auto-selected run regardless of document age.
    let run_id = create_run_with_jobs_with_priority(
        &state.pool,
        document_id,
        &stages,
        mode,
        "manual",
        &auth.0.actor_type,
        Some(0),
    )
    .await?;
    Span::current().record("run_id", tracing::field::display(run_id));
    info!(%run_id, paperless_document_id = document_id, "manual run triggered");
    Ok(Json(json!({ "run_id": run_id })))
}

/// Inbound webhook body. Accepts either a batch (`document_ids`) or a single
/// (`document_id`) shape so a Paperless workflow can post whichever it has.
#[derive(Debug, Deserialize)]
struct WebhookConsumedRequest {
    #[serde(default)]
    document_ids: Option<Vec<i32>>,
    #[serde(default)]
    document_id: Option<i32>,
}

/// Machine-to-machine webhook: a Paperless workflow posts here when it consumes
/// a document so we trigger processing immediately instead of waiting for the
/// next ~60s poll.
///
/// This route lives OUTSIDE the auth-required router layer (no user session); it
/// is gated solely by the shared `ARCHIVIST_WEBHOOK_SECRET`, supplied in the
/// `X-Webhook-Secret` header and compared in constant time. When the env var is
/// unset the endpoint is disabled and returns `503`.
#[tracing::instrument(
    skip(state, headers, request),
    fields(queued = tracing::field::Empty)
)]
async fn webhook_paperless_document_consumed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<WebhookConsumedRequest>,
) -> ApiResult<Response> {
    let Some(expected) = state.config.webhook_secret.as_ref() else {
        return Err(ApiError::service_unavailable("webhook disabled"));
    };
    let provided = headers
        .get("x-webhook-secret")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let expected = expected.expose_secret();
    // Constant-time compare to deny timing oracles on the shared secret.
    if expected.len() != provided.len()
        || !bool::from(expected.as_bytes().ct_eq(provided.as_bytes()))
    {
        return Err(ApiError::unauthorized("invalid webhook secret"));
    }

    // Merge both accepted shapes, drop non-positive ids, and de-duplicate so a
    // single payload never enqueues the same document twice.
    let mut normalized: Vec<i32> = Vec::new();
    for document_id in request
        .document_ids
        .into_iter()
        .flatten()
        .chain(request.document_id)
    {
        if document_id <= 0 {
            return Err(ApiError::bad_request(
                "document ids must be positive Paperless document IDs",
            ));
        }
        if !normalized.contains(&document_id) {
            normalized.push(document_id);
        }
    }
    if normalized.is_empty() {
        return Err(ApiError::bad_request(
            "document_ids or document_id is required",
        ));
    }

    let settings = get_runtime_settings(&state.pool).await?;
    let stages = settings.workflow.enabled_stages.clone();
    let mode = settings.workflow.mode;
    // Webhook-triggered runs carry priority 0 (same as manual triggers) so a
    // freshly consumed document jumps ahead of queued auto-selected work.
    let mut queued: i64 = 0;
    for document_id in normalized {
        create_run_with_jobs_with_priority(
            &state.pool,
            document_id,
            &stages,
            mode,
            "webhook",
            "webhook",
            Some(0),
        )
        .await?;
        queued += 1;
    }
    Span::current().record("queued", queued);
    info!(queued, "webhook enqueued documents");
    Ok((StatusCode::ACCEPTED, Json(json!({ "queued": queued }))).into_response())
}

#[tracing::instrument(
    skip(state, auth),
    fields(user_id = tracing::field::Empty, queued = tracing::field::Empty)
)]
async fn queue_ocr_batch(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    let created = queue_missing_stage(
        &state.pool,
        Stage::Ocr,
        settings.workflow.mode,
        &auth.0.actor_type,
        &settings.workflow.rules,
        None,
    )
    .await?;
    Span::current().record("queued", created);
    info!(queued = created, "queued missing OCR documents");
    Ok(Json(json!({ "queued": created })))
}

#[tracing::instrument(
    skip(state, auth),
    fields(user_id = tracing::field::Empty, queued = tracing::field::Empty)
)]
async fn queue_full_batch(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    // Emit ONE pipeline_run per eligible document with the full enabled-stages array
    // (e.g. `["ocr","metadata"]`), so the document drains the entire pipeline within a
    // single run. The previous per-stage loop created separate single-stage runs which
    // forced operators to press "Queue Full" twice to advance both stages.
    let created = queue_missing_pipeline(
        &state.pool,
        &settings.workflow.enabled_stages,
        settings.workflow.mode,
        "manual-batch",
        &auth.0.actor_type,
        &settings.workflow.rules,
        None,
    )
    .await?;
    Span::current().record("queued", created);
    info!(queued = created, "queued full pipeline batch");
    Ok(Json(json!({ "queued": created })))
}

#[derive(Debug, Deserialize)]
struct RerunBatchRequest {
    document_ids: Vec<i32>,
    /// Stage names (e.g. `["ocr","metadata"]`). Validated against the known [`Stage`] set.
    stages: Vec<String>,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, queued = tracing::field::Empty)
)]
async fn rerun_batch(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<RerunBatchRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }

    if request.document_ids.is_empty() {
        return Err(ApiError::bad_request("document_ids must not be empty"));
    }
    if request.stages.is_empty() {
        return Err(ApiError::bad_request("stages must not be empty"));
    }

    // Validate every requested stage against the known Stage set before queueing anything.
    let stages = request
        .stages
        .iter()
        .map(|raw| {
            raw.parse::<Stage>()
                .map_err(|_| ApiError::bad_request(format!("unknown stage: {raw}")))
        })
        .collect::<Result<Vec<Stage>, _>>()?;

    // De-duplicate ids so a doubled id can't enqueue (or attempt) two runs.
    let mut document_ids: Vec<i32> = request.document_ids;
    document_ids.sort_unstable();
    document_ids.dedup();

    let settings = get_runtime_settings(&state.pool).await?;
    // Operator-initiated re-run jumps ahead of age-derived auto-selected runs (priority 0),
    // mirroring the manual single-document trigger.
    let queued = create_runs_for_documents(
        &state.pool,
        &document_ids,
        &stages,
        settings.workflow.mode,
        "bulk-rerun",
        &auth.0.actor_type,
        Some(0),
    )
    .await?;
    Span::current().record("queued", queued);
    info!(queued, "queued bulk re-run batch");
    Ok(Json(json!({ "queued": queued })))
}

/// Re-run every document the dashboard counts as failed (a failed `ocr` or
/// `metadata` stage, no active run) in one click, so an operator does not have
/// to filter the inventory and hand-select after an upstream incident.
/// Idempotent: documents already being reprocessed are skipped by the
/// per-document active-run guard, so a double click cannot duplicate runs.
async fn rerun_failed_batch(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }

    let document_ids = failed_document_ids(&state.pool).await?;
    if document_ids.is_empty() {
        return Ok(Json(json!({ "queued": 0, "candidates": 0 })));
    }

    let settings = get_runtime_settings(&state.pool).await?;
    // Re-run both stages: OCR is page-cached so a still-good result is cheap to
    // revalidate and a failed OCR is redone. Priority 0 jumps ahead of
    // age-derived auto-selected runs, mirroring the hand-picked rerun.
    let queued = create_runs_for_documents(
        &state.pool,
        &document_ids,
        &[Stage::Ocr, Stage::Metadata],
        settings.workflow.mode,
        "rerun-failed",
        &auth.0.actor_type,
        Some(0),
    )
    .await?;
    Span::current().record("queued", queued);
    info!(
        queued,
        candidates = document_ids.len(),
        "queued re-run of all failed documents"
    );
    Ok(Json(
        json!({ "queued": queued, "candidates": document_ids.len() }),
    ))
}

#[derive(Debug, Deserialize)]
struct ReviewQuery {
    status: Option<String>,
    limit: Option<i64>,
}

async fn reviews(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(query): Query<ReviewQuery>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadReviews)?;
    let settings = get_runtime_settings(&state.pool).await?;
    let items = list_reviews(
        &state.pool,
        query.status.as_deref(),
        query.limit.unwrap_or(100).clamp(1, 500),
    )
    .await?
    .into_iter()
    .map(|review| review_with_debug(review, &settings))
    .collect::<Result<Vec<_>>>()?;
    let total = count_reviews(&state.pool, query.status.as_deref()).await?;
    let has_more = total > items.len() as i64;
    Ok(Json(json!({
        "items": items,
        "total": total,
        "has_more": has_more
    })))
}

fn review_with_debug(review: ReviewItemRecord, settings: &RuntimeSettings) -> Result<Value> {
    let mut value = serde_json::to_value(review)?;
    if let Some(object) = value.as_object_mut() {
        let mut debug = object
            .get("debug_context")
            .cloned()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        debug.insert("workflow_mode".to_owned(), json!(settings.workflow.mode));
        debug.insert(
            "workflow_paused".to_owned(),
            json!(settings.workflow.paused),
        );
        debug.insert("dry_run".to_owned(), json!(settings.workflow.dry_run));
        debug.insert(
            "tag_output_language".to_owned(),
            json!(settings.tagging.tag_output_language),
        );
        let prompt_language = debug
            .get("detected_language")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("und")
            .to_owned();
        debug.insert("prompt_language".to_owned(), json!(prompt_language));
        object.insert("debug_context".to_owned(), Value::Object(debug));
    }
    Ok(value)
}

#[derive(Debug, Deserialize)]
struct BatchReviewRequest {
    ids: Vec<Uuid>,
    decision: String,
}

async fn batch_review(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<BatchReviewRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteReviews)?;
    let actor_id = require_user_session(&auth.0, "review decisions require a user session")?;
    if request.ids.is_empty() {
        return Err(ApiError::bad_request("ids must not be empty"));
    }
    if request.ids.len() > 100 {
        return Err(ApiError::bad_request(
            "batch review is limited to 100 items per request",
        ));
    }
    if !matches!(request.decision.as_str(), "approve" | "reject") {
        return Err(ApiError::bad_request(
            "decision must be either 'approve' or 'reject'",
        ));
    }

    let mut applied = Vec::new();
    let mut failed = Vec::new();
    for id in request.ids {
        let result = if request.decision == "approve" {
            match review_decision(&state.pool, id, "approved", None, actor_id).await {
                Ok(()) => apply_review_patch(&state, id, actor_id).await,
                Err(error) => Err(error),
            }
        } else {
            review_decision(&state.pool, id, "rejected", None, actor_id).await
        };
        match result {
            Ok(()) => applied.push(id),
            Err(error) => failed.push(json!({ "id": id, "error": error.to_string() })),
        }
    }

    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: format!("review.batch_{}", request.decision),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "succeeded": applied.len(), "failed": failed.len() })),
            metadata: None,
            outcome: if failed.is_empty() {
                "success".to_owned()
            } else {
                "partial_failure".to_owned()
            },
            error_message: failed
                .first()
                .and_then(|entry| entry.get("error"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    Ok(Json(json!({
        "ok": failed.is_empty(),
        "succeeded": applied,
        "failed": failed
    })))
}

#[tracing::instrument(
    skip(state, auth),
    fields(review_id = %id, user_id = tracing::field::Empty)
)]
async fn approve_review(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteReviews)?;
    let actor_id = auth
        .0
        .user_id
        .ok_or_else(|| ApiError::forbidden("review decisions require a user session"))?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    review_decision(&state.pool, id, "approved", None, actor_id).await?;
    apply_review_patch(&state, id, actor_id).await?;
    info!(review_id = %id, %actor_id, "review approved");
    Ok(Json(json!({ "ok": true })))
}

#[tracing::instrument(
    skip(state, auth),
    fields(review_id = %id, user_id = tracing::field::Empty)
)]
async fn reject_review(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteReviews)?;
    let actor_id = auth
        .0
        .user_id
        .ok_or_else(|| ApiError::forbidden("review decisions require a user session"))?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    review_decision(&state.pool, id, "rejected", None, actor_id).await?;
    info!(review_id = %id, %actor_id, "review rejected");
    Ok(Json(json!({ "ok": true })))
}

/// Decision made by `clean_review_patch_for_auto_fix` for one review_item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoFixAction {
    /// Patch has meaningful content after cleaning — apply it.
    Apply,
    /// Patch is empty after cleaning — reject the review_item.
    Reject,
}

/// Result of cleaning one review_item's suggested_patch for the auto-fix path.
#[derive(Debug, Clone)]
struct AutoFixDecision {
    cleaned_patch: Value,
    fields_dropped: Vec<String>,
    action: AutoFixAction,
}

/// Inspect a review_item's `suggested_patch` + `validation_warnings` and
/// produce a cleaned patch that drops fields the validator flagged
/// (UnknownChoice / UnknownTag / UnknownField / EmptyOutput), plus a
/// decision on whether anything useful remains.
///
/// The heuristic is conservative:
/// * Any `UnknownChoice` or `UnknownField` warning → drop the entire
///   `custom_fields` array. Most production failures are select-typed
///   custom-field values the LLM made up; Paperless rejects them at the
///   patch boundary, so the safest cleanup is to skip them entirely.
/// * Drop `document_type` / `correspondent` keys whose value is `null`
///   (means the LLM proposed a name that didn't resolve to an ID).
/// * Drop empty `tags` arrays.
/// * Whatever non-null, non-empty fields remain in `{title, correspondent,
///   document_type, created, tags, custom_fields, content}` → Apply.
///   Otherwise → Reject (nothing meaningful left to write to Paperless).
fn clean_review_patch_for_auto_fix(
    suggested_patch: &Value,
    validation_warnings: &Value,
) -> AutoFixDecision {
    let mut patch_obj = suggested_patch.as_object().cloned().unwrap_or_default();
    let warnings_arr: Vec<&Value> = validation_warnings
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();

    let warning_has_kind = |kind: &str| -> bool {
        warnings_arr.iter().any(|w| match w {
            Value::Object(obj) => obj.contains_key(kind),
            Value::String(s) => s == kind || s.contains(kind),
            _ => false,
        })
    };

    let has_unknown_choice = warning_has_kind("UnknownChoice");
    let has_unknown_field = warning_has_kind("UnknownField");
    let has_unknown_tag = warning_has_kind("UnknownTag");
    let has_empty_output = warning_has_kind("EmptyOutput");

    let mut fields_dropped: Vec<String> = Vec::new();

    // Drop custom_fields entirely if any UnknownChoice/UnknownField/EmptyOutput
    // — these all mean at least one custom-field entry would fail at Paperless.
    if (has_unknown_choice || has_unknown_field || has_empty_output)
        && patch_obj.remove("custom_fields").is_some()
    {
        fields_dropped.push("custom_fields".to_owned());
    }

    // Drop document_type / correspondent if they're null (failed resolution).
    for field in ["document_type", "correspondent"] {
        if let Some(v) = patch_obj.get(field)
            && v.is_null()
        {
            patch_obj.remove(field);
            fields_dropped.push(format!("{field} (null)"));
        }
    }

    // Drop tags if empty array (nothing to add) OR if there was an UnknownTag
    // warning AND the patch's tags list is empty/missing (defensive).
    if let Some(v) = patch_obj.get("tags")
        && v.as_array().is_some_and(|a| a.is_empty())
    {
        patch_obj.remove("tags");
        fields_dropped.push("tags (empty)".to_owned());
    }
    if has_unknown_tag && !patch_obj.contains_key("tags") {
        // Already dropped or never there; nothing to do.
    }

    // Strip empty-string title.
    if let Some(v) = patch_obj.get("title")
        && v.as_str().is_some_and(|s| s.trim().is_empty())
    {
        patch_obj.remove("title");
        fields_dropped.push("title (empty)".to_owned());
    }

    // Strip null `created`.
    if let Some(v) = patch_obj.get("created")
        && (v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty()))
    {
        patch_obj.remove("created");
        fields_dropped.push("created (null/empty)".to_owned());
    }

    let useful_keys = [
        "title",
        "correspondent",
        "document_type",
        "created",
        "tags",
        "custom_fields",
        "content",
    ];
    let has_meaningful_content = patch_obj.keys().any(|k| {
        useful_keys.contains(&k.as_str()) && patch_obj.get(k).is_some_and(|v| !v.is_null())
    });

    AutoFixDecision {
        cleaned_patch: Value::Object(patch_obj),
        fields_dropped,
        action: if has_meaningful_content {
            AutoFixAction::Apply
        } else {
            AutoFixAction::Reject
        },
    }
}

#[derive(Debug, Deserialize, Default)]
struct AutoFixRequest {
    /// Limit how many pending reviews to touch in this call.
    #[serde(default)]
    limit: Option<i64>,
}

async fn auto_fix_preview(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<AutoFixRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteReviews)?;
    let limit = request.limit.unwrap_or(500).clamp(1, 2000);
    let items = archivist_db::list_reviews(&state.pool, Some("pending"), limit).await?;
    let mut apply_count = 0_i64;
    let mut reject_count = 0_i64;
    let mut sample: Vec<Value> = Vec::with_capacity(20);
    for item in &items {
        let decision =
            clean_review_patch_for_auto_fix(&item.suggested_patch, &item.validation_warnings);
        match decision.action {
            AutoFixAction::Apply => apply_count += 1,
            AutoFixAction::Reject => reject_count += 1,
        }
        if sample.len() < 20 {
            sample.push(json!({
                "id": item.id,
                "paperless_document_id": item.paperless_document_id,
                "stage": item.stage,
                "action": match decision.action { AutoFixAction::Apply => "apply", AutoFixAction::Reject => "reject" },
                "fields_dropped": decision.fields_dropped,
            }));
        }
    }
    Ok(Json(json!({
        "total_pending": items.len(),
        "would_apply": apply_count,
        "would_reject": reject_count,
        "sample": sample,
    })))
}

async fn auto_fix_bulk(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<AutoFixRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteReviews)?;
    let actor_id = require_user_session(&auth.0, "review decisions require a user session")?;
    let limit = request.limit.unwrap_or(500).clamp(1, 2000);
    let items = archivist_db::list_reviews(&state.pool, Some("pending"), limit).await?;
    let mut applied = 0_i64;
    let mut rejected = 0_i64;
    let mut errors: Vec<Value> = Vec::new();

    for item in items {
        let outcome = auto_fix_apply_one(&state, &item, actor_id).await;
        match outcome {
            Ok(AutoFixAction::Apply) => applied += 1,
            Ok(AutoFixAction::Reject) => rejected += 1,
            Err(error) => {
                errors.push(json!({
                    "id": item.id,
                    "error": error.to_string(),
                }));
            }
        }
    }
    info!(applied, rejected, errors = errors.len(), %actor_id, "auto-fix bulk completed");
    Ok(Json(json!({
        "applied": applied,
        "rejected": rejected,
        "errors": errors,
    })))
}

async fn auto_fix_single(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteReviews)?;
    let actor_id = require_user_session(&auth.0, "review decisions require a user session")?;
    let items = archivist_db::list_reviews(&state.pool, Some("pending"), 2000).await?;
    let Some(item) = items.into_iter().find(|i| i.id == id) else {
        return Err(ApiError::bad_request(
            "review item not pending or not found",
        ));
    };
    let action = auto_fix_apply_one(&state, &item, actor_id).await?;
    Ok(Json(json!({
        "action": match action { AutoFixAction::Apply => "applied", AutoFixAction::Reject => "rejected" },
    })))
}

/// Auto-fix one review_item. Returns the action that was taken.
async fn auto_fix_apply_one(
    state: &AppState,
    item: &archivist_db::ReviewItemRecord,
    actor_id: Uuid,
) -> Result<AutoFixAction> {
    let decision =
        clean_review_patch_for_auto_fix(&item.suggested_patch, &item.validation_warnings);
    match decision.action {
        AutoFixAction::Apply => {
            // Stamp the cleaned patch onto the review_item as edited_patch,
            // then route through the existing approve+apply pipeline so the
            // audit trail and Paperless write semantics stay identical.
            review_decision(
                &state.pool,
                item.id,
                "edited",
                Some(decision.cleaned_patch.clone()),
                actor_id,
            )
            .await?;
            review_decision(&state.pool, item.id, "approved", None, actor_id).await?;
            apply_review_patch(state, item.id, actor_id).await?;
            append_audit(
                &state.pool,
                AuditEventInput {
                    event_type: "review.auto_fix_applied".to_owned(),
                    actor_type: "user".to_owned(),
                    actor_id: Some(actor_id.to_string()),
                    run_id: item.run_id,
                    job_id: item.job_id,
                    paperless_document_id: Some(item.paperless_document_id),
                    before: None,
                    after: Some(json!({ "fields_dropped": decision.fields_dropped })),
                    metadata: Some(json!({ "review_id": item.id, "stage": item.stage })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
            Ok(AutoFixAction::Apply)
        }
        AutoFixAction::Reject => {
            review_decision(&state.pool, item.id, "rejected", None, actor_id).await?;
            append_audit(
                &state.pool,
                AuditEventInput {
                    event_type: "review.auto_fix_rejected".to_owned(),
                    actor_type: "user".to_owned(),
                    actor_id: Some(actor_id.to_string()),
                    run_id: item.run_id,
                    job_id: item.job_id,
                    paperless_document_id: Some(item.paperless_document_id),
                    before: None,
                    after: Some(json!({
                        "fields_dropped": decision.fields_dropped,
                        "reason": "no meaningful patch after cleanup",
                    })),
                    metadata: Some(json!({ "review_id": item.id, "stage": item.stage })),
                    outcome: "success".to_owned(),
                    error_message: None,
                    source_ip: None,
                    user_agent: None,
                },
            )
            .await?;
            Ok(AutoFixAction::Reject)
        }
    }
}

#[derive(Debug, Deserialize)]
struct EditReviewRequest {
    patch: DocumentPatch,
}

async fn edit_review(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<EditReviewRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteReviews)?;
    let actor_id = auth
        .0
        .user_id
        .ok_or_else(|| ApiError::forbidden("review decisions require a user session"))?;
    review_decision(
        &state.pool,
        id,
        "edited",
        Some(serde_json::to_value(request.patch)?),
        actor_id,
    )
    .await?;
    apply_review_patch(&state, id, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct RecoveryQuery {
    older_than_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RecoveryRequest {
    older_than_seconds: Option<i64>,
}

fn recovery_window_seconds(value: Option<i64>) -> i64 {
    value.unwrap_or(600).clamp(60, 86_400)
}

async fn recovery_status(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(query): Query<RecoveryQuery>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadRuns)?;
    let older_than_seconds = recovery_window_seconds(query.older_than_seconds);
    Ok(Json(json!({
        "older_than_seconds": older_than_seconds,
        "items": recovery_candidates(&state.pool, older_than_seconds).await?
    })))
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, older_than_seconds = tracing::field::Empty)
)]
async fn recover_stale_leases_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<RecoveryRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteRuns)?;
    let actor_id = require_user_session(&auth.0, "recovery requires a user session")?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let older_than_seconds = recovery_window_seconds(request.older_than_seconds);
    Span::current().record("older_than_seconds", older_than_seconds);
    let summary = recover_stale_leases(&state.pool, older_than_seconds, actor_id).await?;
    info!(
        %actor_id,
        older_than_seconds,
        ?summary,
        "stale leases recovered"
    );
    Ok(Json(json!({
        "older_than_seconds": older_than_seconds,
        "summary": summary
    })))
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, older_than_seconds = tracing::field::Empty)
)]
async fn recover_stuck_runs_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<RecoveryRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteRuns)?;
    let actor_id = require_user_session(&auth.0, "recovery requires a user session")?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let older_than_seconds = recovery_window_seconds(request.older_than_seconds);
    Span::current().record("older_than_seconds", older_than_seconds);
    let summary = recover_stuck_runs(&state.pool, older_than_seconds, actor_id).await?;
    info!(
        %actor_id,
        older_than_seconds,
        ?summary,
        "stuck runs recovered"
    );
    Ok(Json(json!({
        "older_than_seconds": older_than_seconds,
        "summary": summary
    })))
}

#[derive(Debug, Deserialize, Default)]
struct UnblockJobsRequest {
    /// Optional ILIKE pattern; when set, only failed predecessor jobs
    /// whose `error_message` contains the substring are re-queued.
    /// Useful for unblocking only the post-quota cohort while leaving
    /// genuine code-bug failures pinned.
    #[serde(default)]
    error_substring: Option<String>,
    /// When true (default), also drop every active provider cooldown
    /// so the next claim cycle retries the providers immediately.
    /// Set to false to unblock the queue but keep cooldowns in place
    /// (e.g. operator knows the provider is still rate-limited).
    #[serde(default = "default_true")]
    clear_provider_cooldowns: bool,
}

fn default_true() -> bool {
    true
}

#[tracing::instrument(skip(state, auth, request), fields(user_id = tracing::field::Empty))]
async fn unblock_jobs_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<UnblockJobsRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteRuns)?;
    let actor_id = require_user_session(&auth.0, "unblock requires a user session")?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let summary = archivist_db::unblock_jobs_from_failed_predecessors(
        &state.pool,
        request.error_substring.as_deref(),
    )
    .await?;
    let (cooldowns_cleared, retries_released) = if request.clear_provider_cooldowns {
        let cleared = archivist_db::clear_all_provider_cooldowns(&state.pool).await?;
        // Lifting the cooldowns must also wake the jobs they parked: their
        // `run_after` sits at the (now-irrelevant) cooldown end, so without
        // this the queue would keep waiting it out despite the cooldown being
        // gone (mirrors clear_provider_cooldowns_endpoint). #306
        let released = archivist_db::release_scheduled_retries(&state.pool).await?;
        (cleared, released)
    } else {
        (0, 0)
    };
    info!(
        %actor_id,
        predecessors_requeued = summary.predecessors_requeued,
        runs_unblocked = summary.runs_unblocked,
        cooldowns_cleared,
        retries_released,
        error_substring = ?request.error_substring,
        "operator unblocked queued jobs"
    );
    let _ = archivist_db::append_audit(
        &state.pool,
        archivist_core::AuditEventInput {
            event_type: "operations.jobs_unblocked".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "predecessors_requeued": summary.predecessors_requeued,
                "runs_unblocked": summary.runs_unblocked,
                "cooldowns_cleared": cooldowns_cleared,
                "retries_released": retries_released,
            })),
            metadata: Some(json!({
                "error_substring": request.error_substring,
                "clear_provider_cooldowns": request.clear_provider_cooldowns,
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await;
    Ok(Json(json!({
        "predecessors_requeued": summary.predecessors_requeued,
        "runs_unblocked": summary.runs_unblocked,
        "cooldowns_cleared": cooldowns_cleared,
        "retries_released": retries_released,
    })))
}

async fn provider_cooldowns_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadDashboard)?;
    let cooldowns = archivist_db::list_active_provider_cooldowns(&state.pool).await?;
    let payload = cooldowns
        .into_iter()
        .map(|c| {
            json!({
                "provider_name": c.provider_name,
                "cooldown_until": c.cooldown_until,
                "reason": c.reason,
                "set_at": c.set_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "cooldowns": payload })))
}

#[derive(Debug, Deserialize, Default)]
struct ClearProviderCooldownRequest {
    /// Optional — clear only this provider's cooldown. When None, all
    /// active cooldowns are cleared.
    #[serde(default)]
    provider_name: Option<String>,
}

#[tracing::instrument(skip(state, auth, request), fields(user_id = tracing::field::Empty))]
async fn clear_provider_cooldowns_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<ClearProviderCooldownRequest>,
) -> ApiResult<Json<Value>> {
    // WriteRuns, not WriteSettings: cooldown manipulation is queue/run
    // recovery, and unblock_jobs_endpoint already wipes cooldowns under
    // WriteRuns — requiring more here only forced operators through the
    // unblock detour for the exact same effect (#313).
    require(&auth.0, Permission::WriteRuns)?;
    let actor_id = require_user_session(&auth.0, "clearing cooldowns requires a user session")?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let cleared = match request.provider_name.as_deref() {
        Some(name) => archivist_db::clear_provider_cooldown(&state.pool, name).await?,
        None => archivist_db::clear_all_provider_cooldowns(&state.pool).await?,
    };
    // Lifting a cooldown must also wake the jobs it parked: their `run_after`
    // sits at the (now-irrelevant) cooldown end, so without this the queue
    // would keep waiting it out despite the cooldown being gone. (prod-blocked)
    let released = archivist_db::release_scheduled_retries(&state.pool).await?;
    let _ = archivist_db::append_audit(
        &state.pool,
        archivist_core::AuditEventInput {
            event_type: "ai.provider_cooldown_cleared".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "provider_name": request.provider_name,
                "cleared": cleared,
                "released": released,
            })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await;
    Ok(Json(json!({ "cleared": cleared, "released": released })))
}

/// Wake jobs that a provider cooldown (or other backoff) deferred into the
/// future, so the worker claims them immediately instead of waiting out the
/// cooldown window. Operator-triggered counterpart to the automatic release on
/// a model change; also reachable from the dashboard.
#[tracing::instrument(skip(state, auth), fields(user_id = tracing::field::Empty))]
async fn release_scheduled_retries_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteRuns)?;
    let actor_id = require_user_session(
        &auth.0,
        "releasing scheduled retries requires a user session",
    )?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let released = archivist_db::release_scheduled_retries(&state.pool).await?;
    info!(%actor_id, released, "operator released scheduled job retries");
    let _ = archivist_db::append_audit(
        &state.pool,
        archivist_core::AuditEventInput {
            event_type: "operations.scheduled_retries_released".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "released": released })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await;
    Ok(Json(json!({ "released": released })))
}

#[derive(Debug, Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
}

async fn audit_events(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadAudit)?;
    // Clamp so a caller that only needs a handful of rows (the debug console)
    // doesn't pull the full 200, and a large value can't be requested. (#277)
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    Ok(Json(
        json!({ "items": list_audit_events(&state.pool, limit).await? }),
    ))
}

async fn audit_export(State(state): State<AppState>, auth: Authenticated) -> ApiResult<Response> {
    use bytes::Bytes;
    use futures::TryStreamExt;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    require(&auth.0, Permission::ReadAudit)?;

    // Use a bounded channel so the writer task applies backpressure when
    // the HTTP client (or proxy) is slow: rows accumulate in postgres /
    // sqlx, not in our process. Capacity 16 is plenty for one-CSV-row-
    // at-a-time delivery.
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let pool = state.pool.clone();
    tokio::spawn(async move {
        const HEADER: &str = "id,created_at,event_type,actor_type,actor_id,paperless_document_id,outcome,error_message,metadata,prev_event_hash,event_hash,hash_version,source_ip,user_agent\n";
        if tx
            .send(Ok(Bytes::from_static(HEADER.as_bytes())))
            .await
            .is_err()
        {
            return;
        }
        let mut stream = sqlx::query(
            r#"
            select id, event_type, actor_type, actor_id, paperless_document_id,
                   outcome, error_message, created_at, metadata,
                   prev_event_hash, event_hash, hash_version, source_ip, user_agent
              from audit_events
             order by created_at desc, id desc
            "#,
        )
        .fetch(&pool);

        loop {
            match stream.try_next().await {
                Ok(Some(row)) => match audit_csv_row(&row) {
                    Ok(line) => {
                        if tx.send(Ok(Bytes::from(line))).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx
                            .send(Err(std::io::Error::other(format!(
                                "encode audit row: {error}"
                            ))))
                            .await;
                        break;
                    }
                },
                Ok(None) => break,
                Err(error) => {
                    let _ = tx
                        .send(Err(std::io::Error::other(format!(
                            "stream audit events: {error}"
                        ))))
                        .await;
                    break;
                }
            }
        }
    });

    let body = Body::from_stream(ReceiverStream::new(rx));
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"));
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"paperless-archivist-audit.csv\""),
    );
    Ok(response)
}

fn audit_csv_row(row: &sqlx::postgres::PgRow) -> Result<String, sqlx::Error> {
    let id: Uuid = row.try_get("id")?;
    let event_type: String = row.try_get("event_type")?;
    let actor_type: String = row.try_get("actor_type")?;
    let actor_id: Option<String> = row.try_get("actor_id")?;
    let paperless_document_id: Option<i32> = row.try_get("paperless_document_id")?;
    let outcome: String = row.try_get("outcome")?;
    let error_message: Option<String> = row.try_get("error_message")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let metadata: Option<Value> = row.try_get("metadata")?;
    let prev_event_hash: Option<String> = row.try_get("prev_event_hash")?;
    let event_hash: Option<String> = row.try_get("event_hash")?;
    let hash_version: Option<i16> = row.try_get("hash_version")?;
    let source_ip: Option<String> = row.try_get("source_ip")?;
    let user_agent: Option<String> = row.try_get("user_agent")?;
    let cells = [
        id.to_string(),
        created_at.to_rfc3339(),
        event_type,
        actor_type,
        actor_id.unwrap_or_default(),
        paperless_document_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        outcome,
        error_message.unwrap_or_default(),
        metadata.map(|value| value.to_string()).unwrap_or_default(),
        prev_event_hash.unwrap_or_default(),
        event_hash.unwrap_or_default(),
        hash_version
            .map(|version| version.to_string())
            .unwrap_or_default(),
        source_ip.unwrap_or_default(),
        user_agent.unwrap_or_default(),
    ];
    let mut out = String::with_capacity(256);
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&csv_escape(cell));
    }
    out.push('\n');
    Ok(out)
}

async fn audit_integrity(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadAudit)?;
    Ok(Json(json!(verify_audit_integrity(&state.pool).await?)))
}

async fn apply_audit_retention(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id = require_user_session(&auth.0, "audit retention requires a user session")?;
    let settings = get_runtime_settings(&state.pool).await?;
    Ok(Json(json!(
        apply_security_retention(&state.pool, &settings, actor_id).await?
    )))
}

fn csv_escape(value: &str) -> String {
    // Neutralize spreadsheet formula injection (CWE-1236): a leading
    // = + - @ or tab/CR makes Excel/LibreOffice treat the cell as a formula.
    // Prefix such values with a single quote so they are rendered as text.
    let needs_formula_guard = value
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '=' | '+' | '-' | '@' | '\t' | '\r'));
    let guarded;
    let value = if needs_formula_guard {
        guarded = format!("'{value}");
        guarded.as_str()
    } else {
        value
    };
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

async fn users(State(state): State<AppState>, auth: Authenticated) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    require_user_session(&auth.0, "user management requires a user session")?;
    Ok(Json(json!({ "items": list_users(&state.pool).await? })))
}

#[derive(Debug, Deserialize)]
struct CreateUserRequest {
    username: String,
    email: Option<String>,
    password: String,
    roles: Vec<Role>,
}

async fn create_user(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<CreateUserRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    let actor_id = require_user_session(&auth.0, "user management requires a user session")?;
    validate_password_strength(&request.password).map_err(ApiError::bad_request)?;
    let password_hash = hash_password(&request.password)?;
    let id = create_user_with_roles(
        &state.pool,
        &request.username,
        request.email.as_deref(),
        &password_hash,
        &request.roles,
        Some(actor_id),
    )
    .await?;
    Ok(Json(json!({ "id": id })))
}

async fn enable_user(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    update_user_enabled(&state, &auth, id, true).await
}

async fn disable_user(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    update_user_enabled(&state, &auth, id, false).await
}

async fn update_user_enabled(
    state: &AppState,
    auth: &Authenticated,
    id: Uuid,
    enabled: bool,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    let actor_id = require_user_session(&auth.0, "user management requires a user session")?;
    set_user_enabled(&state.pool, id, enabled, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct UpdateUserRolesRequest {
    roles: Vec<Role>,
}

async fn update_user_roles_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateUserRolesRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    let actor_id = require_user_session(&auth.0, "user management requires a user session")?;
    set_user_roles(&state.pool, id, &request.roles, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct ResetPasswordRequest {
    password: String,
}

async fn reset_user_password(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<ResetPasswordRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    let actor_id = require_user_session(&auth.0, "password reset requires a user session")?;
    validate_password_strength(&request.password).map_err(ApiError::bad_request)?;
    let password_hash = hash_password(&request.password)?;
    update_user_password_hash(
        &state.pool,
        id,
        &password_hash,
        actor_id,
        "user.password_reset",
    )
    .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn api_tokens(State(state): State<AppState>, auth: Authenticated) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    require_user_session(&auth.0, "API token management requires a user session")?;
    Ok(Json(
        json!({ "items": archivist_db::list_api_tokens(&state.pool).await? }),
    ))
}

#[derive(Debug, Deserialize)]
struct CreateApiTokenRequest {
    name: String,
    scopes: Vec<String>,
    expires_in_days: Option<i64>,
}

async fn create_api_token(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<CreateApiTokenRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    let actor_id = require_user_session(&auth.0, "API token creation requires a user session")?;
    validate_api_token_name(&request.name)?;
    validate_api_token_scopes(&request.scopes)?;
    let settings = get_runtime_settings(&state.pool).await?;
    let expires_at = api_token_expiry(&settings, request.expires_in_days)?;
    let token = format!("pa_{}", random_token());
    let token_hash = hash_token(&token);
    let id = archivist_db::create_api_token(
        &state.pool,
        &request.name,
        &token_hash,
        &request.scopes,
        actor_id,
        expires_at,
    )
    .await?;
    Ok(Json(
        json!({ "id": id, "token": token, "expires_at": expires_at }),
    ))
}

#[derive(Debug, Deserialize)]
struct RotateApiTokenRequest {
    expires_in_days: Option<i64>,
}

async fn rotate_api_token_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<RotateApiTokenRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    let actor_id = require_user_session(&auth.0, "API token rotation requires a user session")?;
    let settings = get_runtime_settings(&state.pool).await?;
    let expires_at = api_token_expiry(&settings, request.expires_in_days)?;
    let token = format!("pa_{}", random_token());
    let token_hash = hash_token(&token);
    let new_id = rotate_api_token(&state.pool, id, &token_hash, actor_id, expires_at).await?;
    Ok(Json(
        json!({ "id": new_id, "token": token, "expires_at": expires_at }),
    ))
}

async fn revoke_api_token(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    let actor_id = require_user_session(&auth.0, "API token revocation requires a user session")?;
    archivist_db::revoke_api_token(&state.pool, id, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[tracing::instrument(
    skip(state),
    fields(
        review_id = %review_id,
        user_id = %actor_id,
        run_id = tracing::field::Empty,
        paperless_document_id = tracing::field::Empty
    )
)]
async fn apply_review_patch(state: &AppState, review_id: Uuid, actor_id: Uuid) -> Result<()> {
    let Some(review) = archivist_db::claim_review_for_apply(&state.pool, review_id).await? else {
        return Ok(());
    };
    // review.status holds the pre-claim status ('approved'/'edited'). The row
    // is now 'applying', which fences out a concurrent apply / autopilot
    // drain. Only failures that never produced a nonterminal durable intent
    // may be reverted immediately; ambiguous HTTP outcomes stay fenced for
    // the recovery worker instead of becoming blindly retryable.
    let prior_status = review.status.clone();
    let result = apply_claimed_review(state, &review, actor_id).await;
    if let Err(error) = &result
        && let Some(conflict) = error.downcast_ref::<ReviewApplyConflict>()
    {
        archivist_db::mark_review_apply_conflict(
            &state.pool,
            review_id,
            "pending",
            conflict.fields(),
            "user",
            Some(actor_id.to_string()),
        )
        .await?;
        return result;
    }
    if result.is_err() {
        match archivist_db::review_has_nonterminal_apply_intent(&state.pool, review_id).await {
            Ok(false) => {
                let _ = archivist_db::revert_review_from_applying(
                    &state.pool,
                    review_id,
                    &prior_status,
                )
                .await;
            }
            Ok(true) => warn!(
                %review_id,
                "review apply remains fenced while its Paperless intent is recovered"
            ),
            Err(error) => warn!(
                %review_id,
                error = %error,
                "could not prove review apply safe to revert; leaving it fenced"
            ),
        }
    }
    result
}

async fn apply_claimed_review(
    state: &AppState,
    review: &archivist_db::ReviewItemRecord,
    actor_id: Uuid,
) -> Result<()> {
    let review_id = review.id;
    if let Some(run_id) = review.run_id {
        Span::current().record("run_id", tracing::field::display(run_id));
    }
    Span::current().record("paperless_document_id", review.paperless_document_id);
    let patch_value = review
        .edited_patch
        .clone()
        .unwrap_or_else(|| review.suggested_patch.clone());
    let patch: DocumentPatch = serde_json::from_value(patch_value)?;
    // run_id is None only for review items whose run was pruned by retention
    // (terminal runs only — a pending review keeps its run alive).
    let final_run_stage = if let (Some(run_id), Some(job_id)) = (review.run_id, review.job_id) {
        archivist_db::is_last_active_job(&state.pool, run_id, job_id).await?
    } else {
        false
    };
    let settings = get_runtime_settings(&state.pool).await?;
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    let tag_operations =
        review_workflow_tag_operations(&client, &settings, review.stage, final_run_stage).await?;
    let apply_started = std::time::Instant::now();
    let execution = apply_document(
        &state.pool,
        &client,
        ApplyRequest {
            source: "human_review".to_owned(),
            source_key: format!("review:{review_id}"),
            owner_type: "user".to_owned(),
            owner_id: actor_id.to_string(),
            paperless_document_id: review.paperless_document_id,
            run_id: review.run_id,
            job_id: review.job_id,
            review_id: Some(review.id),
            patch,
            before: None,
            metadata: json!({
                "stage": review.stage,
                "review_id": review.id
            }),
            review_revert_status: Some(review.status.clone()),
            review_precondition: Some(ReviewApplyPrecondition {
                baseline: review.baseline.clone(),
                tag_operations,
            }),
            allow_custom_fields_fallback: false,
        },
    )
    .await?;
    let duration_ms = apply_started.elapsed().as_millis() as u64;
    archivist_db::mark_review_applied(&state.pool, review_id, actor_id).await?;
    archivist_db::finalize_apply_intent(&state.pool, execution.attempt_id()).await?;
    info!(
        %review_id,
        run_id = ?review.run_id,
        paperless_document_id = review.paperless_document_id,
        duration_ms,
        "review patch applied to Paperless"
    );
    Ok(())
}

async fn review_workflow_tag_operations(
    client: &PaperlessClient,
    settings: &RuntimeSettings,
    stage: Stage,
    final_run_stage: bool,
) -> Result<ReviewTagOperations> {
    let all_tags = client.list_tags().await?;
    let completion = settings.workflow.tags.completion_tag_for_stage(stage);
    let trigger = settings.workflow.tags.trigger_tag_for_stage(stage);
    let mut additions = Vec::new();
    let mut removals = Vec::new();
    if let Some(completion_name) = completion {
        let tag = client.ensure_tag(completion_name).await?;
        additions.push(tag.id);
    }
    if final_run_stage {
        let tag = client
            .ensure_tag(&settings.workflow.tags.completion_processed)
            .await?;
        additions.push(tag.id);
    }
    if let Some(trigger_name) = trigger
        && let Some(tag) = all_tags
            .iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(trigger_name))
    {
        removals.push(tag.id);
    }
    if final_run_stage
        && let Some(tag) = all_tags.iter().find(|tag| {
            tag.name
                .eq_ignore_ascii_case(&settings.workflow.tags.trigger_process)
        })
    {
        removals.push(tag.id);
    }
    additions.sort_unstable();
    additions.dedup();
    removals.sort_unstable();
    removals.dedup();
    Ok(ReviewTagOperations {
        additions,
        removals,
    })
}

async fn sync_paperless_inventory(
    pool: &DbPool,
    client: &PaperlessClient,
    settings: &RuntimeSettings,
) -> Result<Value> {
    let archive_name = settings.paperless.active_archive.clone();
    let sync_started_at = Utc::now();
    let mut tags = client.list_tags().await?;
    // Only hit `ensure_tag` for workflow tags that are genuinely absent. Each
    // `ensure_tag` re-fetches the entire Paperless tag catalog, so calling it
    // unconditionally per workflow tag was O(workflow_tags × all_tags) — with a
    // few thousand tags this added minutes to every sync. The catalog is already
    // in `tags`; match it the same case-insensitive way `ensure_tag` does.
    for workflow_tag in settings.workflow.tags.all() {
        if !tags
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(workflow_tag))
        {
            tags.push(client.ensure_tag(workflow_tag).await?);
        }
    }
    let cursor = paperless_sync_cursor(pool, &archive_name).await?;
    let delta_cursor = cursor
        .map(|cursor| cursor - Duration::minutes(settings.paperless.delta_sync_overlap_minutes));
    // These four catalog fetches are independent GETs against Paperless; run
    // them concurrently rather than serially. The tag list above must stay
    // sequential because the workflow-tag loop mutates it in place. custom_fields
    // keeps its best-effort `unwrap_or_default` semantics inside the join.
    let (correspondents, document_types, custom_fields, (sync_mode, documents)) = tokio::try_join!(
        client.list_correspondents(),
        client.list_document_types(),
        async { anyhow::Ok(client.list_custom_fields().await.unwrap_or_default()) },
        async {
            if settings.paperless.delta_sync_enabled {
                if let Some(cursor) = delta_cursor {
                    match client
                        .list_documents_modified_since(&cursor.to_rfc3339())
                        .await
                    {
                        Ok(documents) => anyhow::Ok(("delta", documents)),
                        Err(_) => {
                            anyhow::Ok(("full_after_delta_error", client.list_documents().await?))
                        }
                    }
                } else {
                    anyhow::Ok(("full_initial", client.list_documents().await?))
                }
            } else {
                anyhow::Ok(("full", client.list_documents().await?))
            }
        },
    )?;

    let mut tx = pool.begin().await?;
    for tag in &tags {
        upsert_paperless_tag(
            &mut tx,
            tag.id,
            &tag.name,
            tag.slug.as_deref(),
            tag.color.as_deref(),
            settings.workflow.tags.is_workflow_tag(&tag.name),
        )
        .await?;
    }
    for entity in &correspondents {
        upsert_paperless_named_entity(&mut tx, "paperless_correspondents", entity.id, &entity.name)
            .await?;
    }
    for entity in &document_types {
        upsert_paperless_named_entity(&mut tx, "paperless_document_types", entity.id, &entity.name)
            .await?;
    }
    for field in &custom_fields {
        upsert_paperless_custom_field(&mut tx, field.id, &field.name, field.data_type.as_deref())
            .await?;
    }

    // O(1) id→name lookups: building this map once avoids the previous
    // O(documents × tags) nested linear scan, which was pure CPU burned inside
    // the sync transaction on instances with many tags/documents.
    let tag_names_by_id: HashMap<i32, &str> =
        tags.iter().map(|tag| (tag.id, tag.name.as_str())).collect();
    for document in &documents {
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tag_names_by_id.get(id).copied())
            .map(|name| name.to_owned())
            .collect::<Vec<_>>();
        upsert_inventory_item(
            &mut tx,
            &archivist_db::InventoryUpsert {
                paperless_document_id: document.id,
                title: document.title.clone(),
                original_file_name: document.original_file_name.clone(),
                current_tags: tag_names.clone(),
                current_tag_ids: document.tags.clone(),
                correspondent_id: document.correspondent,
                document_type_id: document.document_type,
                document_date: archivist_db::parse_paperless_document_date(
                    document.created.as_deref(),
                ),
                paperless_modified_at: archivist_db::parse_paperless_modified_at(
                    document.modified.as_deref(),
                ),
                has_ocr_completion_tag: tag_names
                    .iter()
                    .any(|tag| tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_ocr)),
                has_tagging_completion_tag: tag_names.iter().any(|tag| {
                    tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_tagging)
                }),
                has_full_completion_tag: tag_names.iter().any(|tag| {
                    tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_processed)
                }),
            },
        )
        .await?;
    }
    update_paperless_sync_cursor(&mut tx, &archive_name, sync_mode, sync_started_at).await?;
    tx.commit().await?;

    Ok(json!({
        "archive": archive_name,
        "mode": sync_mode,
        "delta_cursor": delta_cursor.map(|cursor| cursor.to_rfc3339()),
        "tags": tags.len(),
        "correspondents": correspondents.len(),
        "document_types": document_types.len(),
        "custom_fields": custom_fields.len(),
        "documents": documents.len()
    }))
}

async fn paperless_client_from_settings(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
) -> Result<PaperlessClient> {
    let active_profile = settings.paperless.archive_profiles.iter().find(|profile| {
        profile.enabled
            && profile
                .name
                .eq_ignore_ascii_case(&settings.paperless.active_archive)
    });
    let base_url = active_profile
        .map(|profile| profile.base_url.as_str())
        .unwrap_or(&settings.paperless.base_url);
    let secret_id = active_profile
        .and_then(|profile| profile.token_secret_id)
        .or(settings.paperless.token_secret_id)
        .ok_or_else(|| anyhow!("Paperless token is not configured"))?;
    let token = resolve_secret(pool, &config.secret_key, secret_id)
        .await?
        .ok_or_else(|| anyhow!("Paperless token secret reference does not exist"))?;
    PaperlessClient::new(base_url, token, settings.paperless.timeout_seconds)
}

async fn issue_session(state: &AppState, user_id: Uuid) -> ApiResult<(String, String)> {
    let session_token = random_token();
    let csrf_token = random_token();
    let session_hash = hash_token(&session_token);
    let csrf_hash = hash_token(&csrf_token);
    let expires_at = Utc::now() + Duration::hours(state.config.session_ttl_hours);
    create_session(&state.pool, user_id, &session_hash, &csrf_hash, expires_at).await?;
    Ok((session_token, csrf_token))
}

fn set_session_cookies(
    headers: &mut HeaderMap,
    config: &AppConfig,
    session_token: &str,
    csrf_token: &str,
) -> Result<(), ApiError> {
    let session_cookie = build_cookie(
        SESSION_COOKIE,
        session_token,
        true,
        config.cookie_secure,
        config.session_ttl_hours,
    );
    let csrf_cookie = build_cookie(
        CSRF_COOKIE,
        csrf_token,
        false,
        config.cookie_secure,
        config.session_ttl_hours,
    );
    headers.append(header::SET_COOKIE, header_value(session_cookie)?);
    headers.append(header::SET_COOKIE, header_value(csrf_cookie)?);
    Ok(())
}

fn oidc_values(config: &AppConfig) -> ApiResult<OidcValues<'_>> {
    if !config.oidc_enabled {
        return Err(ApiError::bad_request("OIDC is not enabled"));
    }
    Ok(OidcValues {
        issuer_url: config
            .oidc_issuer_url
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::internal("OIDC issuer URL is not configured"))?,
        client_id: config
            .oidc_client_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::internal("OIDC client ID is not configured"))?,
        client_secret: config
            .oidc_client_secret
            .as_ref()
            .filter(|value| !value.expose_secret().is_empty())
            .ok_or_else(|| ApiError::internal("OIDC client secret is not configured"))?,
        redirect_uri: config
            .oidc_redirect_uri
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::internal("OIDC redirect URI is not configured"))?,
    })
}

fn oidc_http_client() -> ApiResult<HttpClient> {
    HttpClient::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ApiError::internal(format!("build OIDC HTTP client: {error}")))
}

async fn oidc_discover(
    http_client: &HttpClient,
    issuer_url: &str,
) -> ApiResult<OidcProviderMetadata> {
    let issuer = Url::parse(issuer_url)
        .map_err(|error| ApiError::internal(format!("invalid OIDC issuer URL: {error}")))?;
    if issuer.scheme() != "https" && issuer.host_str() != Some("localhost") {
        return Err(ApiError::internal("OIDC issuer URL must use HTTPS"));
    }
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );
    // Log upstream detail (issuer URL, connection info) server-side but return
    // a generic message — these endpoints are publicly reachable. #291
    let metadata = http_client
        .get(&discovery_url)
        .send()
        .await
        .map_err(|error| {
            tracing::error!(%error, "OIDC discovery request failed");
            ApiError::internal("OIDC discovery failed")
        })?
        .error_for_status()
        .map_err(|error| {
            tracing::error!(%error, "OIDC discovery returned an error status");
            ApiError::internal("OIDC discovery failed")
        })?
        .json::<OidcProviderMetadata>()
        .await
        .map_err(|error| {
            tracing::error!(%error, "OIDC discovery parse failed");
            ApiError::internal("OIDC discovery failed")
        })?;
    if metadata.issuer.trim_end_matches('/') != issuer_url.trim_end_matches('/') {
        return Err(ApiError::unauthorized("OIDC issuer mismatch"));
    }
    Ok(metadata)
}

fn oidc_authorization_url(
    metadata: &OidcProviderMetadata,
    values: &OidcValues<'_>,
    scopes: &[String],
    csrf_state: &str,
    nonce: &str,
    code_challenge: &str,
) -> ApiResult<String> {
    let mut url = Url::parse(&metadata.authorization_endpoint).map_err(|error| {
        ApiError::internal(format!("invalid OIDC authorization endpoint: {error}"))
    })?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", values.client_id)
        .append_pair("redirect_uri", values.redirect_uri)
        .append_pair("scope", &scopes.join(" "))
        .append_pair("state", csrf_state)
        .append_pair("nonce", nonce)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

async fn oidc_exchange_code(
    http_client: &HttpClient,
    metadata: &OidcProviderMetadata,
    values: &OidcValues<'_>,
    code: &str,
    pkce_verifier: &str,
) -> ApiResult<OidcTokenResponse> {
    let client_secret = values.client_secret.expose_secret();
    http_client
        .post(&metadata.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", values.redirect_uri),
            ("client_id", values.client_id),
            ("client_secret", client_secret),
            ("code_verifier", pkce_verifier),
        ])
        .send()
        .await
        .map_err(|error| {
            tracing::error!(%error, "OIDC code exchange request failed");
            ApiError::unauthorized("OIDC code exchange failed")
        })?
        .error_for_status()
        .map_err(|error| {
            tracing::error!(%error, "OIDC code exchange returned an error status");
            ApiError::unauthorized("OIDC code exchange failed")
        })?
        .json::<OidcTokenResponse>()
        .await
        .map_err(|error| {
            tracing::error!(%error, "OIDC token response parse failed");
            ApiError::unauthorized("OIDC code exchange failed")
        })
}

async fn oidc_verify_id_token(
    http_client: &HttpClient,
    metadata: &OidcProviderMetadata,
    values: &OidcValues<'_>,
    id_token: &str,
    expected_nonce: &str,
) -> ApiResult<OidcIdClaims> {
    let header = decode_header(id_token)
        .map_err(|error| ApiError::unauthorized(format!("OIDC ID token header error: {error}")))?;
    if matches!(
        header.alg,
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
    ) {
        return Err(ApiError::unauthorized(
            "OIDC ID token uses unsupported symmetric signing",
        ));
    }
    if !metadata.id_token_signing_alg_values_supported.is_empty()
        && !metadata
            .id_token_signing_alg_values_supported
            .iter()
            .any(|algorithm| algorithm == oidc_algorithm_name(header.alg))
    {
        return Err(ApiError::unauthorized(
            "OIDC ID token algorithm is not supported by issuer metadata",
        ));
    }

    let kid = header
        .kid
        .as_deref()
        .ok_or_else(|| ApiError::unauthorized("OIDC ID token has no key id"))?;
    let jwks = http_client
        .get(&metadata.jwks_uri)
        .send()
        .await
        .map_err(|error| ApiError::unauthorized(format!("OIDC JWKS request failed: {error}")))?
        .error_for_status()
        .map_err(|error| ApiError::unauthorized(format!("OIDC JWKS request failed: {error}")))?
        .json::<JwkSet>()
        .await
        .map_err(|error| ApiError::unauthorized(format!("OIDC JWKS parse failed: {error}")))?;
    let jwk = jwks
        .find(kid)
        .ok_or_else(|| ApiError::unauthorized("OIDC signing key not found"))?;
    let decoding_key = DecodingKey::from_jwk(jwk)
        .map_err(|error| ApiError::unauthorized(format!("OIDC signing key rejected: {error}")))?;
    let mut validation = Validation::new(header.alg);
    validation.set_audience(&[values.client_id]);
    validation.set_issuer(&[metadata.issuer.as_str()]);
    validation.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);

    let claims = decode::<OidcIdClaims>(id_token, &decoding_key, &validation)
        .map_err(|error| {
            ApiError::unauthorized(format!("OIDC ID token validation failed: {error}"))
        })?
        .claims;
    if claims.nonce.as_deref() != Some(expected_nonce) {
        return Err(ApiError::unauthorized("OIDC nonce mismatch"));
    }
    Ok(claims)
}

/// Fetch the IdP userinfo endpoint with the access token and return its claims.
///
/// Verifies the userinfo `sub` equals the ID token `sub` (OIDC Core §5.3.2) so a
/// swapped or forged response cannot inject another user's identity or roles.
/// Best-effort: any transport/parse error or sub mismatch yields `None`, and the
/// caller proceeds on the signed ID token alone.
async fn oidc_fetch_userinfo(
    http_client: &HttpClient,
    userinfo_endpoint: &str,
    access_token: &str,
    expected_sub: &str,
) -> Option<serde_json::Map<String, Value>> {
    let response = http_client
        .get(userinfo_endpoint)
        .bearer_auth(access_token)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let claims = response
        .json::<serde_json::Map<String, Value>>()
        .await
        .ok()?;
    if claims.get("sub").and_then(Value::as_str) != Some(expected_sub) {
        warn!("OIDC userinfo sub did not match the ID token sub; ignoring userinfo");
        return None;
    }
    Some(claims)
}

fn oidc_access_token_hash_matches(alg: Algorithm, access_token: &str, expected_hash: &str) -> bool {
    let digest = match alg {
        Algorithm::RS256 | Algorithm::PS256 | Algorithm::ES256 => {
            Sha256::digest(access_token.as_bytes()).to_vec()
        }
        Algorithm::RS384 | Algorithm::PS384 | Algorithm::ES384 => {
            Sha384::digest(access_token.as_bytes()).to_vec()
        }
        Algorithm::RS512 | Algorithm::PS512 | Algorithm::EdDSA => {
            Sha512::digest(access_token.as_bytes()).to_vec()
        }
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => return false,
    };
    URL_SAFE_NO_PAD.encode(&digest[..digest.len() / 2]) == expected_hash
}

fn oidc_algorithm_name(alg: Algorithm) -> &'static str {
    match alg {
        Algorithm::HS256 => "HS256",
        Algorithm::HS384 => "HS384",
        Algorithm::HS512 => "HS512",
        Algorithm::ES256 => "ES256",
        Algorithm::ES384 => "ES384",
        Algorithm::RS256 => "RS256",
        Algorithm::RS384 => "RS384",
        Algorithm::RS512 => "RS512",
        Algorithm::PS256 => "PS256",
        Algorithm::PS384 => "PS384",
        Algorithm::PS512 => "PS512",
        Algorithm::EdDSA => "EdDSA",
    }
}

fn oidc_scopes(config: &AppConfig) -> Vec<String> {
    let mut scopes = config
        .oidc_scopes
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if !scopes.iter().any(|scope| scope == "openid") {
        scopes.insert(0, "openid".to_owned());
    }
    scopes
}

fn safe_return_to(value: Option<&str>) -> Option<String> {
    let value = value.unwrap_or("/");
    // Reject backslashes too: browsers normalize `\` to `/` per the WHATWG URL
    // spec, so `/\evil.com` becomes the protocol-relative `//evil.com` and
    // redirects off-origin (CWE-601). #271
    if value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains('\\')
        && !value.contains('\r')
        && !value.contains('\n')
    {
        Some(value.to_owned())
    } else {
        Some("/".to_owned())
    }
}

/// The email claim, but only when the IdP asserted `email_verified=true` —
/// the only condition under which OIDC permits using it for authorization.
fn oidc_verified_email(claims: &OidcIdClaims) -> Option<&str> {
    claims
        .email
        .as_deref()
        .filter(|_| claims.email_verified == Some(true))
}

/// An ID token is "degraded" when it carries neither a usable
/// `preferred_username` nor a verified email (#299): the derived username
/// then falls back to the raw subject, and a recomputed role set would be
/// based on identities the operator never allowlisted. Such tokens must not
/// drive role demotions for returning users.
fn oidc_claims_degraded(claims: &OidcIdClaims) -> bool {
    claims
        .preferred_username
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
        && oidc_verified_email(claims).is_none()
}

fn oidc_username(value: &str) -> String {
    let mut username = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '@') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    while username.contains("--") {
        username = username.replace("--", "-");
    }
    let username = username
        .trim_matches(|ch| matches!(ch, '.' | '_' | '-' | '@'))
        .chars()
        .take(80)
        .collect::<String>();
    if username.is_empty() {
        "oidc-user".to_owned()
    } else {
        username
    }
}

/// Outcome of resolving an OIDC login's roles.
struct OidcRoleResolution {
    roles: Vec<Role>,
    /// True when the roles are authoritative for this login: the IdP asserted a
    /// roles claim, or the subject is on the admin allowlist. When false the
    /// roles are a fallback (the configured defaults) and a degraded token must
    /// not be allowed to demote a returning user (#299).
    authoritative: bool,
    /// True when the ID token carried a recognizable roles claim at all
    /// (independent of whether any role mapped). Drives the diagnostic that
    /// tells an operator their IdP is not asserting roles into the ID token.
    idp_claim_present: bool,
}

/// Resolve the effective roles for an OIDC login. Precedence:
///
/// 1. The admin allowlist (`ARCHIVIST_OIDC_ADMIN_USERS`) is a break-glass that
///    always grants full admin — it keeps working even when the IdP stops
///    asserting roles, and matches the immutable `sub` (#299).
/// 2. Roles the IdP asserted in the configured roles claim, mapped through
///    `ARCHIVIST_OIDC_ROLE_MAPPINGS`.
///
/// Their union is authoritative and replaces the stored roles on every login
/// (#289). Only when neither source yields a role do the configured default
/// roles apply, and that fallback is *not* authoritative.
fn oidc_roles(
    config: &AppConfig,
    claims: &OidcIdClaims,
    subject: &str,
    username: &str,
    email: Option<&str>,
) -> ApiResult<OidcRoleResolution> {
    let mut roles: Vec<Role> = Vec::new();

    let allowlisted = oidc_is_admin(config, subject, username, email);
    if allowlisted {
        roles.extend([Role::Admin, Role::Operator, Role::Reviewer, Role::Auditor]);
    }

    let idp_roles = oidc_idp_roles(config, claims);
    let idp_claim_present = idp_roles.is_some();
    for role in idp_roles.into_iter().flatten() {
        if !roles.contains(&role) {
            roles.push(role);
        }
    }

    let authoritative = allowlisted || idp_claim_present;
    if roles.is_empty() {
        roles = parse_oidc_roles(&config.oidc_default_roles)
            .map_err(|error| ApiError::internal(format!("invalid OIDC role config: {error}")))?;
    }

    Ok(OidcRoleResolution {
        roles,
        authoritative,
        idp_claim_present,
    })
}

/// Read and map the roles the IdP asserted in the ID token.
///
/// Returns `None` when the token carries no recognizable roles claim — the
/// caller treats that as "the IdP did not assert roles" and falls back without
/// demoting a returning user. Returns `Some(roles)` when a roles claim is
/// present; the vec may be empty if none of the asserted roles map to an app
/// role. Unmapped IdP roles are dropped, so the IdP can never grant a role the
/// operator did not explicitly map (no privilege escalation). #299.
fn oidc_idp_roles(config: &AppConfig, claims: &OidcIdClaims) -> Option<Vec<Role>> {
    let value = oidc_roles_claim_value(config, claims)?;
    let mappings = parse_oidc_role_mappings(&config.oidc_role_mappings);
    let mut roles = Vec::new();
    for raw in extract_role_strings(value) {
        let key = raw.trim().to_ascii_lowercase();
        if let Some(role) = mappings.get(key.as_str())
            && !roles.contains(role)
        {
            roles.push(role.clone());
        }
    }
    Some(roles)
}

/// Locate the roles claim value in the token: the operator-configured claim
/// name first, then the well-known ZITADEL project-roles claims — the generic
/// `urn:zitadel:iam:org:project:roles` and any project-scoped
/// `urn:zitadel:iam:org:project:<projectid>:roles`. This makes the default work
/// whether the deployment surfaces roles in the generic or the scoped claim.
fn oidc_roles_claim_value<'a>(config: &AppConfig, claims: &'a OidcIdClaims) -> Option<&'a Value> {
    let configured = config.oidc_roles_claim.trim();
    if !configured.is_empty()
        && let Some(value) = claims.additional.get(configured)
    {
        return Some(value);
    }
    if let Some(value) = claims.additional.get("urn:zitadel:iam:org:project:roles") {
        return Some(value);
    }
    claims
        .additional
        .iter()
        .find(|(key, _)| key.starts_with("urn:zitadel:iam:org:project:") && key.ends_with(":roles"))
        .map(|(_, value)| value)
}

/// Pull the raw role names out of a roles claim value, accepting the three
/// shapes IdPs use: a ZITADEL-style object (the KEYS are the granted roles), a
/// JSON array of strings, or a single space/comma-delimited string.
fn extract_role_strings(value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) => map.keys().cloned().collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Value::String(text) => text
            .split([',', ' ', '\n', '\t'])
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse `ARCHIVIST_OIDC_ROLE_MAPPINGS` (`idp-role=app-role,…`) into a lookup
/// keyed by the lowercased IdP role. Malformed entries (no `=`, empty side, or
/// an unknown app role) are skipped rather than failing the login.
fn parse_oidc_role_mappings(value: &str) -> HashMap<String, Role> {
    let mut map = HashMap::new();
    for entry in value
        .split([',', '\n', '\t'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let Some((idp, app)) = entry.split_once('=') else {
            continue;
        };
        let idp = idp.trim().to_ascii_lowercase();
        if idp.is_empty() {
            continue;
        }
        if let Ok(role) = app.trim().to_ascii_lowercase().parse::<Role>() {
            map.insert(idp, role);
        }
    }
    map
}

// NOTE (#291): the `username` matched here is derived from the IdP's
// `preferred_username` claim. The email path is gated on `email_verified`, but
// username matching trusts the IdP to keep `preferred_username` unique and
// non-user-settable (true for ZITADEL). For an IdP that lets users choose it,
// prefer allowlisting by verified email or the immutable `sub` (#299).
fn oidc_is_admin(config: &AppConfig, subject: &str, username: &str, email: Option<&str>) -> bool {
    let username = username.to_ascii_lowercase();
    let email = email.map(str::to_ascii_lowercase);
    config
        .oidc_admin_users
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .any(|admin| {
            // The immutable subject is matched verbatim (`sub` is
            // case-sensitive per OIDC Core §2), so the allowlist keeps
            // working even when the ID token carries no preferred_username
            // and no verified email (#299).
            if admin == subject {
                return true;
            }
            let admin = admin.to_ascii_lowercase();
            admin == username
                || email
                    .as_deref()
                    .is_some_and(|email_value| admin == email_value)
        })
}

fn parse_oidc_roles(value: &str) -> Result<Vec<Role>> {
    let mut roles = Vec::new();
    for token in value
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        let role = token
            .parse::<Role>()
            .map_err(|error| anyhow!("invalid role {token}: {error}"))?;
        if !roles.contains(&role) {
            roles.push(role);
        }
    }
    if roles.is_empty() {
        roles.push(Role::Viewer);
    }
    Ok(roles)
}

async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let auth = authenticate(&state.pool, request.headers()).await?;
    enforce_csrf(&auth, request.method(), request.headers())?;
    request.extensions_mut().insert(auth);
    Ok(next.run(request).await)
}

async fn authenticate(pool: &DbPool, headers: &HeaderMap) -> Result<AuthContext, ApiError> {
    if let Some(token) = bearer_token(headers) {
        let token_hash = hash_token(token);
        if let Some(principal) = find_api_token(pool, &token_hash).await? {
            return Ok(AuthContext {
                actor_type: "api_token".to_owned(),
                actor_id: Some(principal.name),
                user_id: principal.user_id,
                username: None,
                roles: Vec::new(),
                scopes: principal.scopes,
                session_id: None,
                csrf_secret_hash: None,
                cookie_auth: false,
            });
        }
    }

    let Some(session_token) = cookie_value(headers, SESSION_COOKIE) else {
        return Err(ApiError::unauthorized("authentication required"));
    };
    let session_hash = hash_token(&session_token);
    let Some(session) = find_session(pool, &session_hash).await? else {
        return Err(ApiError::unauthorized("invalid or expired session"));
    };
    Ok(AuthContext {
        actor_type: "user".to_owned(),
        actor_id: Some(session.user_id.to_string()),
        user_id: Some(session.user_id),
        username: Some(session.username),
        roles: session.roles,
        scopes: Vec::new(),
        session_id: Some(session.session_id),
        csrf_secret_hash: Some(session.csrf_secret_hash),
        cookie_auth: true,
    })
}

fn enforce_csrf(auth: &AuthContext, method: &Method, headers: &HeaderMap) -> Result<(), ApiError> {
    if !auth.cookie_auth || matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(());
    }
    let provided = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("missing CSRF token"))?;
    let provided_hash = hash_token(provided);
    let expected = auth
        .csrf_secret_hash
        .as_deref()
        .ok_or_else(|| ApiError::forbidden("invalid CSRF token"))?;
    // Constant-time compare to deny timing oracles on the hex hash.
    if expected.len() != provided_hash.len()
        || !bool::from(expected.as_bytes().ct_eq(provided_hash.as_bytes()))
    {
        return Err(ApiError::forbidden("invalid CSRF token"));
    }
    Ok(())
}

fn require(auth: &AuthContext, permission: Permission) -> Result<(), ApiError> {
    if roles_have_permission(&auth.roles, permission)
        || auth
            .scopes
            .iter()
            .any(|scope| scope == scope_for_permission(permission))
    {
        Ok(())
    } else {
        Err(ApiError::forbidden("insufficient permissions"))
    }
}

fn require_user_session(auth: &AuthContext, message: &'static str) -> Result<Uuid, ApiError> {
    if !auth.cookie_auth {
        return Err(ApiError::forbidden(message));
    }
    auth.user_id.ok_or_else(|| ApiError::forbidden(message))
}

fn scope_for_permission(permission: Permission) -> &'static str {
    match permission {
        Permission::ReadDashboard | Permission::ReadRuns => "runs:read",
        Permission::WriteRuns => "runs:write",
        Permission::ReadInventory => "inventory:read",
        Permission::WriteBatches => "batches:write",
        Permission::UseChat => "chat:write",
        Permission::ReadReviews => "reviews:read",
        Permission::WriteReviews => "reviews:write",
        Permission::ReadSettings => "settings:read",
        Permission::WriteSettings => "settings:write",
        Permission::ManageUsers => "users:manage",
        Permission::ReadAudit => "audit:read",
    }
}

fn validate_api_token_name(name: &str) -> Result<(), ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 80 {
        return Err(ApiError::bad_request(
            "API token name must be between 1 and 80 characters",
        ));
    }
    Ok(())
}

fn api_token_expiry(
    settings: &RuntimeSettings,
    requested_days: Option<i64>,
) -> Result<Option<chrono::DateTime<Utc>>, ApiError> {
    let security = settings.clone().normalized().security;
    let days = requested_days.unwrap_or(security.api_token_default_ttl_days);
    if days <= 0 {
        if security.api_token_expiry_required {
            return Err(ApiError::bad_request(
                "API token expiry is required by security policy",
            ));
        }
        return Ok(None);
    }
    if days > security.api_token_max_ttl_days {
        return Err(ApiError::bad_request(format!(
            "API token expiry exceeds maximum of {} days",
            security.api_token_max_ttl_days
        )));
    }
    Ok(Some(Utc::now() + Duration::days(days)))
}

fn validate_api_token_scopes(scopes: &[String]) -> Result<(), ApiError> {
    const ALLOWED: &[&str] = &[
        "runs:read",
        "runs:write",
        "inventory:read",
        "batches:write",
        "chat:write",
        "reviews:read",
        "reviews:write",
        "settings:read",
        "settings:write",
        "users:manage",
        "audit:read",
    ];
    if scopes.is_empty() {
        return Err(ApiError::bad_request(
            "API token requires at least one scope",
        ));
    }
    for scope in scopes {
        if !ALLOWED.contains(&scope.as_str()) {
            return Err(ApiError::bad_request(format!(
                "unsupported API token scope: {scope}"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Authenticated(AuthContext);

impl<S> axum::extract::FromRequestParts<S> for Authenticated
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthContext>()
            .cloned()
            .map(Self)
            .ok_or_else(|| ApiError::unauthorized("authentication required"))
    }
}

fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let params =
        Params::new(19_456, 2, 1, None).map_err(|error| anyhow!("argon2 params: {error}"))?;
    let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    Ok(argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow!("hash password: {error}"))?
        .to_string())
}

fn validate_password_strength(password: &str) -> std::result::Result<(), &'static str> {
    if password.chars().count() < 12 {
        return Err("password must be at least 12 characters");
    }
    if password.chars().all(char::is_whitespace) {
        return Err("password must not be blank");
    }
    Ok(())
}

fn verify_password(user: &AuthUser, password: &str) -> Result<bool> {
    let parsed = PasswordHash::new(&user.password_hash)
        .map_err(|error| anyhow!("parse password hash: {error}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// A real Argon2id hash computed once at first use. Verifying a candidate
/// password against this when the supplied username does not exist makes the
/// login path spend the same ~Argon2id time it would for a real account,
/// closing the timing side channel that otherwise enumerates valid usernames.
static DUMMY_PASSWORD_HASH: std::sync::LazyLock<Option<String>> =
    std::sync::LazyLock::new(|| hash_password("paperless-archivist-dummy-password").ok());

/// Perform a throwaway Argon2id verification to equalize login timing for
/// non-existent users. The result is intentionally discarded.
fn verify_dummy_password(password: &str) {
    if let Some(hash) = DUMMY_PASSWORD_HASH.as_deref()
        && let Ok(parsed) = PasswordHash::new(hash)
    {
        let _ = Argon2::default().verify_password(password.as_bytes(), &parsed);
    }
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
                "users:manage".to_owned(),
                "chat:write".to_owned()
            ])
            .is_ok()
        );

        let empty_error = validate_api_token_scopes(&[]).expect_err("empty scopes are rejected");
        assert_eq!(empty_error.status, StatusCode::BAD_REQUEST);

        let invalid_error =
            validate_api_token_scopes(&["admin:*".to_owned()]).expect_err("unknown scopes fail");
        assert_eq!(invalid_error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn permission_scopes_are_explicit_and_accepted() {
        let permissions = [
            Permission::ReadDashboard,
            Permission::ReadRuns,
            Permission::WriteRuns,
            Permission::ReadInventory,
            Permission::WriteBatches,
            Permission::UseChat,
            Permission::ReadReviews,
            Permission::WriteReviews,
            Permission::ReadSettings,
            Permission::WriteSettings,
            Permission::ManageUsers,
            Permission::ReadAudit,
        ];
        for permission in permissions {
            let scope = scope_for_permission(permission).to_owned();
            assert!(
                validate_api_token_scopes(&[scope]).is_ok(),
                "permission {permission:?} maps to unsupported scope"
            );
        }
        assert_eq!(
            scope_for_permission(Permission::ManageUsers),
            "users:manage"
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
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn build_cookie(
    name: &'static str,
    value: &str,
    http_only: bool,
    secure: bool,
    ttl_hours: i64,
) -> Cookie<'static> {
    let mut cookie = Cookie::build((name, value.to_owned()))
        .path("/")
        .same_site(SameSite::Lax)
        .http_only(http_only)
        .secure(secure)
        .max_age(cookie::time::Duration::hours(ttl_hours))
        .build();
    cookie.set_http_only(http_only);
    cookie
}

fn expire_cookie(name: &'static str, http_only: bool, secure: bool) -> Cookie<'static> {
    Cookie::build((name, ""))
        .path("/")
        .same_site(SameSite::Lax)
        .http_only(http_only)
        .secure(secure)
        .max_age(cookie::time::Duration::seconds(0))
        .build()
}

fn header_value(cookie: Cookie<'static>) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(&cookie.to_string())
        .map_err(|_| ApiError::internal("invalid cookie header"))
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    for cookie in header.split(';') {
        // Skip valueless segments rather than aborting the whole scan — a
        // leading junk cookie without `=` must not break session lookup.
        let Some((key, value)) = cookie.trim().split_once('=') else {
            continue;
        };
        if key == name {
            return Some(value.to_owned());
        }
    }
    None
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn service_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        if error.downcast_ref::<LastEnabledAdminError>().is_some() {
            warn!("user mutation rejected to preserve the enabled administrator invariant");
            return Self {
                status: StatusCode::CONFLICT,
                message: error.to_string(),
            };
        }
        if error.downcast_ref::<UserIdentityConflictError>().is_some() {
            warn!("user mutation rejected due to a normalized identity conflict");
            return Self {
                status: StatusCode::CONFLICT,
                message: "username or email is already assigned".to_owned(),
            };
        }
        if error.downcast_ref::<InvalidUserIdentityError>().is_some() {
            return Self::bad_request(error.to_string());
        }
        if error
            .downcast_ref::<AmbiguousUserIdentityLinkError>()
            .is_some()
        {
            warn!("OIDC linking rejected because claims identify multiple local accounts");
            return Self {
                status: StatusCode::CONFLICT,
                message: "OIDC identity matches multiple local accounts".to_owned(),
            };
        }
        if let Some(conflict) = error.downcast_ref::<ReviewApplyConflict>() {
            warn!(
                fields = ?conflict.fields(),
                "review apply rejected due to newer Paperless changes"
            );
            return Self {
                status: StatusCode::CONFLICT,
                message: conflict.to_string(),
            };
        }
        // Log the full cause chain server-side, but never return internal
        // error text (SQL fragments, column/constraint names, pool/reqwest
        // URLs) to the client — some 5xx paths are unauthenticated.
        tracing::error!(error = format!("{error:#}"), "internal server error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".to_owned(),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = format!("{error:#}"), "database error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".to_owned(),
        }
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod metadata_trace_tests {
    //! Table-driven unit tests for the metadata-trace outcome decision tree.
    //!
    //! Covers every branch of the 5-step tree from
    //! `docs/METADATA_TRACE_CONTRACT.md` so a regression in the worker's
    //! review_item shape or in the audit payload breaks tests at CI time
    //! rather than producing a confusing UI surface. Each case maps to one
    //! branch of `compute_field_outcome`.

    use super::*;
    use archivist_db::{MetadataApplyAudit, MetadataReviewItem};
    use chrono::TimeZone;
    use serde_json::json;
    use uuid::Uuid;

    /// Default runtime settings + a current_state with values set for the
    /// fields that have an `overwrite_existing_*` toggle. Used as the
    /// baseline for the table tests below.
    fn baseline_settings() -> RuntimeSettings {
        RuntimeSettings::default()
    }

    fn baseline_current_state() -> CurrentState {
        CurrentState {
            title: Some("Original Title".to_owned()),
            correspondent: Some("Existing Co".to_owned()),
            document_type: Some("Existing Type".to_owned()),
            document_date: chrono::NaiveDate::from_ymd_opt(2020, 1, 1),
            tags: vec!["existing".to_owned()],
        }
    }

    fn empty_current_state() -> CurrentState {
        CurrentState::default()
    }

    /// Helper for assembling a review_item that the decision tree should
    /// match against `field`.
    fn review_item(field: MetadataField, status: &str, suggested: Value) -> MetadataReviewItem {
        let mut patch = suggested;
        if let Some(object) = patch.as_object_mut() {
            object
                .entry("standard_metadata".to_owned())
                .or_insert_with(|| json!({ "field": field.as_str() }));
            if let Some(sm) = object
                .get_mut("standard_metadata")
                .and_then(Value::as_object_mut)
            {
                sm.insert("field".to_owned(), json!(field.as_str()));
            }
        }
        MetadataReviewItem {
            id: Uuid::nil(),
            run_id: Uuid::nil(),
            stage: "metadata".to_owned(),
            status: status.to_owned(),
            suggested_patch: patch,
            edited_patch: None,
            validation_warnings: json!([]),
            created_at: chrono::Utc.with_ymd_and_hms(2026, 5, 17, 0, 0, 0).unwrap(),
        }
    }

    fn audit_with_after(after: Value) -> MetadataApplyAudit {
        MetadataApplyAudit {
            id: Uuid::nil(),
            run_id: Uuid::nil(),
            after: Some(after),
            outcome: "success".to_owned(),
            created_at: chrono::Utc.with_ymd_and_hms(2026, 5, 17, 0, 0, 0).unwrap(),
        }
    }

    // -------- Branch 1a: review_item status == pending → review ---------
    #[test]
    fn pending_review_item_yields_review_outcome_with_below_threshold_reason() {
        let mut item = review_item(
            MetadataField::DocumentDate,
            "pending",
            json!({
                "created": "2026-04-15",
                "standard_metadata": {
                    "field": "document_date",
                    "suggested_date": "2026-04-15",
                    "confidence": 0.62,
                }
            }),
        );
        item.validation_warnings =
            json!([{ "LowConfidence": { "actual": 0.62, "threshold": 0.9 } }]);

        let outcome = compute_field_outcome(
            MetadataField::DocumentDate,
            &[&item],
            None,
            &empty_current_state(),
            &baseline_settings(),
            None,
        );
        assert_eq!(outcome.outcome, "review");
        assert_eq!(outcome.reason, Some("below_threshold"));
        assert_eq!(outcome.value, json!("2026-04-15"));
        assert_eq!(outcome.confidence, Some(0.62));
        assert_eq!(outcome.warnings.len(), 1);
    }

    // -------- Branch 1b: review_item status == approved → applied -------
    #[test]
    fn approved_review_item_yields_applied_outcome_with_value() {
        let item = review_item(
            MetadataField::Title,
            "approved",
            json!({
                "title": "Invoice 2026-04 Acme",
                "standard_metadata": { "field": "title", "auto_validated": true }
            }),
        );
        let outcome = compute_field_outcome(
            MetadataField::Title,
            &[&item],
            None,
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({ "title": { "title": "Invoice 2026-04 Acme", "confidence": 0.92 } })),
        );
        assert_eq!(outcome.outcome, "applied");
        assert_eq!(outcome.value, json!("Invoice 2026-04 Acme"));
        assert_eq!(outcome.reason, None);
    }

    // -------- Branch 1c: review_item status == rejected → rejected ------
    #[test]
    fn rejected_review_item_yields_rejected_outcome_with_operator_reason() {
        let item = review_item(
            MetadataField::Correspondent,
            "rejected",
            json!({
                "correspondent": "",
                "standard_metadata": {
                    "field": "correspondent",
                    "suggested_name": "Acme Corp",
                    "confidence": 0.85,
                }
            }),
        );
        let outcome = compute_field_outcome(
            MetadataField::Correspondent,
            &[&item],
            None,
            &empty_current_state(),
            &baseline_settings(),
            None,
        );
        assert_eq!(outcome.outcome, "rejected");
        assert_eq!(outcome.reason, Some("rejected_by_operator"));
        assert_eq!(outcome.value, json!("Acme Corp"));
    }

    // -------- Branch 2: audit `after` carries the field → applied -------
    #[test]
    fn audit_applied_field_yields_applied_outcome_when_no_review_item_exists() {
        let audit = audit_with_after(json!({
            "title": "Acme Invoice",
            "correspondent": 42,
            "tags": [1, 2, 3]
        }));
        let llm = json!({
            "title": { "title": "Acme Invoice", "confidence": 0.95 }
        });

        let outcome = compute_field_outcome(
            MetadataField::Title,
            &[],
            Some(&audit),
            &empty_current_state(),
            &baseline_settings(),
            Some(&llm),
        );
        assert_eq!(outcome.outcome, "applied");
        assert_eq!(outcome.value, json!("Acme Invoice"));
        assert_eq!(outcome.confidence, Some(0.95));
    }

    #[test]
    fn audit_applied_document_date_keyed_as_created() {
        let audit = audit_with_after(json!({ "created": "2026-04-15" }));
        let outcome = compute_field_outcome(
            MetadataField::DocumentDate,
            &[],
            Some(&audit),
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({
                "document_date": { "date": "2026-04-15", "confidence": 0.93 }
            })),
        );
        assert_eq!(outcome.outcome, "applied");
        assert_eq!(outcome.value, json!("2026-04-15"));
    }

    // -------- Branch 3: current value set + overwrite disabled → skipped ----
    #[test]
    fn skipped_when_overwrite_disabled_for_correspondent() {
        // No review item, no audit. Settings default has
        // overwrite_existing_correspondent = false.
        let outcome = compute_field_outcome(
            MetadataField::Correspondent,
            &[],
            None,
            &baseline_current_state(),
            &baseline_settings(),
            Some(&json!({
                "correspondent": { "name": "New Co", "confidence": 0.99 }
            })),
        );
        assert_eq!(outcome.outcome, "skipped");
        assert_eq!(outcome.reason, Some("overwrite_disabled"));
        assert_eq!(outcome.value, Value::Null);
    }

    #[test]
    fn overwrite_disabled_branch_does_not_fire_when_setting_is_on() {
        // Same as the previous test but with the override flipped on —
        // branch 3 must NOT fire. The decision tree falls through to
        // branch 5 (entity_not_found) because no audit / review row
        // exists, so the outcome is still "skipped" but with a different
        // reason. The test exists to prove branch 3 is gated correctly.
        let mut settings = baseline_settings();
        settings.metadata.overwrite_existing_correspondent = true;
        let outcome = compute_field_outcome(
            MetadataField::Correspondent,
            &[],
            None,
            &baseline_current_state(),
            &settings,
            Some(&json!({
                "correspondent": { "name": "New Co", "confidence": 0.99 }
            })),
        );
        assert_eq!(outcome.outcome, "skipped");
        assert_eq!(
            outcome.reason,
            Some("entity_not_found"),
            "branch 3 must not fire when overwrite_existing_correspondent is true"
        );
    }

    // -------- Branch 4: LLM omitted the field entirely → dropped --------
    #[test]
    fn dropped_when_llm_suggestion_omits_field() {
        let outcome = compute_field_outcome(
            MetadataField::Tags,
            &[],
            None,
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({ "title": { "title": "x", "confidence": 0.9 } })),
        );
        assert_eq!(outcome.outcome, "dropped");
        assert_eq!(outcome.reason, Some("no_proposal"));
        assert_eq!(outcome.value, Value::Null);
        assert_eq!(outcome.confidence, None);
    }

    #[test]
    fn dropped_when_llm_artifact_is_absent_entirely() {
        for field in MetadataField::all() {
            let outcome = compute_field_outcome(
                field,
                &[],
                None,
                &empty_current_state(),
                &baseline_settings(),
                None,
            );
            assert_eq!(
                outcome.outcome,
                "dropped",
                "{} should be dropped",
                field.as_str()
            );
            assert_eq!(outcome.reason, Some("no_proposal"));
        }
    }

    // -------- Branch 5: LLM proposed but entity didn't resolve → skipped --
    #[test]
    fn skipped_entity_not_found_for_correspondent_when_no_audit_and_no_review() {
        // No audit, no review item, correspondent absent from current
        // state (so branch 3 doesn't fire) but LLM proposed a name.
        let outcome = compute_field_outcome(
            MetadataField::Correspondent,
            &[],
            None,
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({
                "correspondent": { "name": "Ghost Co", "confidence": 0.95 }
            })),
        );
        assert_eq!(outcome.outcome, "skipped");
        assert_eq!(outcome.reason, Some("entity_not_found"));
        assert_eq!(outcome.value, json!("Ghost Co"));
        assert_eq!(outcome.confidence, Some(0.95));
    }

    #[test]
    fn skipped_entity_not_found_for_document_type_when_no_audit_and_no_review() {
        let outcome = compute_field_outcome(
            MetadataField::DocumentType,
            &[],
            None,
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({
                "document_type": { "name": "Unknown", "confidence": 0.91 }
            })),
        );
        assert_eq!(outcome.outcome, "skipped");
        assert_eq!(outcome.reason, Some("entity_not_found"));
        assert_eq!(outcome.value, json!("Unknown"));
    }

    // -------- Branch ordering: review_item beats audit row --------------
    #[test]
    fn review_item_takes_precedence_over_audit_row() {
        // The metadata path can produce both: the auto_validated review
        // item AND the audit row. Branch 1 must win so the operator sees
        // the review status rather than a stale "applied" badge.
        let item = review_item(
            MetadataField::Title,
            "pending",
            json!({
                "title": "Pending Title",
                "standard_metadata": { "field": "title", "confidence": 0.8 }
            }),
        );
        let audit = audit_with_after(json!({ "title": "Audit Title" }));
        let outcome = compute_field_outcome(
            MetadataField::Title,
            &[&item],
            Some(&audit),
            &empty_current_state(),
            &baseline_settings(),
            None,
        );
        assert_eq!(outcome.outcome, "review");
        assert_eq!(outcome.value, json!("Pending Title"));
    }

    // -------- Field invariant: outcome ordering matches contract --------
    #[test]
    fn metadata_field_ordering_matches_contract() {
        let names: Vec<&'static str> = MetadataField::all()
            .iter()
            .map(|field| field.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "title",
                "correspondent",
                "document_type",
                "document_date",
                "tags",
                "fields",
            ]
        );
    }

    // -------- Response body assembly --------
    #[test]
    fn metadata_trace_response_body_carries_run_header_and_outcomes() {
        let header = MetadataRunHeader {
            run_id: Uuid::nil(),
            paperless_document_id: 4904,
            status: "succeeded".to_owned(),
            created_at: chrono::Utc
                .with_ymd_and_hms(2026, 5, 17, 13, 42, 11)
                .unwrap(),
            finished_at: None,
        };
        let audit = audit_with_after(json!({ "title": "Acme" }));
        let outcomes = MetadataField::all()
            .iter()
            .map(|field| json!({ "field": field.as_str(), "outcome": "dropped" }))
            .collect();
        let body = metadata_trace_response_body(
            4904,
            &header,
            None,
            Some(&audit),
            &CurrentState::default(),
            outcomes,
            None,
        );
        assert_eq!(body["paperless_document_id"], json!(4904));
        assert_eq!(body["latest_run"]["status"], json!("succeeded"));
        assert_eq!(body["latest_run"]["stage"], json!("metadata"));
        assert_eq!(
            body["latest_run"]["applied_at"].as_str(),
            Some("2026-05-17T00:00:00Z")
        );
        assert_eq!(
            body["latest_run"]["per_field_outcomes"]
                .as_array()
                .map(Vec::len),
            Some(6)
        );
    }

    #[test]
    fn metadata_trace_response_body_omits_applied_at_when_audit_failed() {
        let header = MetadataRunHeader {
            run_id: Uuid::nil(),
            paperless_document_id: 4904,
            status: "failed".to_owned(),
            created_at: chrono::Utc
                .with_ymd_and_hms(2026, 5, 17, 13, 42, 11)
                .unwrap(),
            finished_at: None,
        };
        let failed_audit = MetadataApplyAudit {
            id: Uuid::nil(),
            run_id: Uuid::nil(),
            after: Some(json!({ "title": "Acme" })),
            outcome: "failed".to_owned(),
            created_at: chrono::Utc.with_ymd_and_hms(2026, 5, 17, 0, 0, 0).unwrap(),
        };
        let body = metadata_trace_response_body(
            4904,
            &header,
            None,
            Some(&failed_audit),
            &CurrentState::default(),
            Vec::new(),
            None,
        );
        assert!(body["latest_run"]["applied_at"].is_null());
    }
}
