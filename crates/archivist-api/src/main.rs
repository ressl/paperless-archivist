use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use archivist_ai::{
    AiResponse, AnthropicClient, ChatRequest, OllamaClient, OllamaLoadedModel, OllamaModel,
    OpenAiCompatibleClient, TextProvider,
};
use archivist_config::AppConfig;
use archivist_core::{
    AiProviderKind, AuditEventInput, DashboardProviderCostSummary, DashboardRange, DashboardStats,
    DocumentChatSource, DocumentInventoryItem, DocumentPatch, Permission, ProcessingMode,
    ProviderUsageStats, Role, RuntimeSettings, Stage, WorkflowRules, build_document_chat_prompt,
    document_chat_snippet, document_chat_terms, roles_have_permission, score_document_chat_source,
};
use archivist_db::{
    AuthUser, DbPool, DocumentChatCandidate, MetadataApplyAudit, MetadataArtifact,
    MetadataReviewItem, MetadataRunHeader, OidcUserInput, ProviderBucketEntry, ReviewItemRecord,
    append_audit, apply_security_retention, connect, consume_oidc_login_state,
    create_document_chat_session, create_oidc_login_state, create_run_with_jobs_with_priority,
    create_session, create_user_with_roles, dashboard_bucket_labels, dashboard_range_start,
    document_chat_session_visible, find_api_token, find_session, find_user_for_login,
    get_backlog_counts, get_dashboard_live_status, get_dashboard_stats, get_runtime_settings,
    has_any_user, hash_token, insert_document_chat_message, insert_document_chat_sources,
    latest_apply_audit_for_run, latest_metadata_artifact_for_run, latest_metadata_run_for_document,
    list_audit_events, list_document_chat_messages, list_document_chat_sessions, list_inventory,
    list_prompt_usage, list_prompts, list_reviews, list_secret_references, list_sessions,
    list_users, metadata_review_items_for_run, metrics_snapshot as db_metrics_snapshot, migrate,
    paperless_sync_cursor, provider_bucket_entries, queue_missing_pipeline, queue_missing_stage,
    record_login_failure, record_login_success, recover_stale_leases, recover_stuck_runs,
    recovery_candidates, resolve_secret, review_decision, revoke_session_by_admin,
    rotate_api_token, search_document_chat_candidates, set_user_enabled, set_user_roles,
    update_paperless_sync_cursor, update_runtime_settings, update_user_password_hash,
    upsert_encrypted_secret, upsert_inventory_item, upsert_oidc_user,
    upsert_paperless_custom_field, upsert_paperless_named_entity, upsert_paperless_tag,
    verify_audit_integrity,
};
use archivist_paperless::{PaperlessClient, PaperlessDocumentDetail, PaperlessTag};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, Params};
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
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
        forwarded_for_first_hop(req.headers())
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

fn forwarded_for_first_hop(headers: &HeaderMap) -> Option<IpAddr> {
    let value = headers.get("x-forwarded-for")?.to_str().ok()?;
    let first = value.split(',').next()?.trim();
    first.parse::<IpAddr>().ok()
}

/// Resolve the client IP for audit/logging purposes. When `trust_proxy` is
/// enabled and `X-Forwarded-For` is present, use the first hop; otherwise
/// fall back to the TCP peer recorded by axum.
fn request_source_ip(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<String> {
    let forwarded = if state.config.trust_proxy {
        forwarded_for_first_hop(headers)
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
        .route("/workflow/mode", put(update_workflow_mode))
        .route("/workflow/controls", patch(update_workflow_controls))
        .route("/inventory", get(inventory))
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
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .nest("/api/auth", auth_public)
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
        .with_state(state)
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

async fn metrics(State(state): State<AppState>) -> ApiResult<Response> {
    let snapshot = db_metrics_snapshot(&state.pool).await?;
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
            "# HELP paperless_archivist_audit_events Audit events total\n",
            "# TYPE paperless_archivist_audit_events counter\n",
            "paperless_archivist_audit_events {}\n",
            "# HELP paperless_archivist_selector_runs_total Automatic selector runs\n",
            "# TYPE paperless_archivist_selector_runs_total counter\n",
            "paperless_archivist_selector_runs_total {}\n",
            "# HELP paperless_archivist_selector_documents_queued_total Documents queued by automatic selector\n",
            "# TYPE paperless_archivist_selector_documents_queued_total counter\n",
            "paperless_archivist_selector_documents_queued_total {}\n",
            "# HELP paperless_archivist_job_retries_scheduled_total Job retries scheduled after transient failures\n",
            "# TYPE paperless_archivist_job_retries_scheduled_total counter\n",
            "paperless_archivist_job_retries_scheduled_total {}\n",
            "# HELP paperless_archivist_model_errors_total Jobs with model-stage error messages\n",
            "# TYPE paperless_archivist_model_errors_total gauge\n",
            "paperless_archivist_model_errors_total {}\n",
            "# HELP paperless_archivist_apply_success_total Successful Paperless apply operations\n",
            "# TYPE paperless_archivist_apply_success_total counter\n",
            "paperless_archivist_apply_success_total {}\n",
            "# HELP paperless_archivist_apply_failure_total Failed Paperless apply operations\n",
            "# TYPE paperless_archivist_apply_failure_total counter\n",
            "paperless_archivist_apply_failure_total {}\n",
            "# HELP paperless_archivist_apply_latency_ms_sum Sum of observed Paperless apply latency in milliseconds\n",
            "# TYPE paperless_archivist_apply_latency_ms_sum counter\n",
            "paperless_archivist_apply_latency_ms_sum {}\n",
            "# HELP paperless_archivist_apply_latency_ms_count Count of observed Paperless apply latency samples\n",
            "# TYPE paperless_archivist_apply_latency_ms_count counter\n",
            "paperless_archivist_apply_latency_ms_count {}\n",
            "# HELP paperless_archivist_apply_latency_ms_p95 Rolling p95 of observed Paperless apply latency in milliseconds\n",
            "# TYPE paperless_archivist_apply_latency_ms_p95 gauge\n",
            "paperless_archivist_apply_latency_ms_p95 {}\n"
        ),
        snapshot.jobs_queued,
        snapshot.jobs_running,
        snapshot.jobs_failed,
        snapshot.jobs_succeeded,
        snapshot.reviews_pending,
        snapshot.runs_active,
        snapshot.audit_events,
        snapshot.selector_runs_total,
        snapshot.selector_documents_queued_total,
        snapshot.job_retries_scheduled_total,
        snapshot.model_errors_total,
        snapshot.apply_success_total,
        snapshot.apply_failure_total,
        snapshot.apply_latency_ms_sum,
        snapshot.apply_latency_ms_count,
        snapshot.apply_latency_ms_p95
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
    preferred_username: Option<String>,
    #[serde(default)]
    at_hash: Option<String>,
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
    let claims = oidc_verify_id_token(
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

    let subject = claims.sub.as_str();
    let email = claims.email.as_deref();
    let username = oidc_username(
        claims
            .preferred_username
            .as_deref()
            .or(email)
            .unwrap_or(subject),
    );
    let roles = oidc_roles(&state.config, &username, email)?;
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
    if user
        .locked_until
        .is_some_and(|locked_until| locked_until > Utc::now())
    {
        return Err(ApiError::unauthorized("invalid credentials"));
    }
    if !user.enabled || !verify_password(&user, &request.password)? {
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

    if verify_paperless_credentials(&settings, paperless_username, &request.password)
        .await
        .is_err()
    {
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

    let username = paperless_bridge_username(paperless_username);
    let user = match find_user_for_login(&state.pool, &username).await? {
        Some(user) => user,
        None => {
            let disabled_password_hash = hash_password(&random_token())?;
            create_user_with_roles(
                &state.pool,
                &username,
                None,
                &disabled_password_hash,
                &[Role::Viewer],
                None,
            )
            .await?;
            find_user_for_login(&state.pool, &username)
                .await?
                .ok_or_else(|| ApiError::internal("created Paperless bridge user was not found"))?
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
                "paperless_username": paperless_username
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
) -> Result<()> {
    let base_url = Url::parse(settings.paperless.base_url.trim())
        .context("Paperless base URL is not valid")?;
    let token_url = base_url
        .join("api/token/")
        .context("build Paperless token URL")?;
    let client = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(
            settings.paperless.timeout_seconds.clamp(1, 120),
        ))
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
    if token.token.trim().is_empty() {
        return Err(anyhow!("Paperless returned an empty token"));
    }
    Ok(())
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
    let user_id = if roles_have_permission(&auth.0.roles, Permission::ManageUsers) {
        None
    } else {
        Some(
            auth.0
                .user_id
                .ok_or_else(|| ApiError::forbidden("session listing requires a user session"))?,
        )
    };
    Ok(Json(
        json!({ "items": list_sessions(&state.pool, user_id).await? }),
    ))
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
    update_runtime_settings(&state.pool, &request.settings, actor_id).await?;
    info!(%actor_id, "runtime settings updated");
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
    let mut chat_request = build_prompt_test_chat_request(&request, &sample_text)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    chat_request.system_prompt = request.content.trim().to_owned();

    let mut provider = if let Some(provider_name) = request
        .provider_name
        .as_deref()
        .filter(|provider_name| !provider_name.trim().is_empty())
    {
        provider_by_name(&settings, provider_name)
            .map_err(|error| ApiError::bad_request(error.to_string()))?
    } else {
        provider_for_default_text(&settings)
            .map_err(|error| ApiError::bad_request(error.to_string()))?
    };
    if let Some(model) = request
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
    {
        provider.model = model.trim().to_owned();
    }
    chat_request.model = provider.model.clone();

    let response = chat_with_default_provider(&state, &provider, chat_request.clone()).await?;
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

#[derive(Debug)]
struct PromptTestParsed {
    parsed: Option<Value>,
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

async fn build_prompt_test_chat_request(
    request: &TestPromptRequest,
    sample_text: &str,
) -> Result<ChatRequest> {
    match request.stage {
        Stage::Ocr => Ok(ChatRequest {
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
        }),
        // Stage::Metadata uses the consolidated prompt builder added alongside the worker
        // handler. The builder is registered in archivist-ai in a follow-up commit; until
        // then, surface a clear error rather than silently testing a stale prompt.
        Stage::Metadata => Err(anyhow!(
            "prompt testing for the consolidated metadata stage is added in a later v1.4.0 commit"
        )),
        Stage::Apply => Err(anyhow!(
            "prompt testing is not supported for stage {}",
            request.stage
        )),
    }
}

fn parse_prompt_test_output(stage: Stage, text: &str) -> PromptTestParsed {
    match stage {
        Stage::Ocr => PromptTestParsed {
            parsed: Some(json!({ "content": text })),
            validation_errors: Vec::new(),
            warnings: Vec::new(),
        },
        Stage::Metadata | Stage::Apply => PromptTestParsed {
            parsed: None,
            validation_errors: vec![format!("unsupported stage: {stage}")],
            warnings: Vec::new(),
        },
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

async fn test_provider(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadSettings)?;
    let settings = get_runtime_settings(&state.pool).await?;
    let provider = provider_for_default_text(&settings)?;
    if let Err(error) = validate_outbound_url(&provider.base_url).await {
        return Ok(Json(json!({
            "ok": false,
            "error": format!("AI provider base URL rejected: {}", error.message),
        })));
    }
    let result = test_ai_provider(&state, &provider).await;
    match result {
        Ok(value) => Ok(Json(json!({ "ok": true, "details": value }))),
        Err(error) => Ok(Json(json!({ "ok": false, "error": error.to_string() }))),
    }
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
/// an admin can supply an arbitrary URL.
///
/// Rejections:
///  * non-http/https schemes
///  * URLs containing `user:pass@` userinfo
///  * URLs whose host resolves (DNS) to a loopback, link-local, private
///    (RFC1918), shared-address (RFC6598), or unique-local (RFC4193) address
///
/// Returns the parsed `Url` on success.
async fn validate_outbound_url(raw: &str) -> Result<Url, ApiError> {
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
        url::Host::Domain(domain) => tokio::net::lookup_host((domain, port))
            .await
            .map_err(|error| ApiError::bad_request(format!("failed to resolve host: {error}")))?
            .map(|addr| addr.ip())
            .collect(),
    };
    if ips.is_empty() {
        return Err(ApiError::bad_request("host did not resolve to any address"));
    }
    for ip in &ips {
        if is_ssrf_dangerous_ip(*ip) {
            return Err(ApiError::bad_request(
                "URL resolves to a loopback, link-local, or otherwise unroutable address",
            ));
        }
    }
    Ok(parsed)
}

/// Decide whether an IP must be hard-rejected for admin-supplied "test"
/// URLs (Paperless health probe, Ollama provider test, generic notification
/// webhook test).
///
/// The threat model here is narrow on purpose:
///
/// - These endpoints require `WriteSettings`; the operator who can call them
///   already controls the settings document and is trusted to point the
///   integration at real internal services.
/// - Paperless Archivist is routinely deployed inside Kubernetes / Docker
///   Compose / on-prem networks where Paperless-ngx and Ollama live on
///   private addresses (10/8, 172.16/12, 192.168/16, RFC6598 100.64/10,
///   RFC4193 fc00::/7). Rejecting those would make the in-UI "Test" buttons
///   unusable in every realistic deployment.
///
/// What we DO still reject — the addresses that have no legitimate operator
/// use case and that an attacker who briefly steals a session could abuse:
///
/// - Loopback (127.0.0.0/8, ::1)
/// - Link-local incl. cloud metadata IMDS (169.254.0.0/16, fe80::/10)
/// - Unspecified (0.0.0.0, ::)
/// - Broadcast (255.255.255.255)
/// - Multicast
///
/// See `docs/SECURITY_DESIGN.md` section 4.3 for the full rationale.
fn is_ssrf_dangerous_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_ssrf_dangerous_ipv4(v4),
        IpAddr::V6(v6) => is_ssrf_dangerous_ipv6(v6),
    }
}

fn is_ssrf_dangerous_ipv4(ip: Ipv4Addr) -> bool {
    // Hard reject — no legitimate operator target.
    if ip.is_loopback() || ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() {
        return true;
    }
    // Link-local 169.254/16 includes the cloud-metadata IMDS endpoint
    // (169.254.169.254). Keep rejecting — leaking cloud IAM creds via a
    // ghost "test" request would be catastrophic, and no real integration
    // target lives there.
    if ip.is_link_local() {
        return true;
    }
    false
}

fn is_ssrf_dangerous_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    // Mapped IPv4: re-evaluate the embedded v4 so an attacker can't smuggle
    // 127.0.0.1 as ::ffff:127.0.0.1 / ::127.0.0.1 past the loopback check.
    if let Some(v4) = ip.to_ipv4_mapped()
        && is_ssrf_dangerous_ipv4(v4)
    {
        return true;
    }
    if let Some(v4) = ip.to_ipv4()
        && is_ssrf_dangerous_ipv4(v4)
    {
        return true;
    }
    let segments = ip.segments();
    // Link-local fe80::/10 — same metadata/IMDS reasoning as v4.
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    false
}

async fn send_notification_webhook(webhook_url: &str, payload: Value) -> Result<()> {
    let response = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(10))
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
            let client =
                OllamaClient::new_with_timeout(base, secret, std::time::Duration::from_secs(12))?;
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
    let client = match OllamaClient::new_with_timeout(
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
}

fn provider_by_name(settings: &RuntimeSettings, name: &str) -> Result<ApiProvider> {
    let mut provider = settings
        .ai
        .providers
        .iter()
        .find(|provider| provider.name == name)
        .cloned()
        .or_else(|| {
            if name == "ollama" {
                Some(archivist_core::AiProviderSettings::ollama_default())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("AI provider '{name}' is not configured"))?;
    if provider.name == "ollama" {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings.ai.default_model_for_provider(&provider, false);
    let base_url = provider_base_url(&provider.kind, &provider.base_url);
    Ok(ApiProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
    })
}

fn provider_for_default_text(settings: &RuntimeSettings) -> Result<ApiProvider> {
    let mut provider = settings
        .ai
        .providers
        .iter()
        .find(|provider| provider.enabled && provider.name == settings.ai.default_provider)
        .cloned()
        .or_else(|| {
            if settings.ai.default_provider == "ollama" {
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
    if provider.name == "ollama" {
        provider.base_url = settings.ai.ollama_base_url.clone();
    }
    let model = settings.ai.default_model_for_provider(&provider, false);
    let base_url = provider_base_url(&provider.kind, &provider.base_url);
    Ok(ApiProvider {
        name: provider.name,
        kind: provider.kind,
        base_url,
        model,
        secret_id: provider.secret_id,
    })
}

fn provider_base_url(kind: &AiProviderKind, configured: &str) -> String {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return trimmed.trim_end_matches('/').to_owned();
    }
    match kind {
        AiProviderKind::Ollama => "http://ollama:11434".to_owned(),
        AiProviderKind::Openai => "https://api.openai.com/v1".to_owned(),
        AiProviderKind::Anthropic => "https://api.anthropic.com/v1".to_owned(),
        AiProviderKind::OpenaiCompatible => "http://localhost:8000/v1".to_owned(),
    }
}

async fn provider_secret(state: &AppState, provider: &ApiProvider) -> Result<Option<SecretString>> {
    let Some(secret_id) = provider.secret_id else {
        return Ok(None);
    };
    resolve_secret(&state.pool, &state.config.secret_key, secret_id).await
}

async fn test_ai_provider(state: &AppState, provider: &ApiProvider) -> Result<Value> {
    match provider.kind {
        AiProviderKind::Ollama => {
            let client =
                OllamaClient::new(&provider.base_url, provider_secret(state, provider).await?)?;
            client.test_connection(Some(&provider.model)).await
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new(
                &provider.name,
                &provider.base_url,
                provider_secret(state, provider).await?,
            )?;
            let response = client
                .chat(ChatRequest {
                    model: provider.model.clone(),
                    system_prompt: "Return a short health check response.".to_owned(),
                    user_prompt: "Return OK.".to_owned(),
                    temperature: 0.0,
                    num_ctx: None,
                    response_schema: None,
                    reasoning_effort: None,
                })
                .await?;
            Ok(
                json!({ "provider": response.provider, "model": response.model, "text": response.text }),
            )
        }
        AiProviderKind::Anthropic => {
            let secret = provider_secret(state, provider).await?.ok_or_else(|| {
                anyhow!("AI provider '{}' requires an API key secret", provider.name)
            })?;
            let client = AnthropicClient::new(&provider.name, &provider.base_url, secret)?;
            let response = client
                .chat(ChatRequest {
                    model: provider.model.clone(),
                    system_prompt: "Return a short health check response.".to_owned(),
                    user_prompt: "Return OK.".to_owned(),
                    temperature: 0.0,
                    num_ctx: None,
                    response_schema: None,
                    reasoning_effort: None,
                })
                .await?;
            Ok(
                json!({ "provider": response.provider, "model": response.model, "text": response.text }),
            )
        }
    }
}

async fn chat_with_default_provider(
    state: &AppState,
    provider: &ApiProvider,
    request: ChatRequest,
) -> Result<AiResponse> {
    match provider.kind {
        AiProviderKind::Ollama => {
            let client =
                OllamaClient::new(&provider.base_url, provider_secret(state, provider).await?)?;
            client.chat(request).await
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            let client = OpenAiCompatibleClient::new(
                &provider.name,
                &provider.base_url,
                provider_secret(state, provider).await?,
            )?;
            client.chat(request).await
        }
        AiProviderKind::Anthropic => {
            let secret = provider_secret(state, provider).await?.ok_or_else(|| {
                anyhow!("AI provider '{}' requires an API key secret", provider.name)
            })?;
            let client = AnthropicClient::new(&provider.name, &provider.base_url, secret)?;
            client.chat(request).await
        }
    }
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
        if completion_tag_reconcile_needed(
            &tag_names,
            &stage_completion_tags,
            &settings.workflow.tags.completion_processed,
        ) {
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
) -> bool {
    !stage_completion_tags.is_empty()
        && stage_completion_tags
            .iter()
            .all(|tag| tag_names.iter().any(|name| name.eq_ignore_ascii_case(tag)))
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
        date_from: query.date_from.filter(|s| !s.is_empty()),
        date_to: query.date_to.filter(|s| !s.is_empty()),
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

    /// Key under which `audit_events.document.patch_applied.after` carries
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
            Self::DocumentDate => current
                .document_date
                .as_deref()
                .is_some_and(|v| !v.is_empty()),
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
    document_date: Option<String>,
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
    let response = chat_with_default_provider(
        &state,
        &provider,
        ChatRequest {
            model: provider.model.clone(),
            system_prompt: prompt.system_prompt,
            user_prompt: prompt.user_prompt,
            temperature: 0.1,
            num_ctx: None,
            response_schema: None,
            reasoning_effort: None,
        },
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
        query.limit.unwrap_or(100),
    )
    .await?
    .into_iter()
    .map(|review| review_with_debug(review, &settings))
    .collect::<Result<Vec<_>>>()?;
    Ok(Json(json!({
        "items": items
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
                    run_id: Some(item.run_id),
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
                    run_id: Some(item.run_id),
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
    let cooldowns_cleared = if request.clear_provider_cooldowns {
        archivist_db::clear_all_provider_cooldowns(&state.pool).await?
    } else {
        0
    };
    info!(
        %actor_id,
        predecessors_requeued = summary.predecessors_requeued,
        runs_unblocked = summary.runs_unblocked,
        cooldowns_cleared,
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
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id = require_user_session(&auth.0, "clearing cooldowns requires a user session")?;
    Span::current().record("user_id", tracing::field::display(actor_id));
    let cleared = match request.provider_name.as_deref() {
        Some(name) => archivist_db::clear_provider_cooldown(&state.pool, name).await?,
        None => archivist_db::clear_all_provider_cooldowns(&state.pool).await?,
    };
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
            })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await;
    Ok(Json(json!({ "cleared": cleared })))
}

async fn audit_events(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadAudit)?;
    Ok(Json(
        json!({ "items": list_audit_events(&state.pool, 200).await? }),
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
        const HEADER: &str = "id,created_at,event_type,actor_type,actor_id,paperless_document_id,outcome,error_message,metadata,prev_event_hash,event_hash,source_ip,user_agent\n";
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
                   prev_event_hash, event_hash, source_ip, user_agent
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
    let Some(review) = archivist_db::pending_review_for_apply(&state.pool, review_id).await? else {
        return Ok(());
    };
    Span::current().record("run_id", tracing::field::display(review.run_id));
    Span::current().record("paperless_document_id", review.paperless_document_id);
    let patch_value = review
        .edited_patch
        .clone()
        .unwrap_or_else(|| review.suggested_patch.clone());
    let mut patch: DocumentPatch = serde_json::from_value(patch_value)?;
    let final_run_stage = if let Some(job_id) = review.job_id {
        archivist_db::is_last_active_job(&state.pool, review.run_id, job_id).await?
    } else {
        false
    };
    add_completion_and_remove_trigger_tags(state, &review, &mut patch, final_run_stage).await?;
    let client = paperless_client(&state.pool, &state.config).await?;
    let document = client.get_document(review.paperless_document_id).await?;
    prune_unchanged_patch_fields(&mut patch, &document);
    let before = audit_before_for_patch(&document, &patch);
    let after = audit_patch_payload(&patch);
    let apply_started = std::time::Instant::now();
    if let Err(error) = client
        .patch_document(review.paperless_document_id, &patch)
        .await
    {
        let duration_ms = apply_started.elapsed().as_millis() as u64;
        append_audit(
            &state.pool,
            AuditEventInput {
                event_type: "document.patch_apply_failed".to_owned(),
                actor_type: "user".to_owned(),
                actor_id: Some(actor_id.to_string()),
                run_id: Some(review.run_id),
                job_id: review.job_id,
                paperless_document_id: Some(review.paperless_document_id),
                before: Some(before),
                after: Some(after),
                metadata: Some(json!({
                    "stage": review.stage,
                    "review_id": review.id,
                    "duration_ms": duration_ms
                })),
                outcome: "failed".to_owned(),
                error_message: Some(error.to_string()),
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
        return Err(error);
    }
    let duration_ms = apply_started.elapsed().as_millis() as u64;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "document.patch_applied".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: Some(review.run_id),
            job_id: review.job_id,
            paperless_document_id: Some(review.paperless_document_id),
            before: Some(before),
            after: Some(after),
            metadata: Some(json!({
                "stage": review.stage,
                "review_id": review.id,
                "duration_ms": duration_ms
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    archivist_db::mark_review_applied(&state.pool, review_id, actor_id).await?;
    info!(
        %review_id,
        run_id = %review.run_id,
        paperless_document_id = review.paperless_document_id,
        duration_ms,
        "review patch applied to Paperless"
    );
    Ok(())
}

fn prune_unchanged_patch_fields(patch: &mut DocumentPatch, document: &PaperlessDocumentDetail) {
    if patch.content.as_deref() == document.content.as_deref() {
        patch.content = None;
    }
    if patch.title == document.title {
        patch.title = None;
    }
    if patch
        .tags
        .as_ref()
        .is_some_and(|tags| same_i32_set(tags, &document.tags))
    {
        patch.tags = None;
    }
    if patch
        .correspondent
        .as_ref()
        .is_some_and(|value| *value == document.correspondent)
    {
        patch.correspondent = None;
    }
    if patch
        .document_type
        .as_ref()
        .is_some_and(|value| *value == document.document_type)
    {
        patch.document_type = None;
    }
    if patch
        .created
        .as_deref()
        .is_some_and(|value| document_date_equals(document.created.as_deref(), value))
    {
        patch.created = None;
    }
}

fn same_i32_set(left: &[i32], right: &[i32]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort_unstable();
    left.dedup();
    right.sort_unstable();
    right.dedup();
    left == right
}

fn document_date_equals(current: Option<&str>, requested: &str) -> bool {
    current
        .map(|value| value.get(..10).unwrap_or(value) == requested)
        .unwrap_or(false)
}

fn audit_before_for_patch(
    document: &PaperlessDocumentDetail,
    patch: &DocumentPatch,
) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    if patch.content.is_some() {
        object.insert(
            "content".to_owned(),
            audit_text_metadata(document.content.as_deref().unwrap_or_default()),
        );
    }
    if patch.title.is_some() {
        object.insert("title".to_owned(), json!(document.title));
    }
    if patch.tags.is_some() {
        object.insert("tags".to_owned(), json!(document.tags));
    }
    if patch.correspondent.is_some() {
        object.insert("correspondent".to_owned(), json!(document.correspondent));
    }
    if patch.document_type.is_some() {
        object.insert("document_type".to_owned(), json!(document.document_type));
    }
    if patch.created.is_some() {
        object.insert("created".to_owned(), json!(document.created));
    }
    if patch.custom_fields.is_some() {
        object.insert("custom_fields".to_owned(), json!({ "present": "redacted" }));
    }
    serde_json::Value::Object(object)
}

fn audit_patch_payload(patch: &DocumentPatch) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    if let Some(content) = &patch.content {
        object.insert("content".to_owned(), audit_text_metadata(content));
    }
    if let Some(title) = &patch.title {
        object.insert("title".to_owned(), json!(title));
    }
    if let Some(tags) = &patch.tags {
        object.insert("tags".to_owned(), json!(tags));
    }
    if let Some(correspondent) = &patch.correspondent {
        object.insert("correspondent".to_owned(), json!(correspondent));
    }
    if let Some(document_type) = &patch.document_type {
        object.insert("document_type".to_owned(), json!(document_type));
    }
    if let Some(created) = &patch.created {
        object.insert("created".to_owned(), json!(created));
    }
    if let Some(custom_fields) = &patch.custom_fields {
        object.insert(
            "custom_fields".to_owned(),
            json!({
                "sha256": hex::encode(Sha256::digest(custom_fields.to_string().as_bytes())),
                "redacted": true
            }),
        );
    }
    serde_json::Value::Object(object)
}

fn audit_text_metadata(value: &str) -> serde_json::Value {
    json!({
        "sha256": hex::encode(Sha256::digest(value.as_bytes())),
        "chars": value.chars().count(),
        "redacted": true
    })
}

async fn add_completion_and_remove_trigger_tags(
    state: &AppState,
    review: &ReviewItemRecord,
    patch: &mut DocumentPatch,
    final_run_stage: bool,
) -> Result<()> {
    let settings = get_runtime_settings(&state.pool).await?;
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    let document = client.get_document(review.paperless_document_id).await?;
    let all_tags = client.list_tags().await?;
    let completion = settings
        .workflow
        .tags
        .completion_tag_for_stage(review.stage);
    let trigger = settings.workflow.tags.trigger_tag_for_stage(review.stage);
    let mut ids = patch.tags.clone().unwrap_or(document.tags);
    if let Some(completion_name) = completion {
        let tag = client.ensure_tag(completion_name).await?;
        if !ids.contains(&tag.id) {
            ids.push(tag.id);
        }
    }
    if final_run_stage {
        let tag = client
            .ensure_tag(&settings.workflow.tags.completion_processed)
            .await?;
        if !ids.contains(&tag.id) {
            ids.push(tag.id);
        }
    }
    if let Some(trigger_name) = trigger
        && let Some(tag) = all_tags
            .iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(trigger_name))
    {
        ids.retain(|id| *id != tag.id);
    }
    if final_run_stage
        && let Some(tag) = all_tags.iter().find(|tag| {
            tag.name
                .eq_ignore_ascii_case(&settings.workflow.tags.trigger_process)
        })
    {
        ids.retain(|id| *id != tag.id);
    }
    ids.sort_unstable();
    ids.dedup();
    patch.tags = Some(ids);
    Ok(())
}

async fn sync_paperless_inventory(
    pool: &DbPool,
    client: &PaperlessClient,
    settings: &RuntimeSettings,
) -> Result<Value> {
    let archive_name = settings.paperless.active_archive.clone();
    let sync_started_at = Utc::now();
    let mut tags = client.list_tags().await?;
    for workflow_tag in settings.workflow.tags.all() {
        let tag = client.ensure_tag(workflow_tag).await?;
        if !tags.iter().any(|existing| existing.id == tag.id) {
            tags.push(tag);
        }
    }
    let correspondents = client.list_correspondents().await?;
    let document_types = client.list_document_types().await?;
    let custom_fields = client.list_custom_fields().await.unwrap_or_default();
    let cursor = paperless_sync_cursor(pool, &archive_name).await?;
    let delta_cursor = cursor
        .map(|cursor| cursor - Duration::minutes(settings.paperless.delta_sync_overlap_minutes));
    let (sync_mode, documents) = if settings.paperless.delta_sync_enabled {
        if let Some(cursor) = delta_cursor {
            match client
                .list_documents_modified_since(&cursor.to_rfc3339())
                .await
            {
                Ok(documents) => ("delta", documents),
                Err(_) => ("full_after_delta_error", client.list_documents().await?),
            }
        } else {
            ("full_initial", client.list_documents().await?)
        }
    } else {
        ("full", client.list_documents().await?)
    };

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

    for document in &documents {
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tags.iter().find(|tag| tag.id == *id))
            .map(|tag| tag.name.clone())
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
                document_date: document.created.clone(),
                paperless_modified_at: parse_paperless_datetime(document.modified.as_deref()),
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

fn parse_paperless_datetime(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

async fn paperless_client(pool: &DbPool, config: &AppConfig) -> Result<PaperlessClient> {
    let settings = get_runtime_settings(pool).await?;
    paperless_client_from_settings(pool, config, &settings).await
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
    let metadata = http_client
        .get(&discovery_url)
        .send()
        .await
        .map_err(|error| ApiError::internal(format!("OIDC discovery request failed: {error}")))?
        .error_for_status()
        .map_err(|error| ApiError::internal(format!("OIDC discovery failed: {error}")))?
        .json::<OidcProviderMetadata>()
        .await
        .map_err(|error| ApiError::internal(format!("OIDC discovery parse failed: {error}")))?;
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
        .map_err(|error| ApiError::unauthorized(format!("OIDC code exchange failed: {error}")))?
        .error_for_status()
        .map_err(|error| ApiError::unauthorized(format!("OIDC code exchange failed: {error}")))?
        .json::<OidcTokenResponse>()
        .await
        .map_err(|error| {
            ApiError::unauthorized(format!("OIDC token response parse failed: {error}"))
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
    if value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains('\r')
        && !value.contains('\n')
    {
        Some(value.to_owned())
    } else {
        Some("/".to_owned())
    }
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

fn oidc_roles(config: &AppConfig, username: &str, email: Option<&str>) -> ApiResult<Vec<Role>> {
    if oidc_is_admin(config, username, email) {
        return Ok(vec![
            Role::Admin,
            Role::Operator,
            Role::Reviewer,
            Role::Auditor,
        ]);
    }
    parse_oidc_roles(&config.oidc_default_roles)
        .map_err(|error| ApiError::internal(format!("invalid OIDC role config: {error}")))
}

fn oidc_is_admin(config: &AppConfig, username: &str, email: Option<&str>) -> bool {
    let username = username.to_ascii_lowercase();
    let email = email.map(str::to_ascii_lowercase);
    config
        .oidc_admin_users
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .any(|admin| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_cloud_detection_matches_hosted_endpoint() {
        assert!(is_ollama_cloud("https://ollama.com"));
        assert!(is_ollama_cloud("https://OLLAMA.com/"));
        assert!(!is_ollama_cloud("http://ollama:11434"));
        assert!(!is_ollama_cloud("http://localhost:11434"));
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
            "ai-processed"
        ));

        let already_complete = vec![
            "archivist-ocr".to_owned(),
            "archivist-tags".to_owned(),
            "AI-PROCESSED".to_owned(),
        ];
        assert!(!completion_tag_reconcile_needed(
            &already_complete,
            &stage_tags,
            "ai-processed"
        ));

        let missing_stage = vec!["archivist-ocr".to_owned()];
        assert!(!completion_tag_reconcile_needed(
            &missing_stage,
            &stage_tags,
            "ai-processed"
        ));

        assert!(!completion_tag_reconcile_needed(
            &document_tags,
            &[],
            "ai-processed"
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

    #[test]
    fn oidc_admin_allowlist_gets_admin_roles() {
        let mut config = test_config();
        config.oidc_admin_users = "oidc-admin, admin@example.com".to_owned();

        let roles = oidc_roles(&config, "oidc-admin", None).expect("roles parse");
        assert!(roles.contains(&Role::Admin));
        assert!(roles.contains(&Role::Auditor));

        let email_roles =
            oidc_roles(&config, "someone", Some("admin@example.com")).expect("roles parse");
        assert!(email_roles.contains(&Role::Admin));
    }

    #[test]
    fn oidc_default_roles_are_deduplicated() {
        let mut config = test_config();
        config.oidc_default_roles = "viewer reviewer viewer".to_owned();
        assert_eq!(
            oidc_roles(&config, "user", None).expect("roles parse"),
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
            secret_key: SecretString::from("a 32 byte local secret for tests".to_owned()),
            static_dir: "frontend/dist".to_owned(),
            trust_proxy: false,
            auth_rate_limit: 10,
            auth_rate_limit_window_seconds: 60,
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
        }
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
        let client =
            OllamaClient::new_with_timeout(&base_url, None, std::time::Duration::from_secs(2))
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
        let (key, value) = cookie.trim().split_once('=')?;
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
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
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
            document_date: Some("2020-01-01".to_owned()),
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
