use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use archivist_ai::{
    AiResponse, AnthropicClient, ChatRequest, OllamaClient, OllamaModel, OpenAiCompatibleClient,
    PromptLanguageContext, TextProvider, parse_choice_suggestion, parse_field_suggestion,
    parse_tag_suggestion, parse_title_suggestion, prompt_for_choice, prompt_for_fields,
    prompt_for_tags, prompt_for_title,
};
use archivist_config::AppConfig;
use archivist_core::{
    AiProviderKind, AuditEventInput, DashboardProviderCostSummary, DashboardRange, DashboardStats,
    DocumentChatSource, DocumentInventoryItem, DocumentPatch, Permission, ProcessingMode,
    ProviderUsageStats, Role, RuntimeSettings, Stage, WorkflowRules, build_document_chat_prompt,
    detect_document_language, document_chat_snippet, document_chat_terms,
    extract_issue_date_suggestion, roles_have_permission, score_document_chat_source,
    validate_choice_suggestion, validate_document_date_suggestion, validate_field_suggestion,
    validate_tag_suggestion, validate_title_suggestion,
};
use archivist_db::{
    AuthUser, DbPool, DocumentChatCandidate, OidcUserInput, ProviderBucketEntry, ReviewItemRecord,
    append_audit, apply_security_retention, connect, consume_oidc_login_state,
    create_document_chat_session, create_oidc_login_state, create_run_with_jobs, create_session,
    create_user_with_roles, dashboard_bucket_labels, dashboard_range_start,
    document_chat_session_visible, find_api_token, find_session, find_user_for_login,
    get_backlog_counts, get_dashboard_live_status, get_dashboard_stats, get_runtime_settings,
    has_any_user, hash_token, insert_document_chat_message, insert_document_chat_sources,
    list_allowed_named_entities, list_allowed_tag_names, list_audit_events, list_custom_fields,
    list_document_chat_messages, list_document_chat_sessions, list_inventory, list_prompt_usage,
    list_prompts, list_reviews, list_secret_references, list_sessions, list_users,
    metrics_snapshot as db_metrics_snapshot, migrate, paperless_sync_cursor,
    provider_bucket_entries, queue_missing_stage, record_login_failure, record_login_success,
    recover_stale_leases, recover_stuck_runs, recovery_candidates, resolve_secret, review_decision,
    revoke_session_by_admin, rotate_api_token, search_document_chat_candidates, set_user_enabled,
    set_user_roles, update_paperless_sync_cursor, update_runtime_settings,
    update_user_password_hash, upsert_encrypted_secret, upsert_inventory_item, upsert_oidc_user,
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

    let pool = connect(config.database_url.expose_secret()).await?;
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
        .route("/batches/tags", post(queue_tags_batch))
        .route("/batches/full", post(queue_full_batch))
        .route("/reviews", get(reviews))
        .route("/reviews/batch", post(batch_review))
        .route("/reviews/{id}/approve", post(approve_review))
        .route("/reviews/{id}/reject", post(reject_review))
        .route("/reviews/{id}/edit", post(edit_review))
        .route("/operations/recovery", get(recovery_status))
        .route(
            "/operations/recovery/stale-leases",
            post(recover_stale_leases_endpoint),
        )
        .route(
            "/operations/recovery/stuck-runs",
            post(recover_stuck_runs_endpoint),
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
    csrf_token: Option<String>,
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

    let body = Json(MeResponse {
        username: user.username,
        roles: user.roles,
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
    let body = Json(MeResponse {
        username: user.username,
        roles: user.roles,
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
    Ok(Json(MeResponse {
        username: auth.0.username.unwrap_or_else(|| "api-token".to_owned()),
        roles: auth.0.roles,
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
    let mut chat_request =
        build_prompt_test_chat_request(&state, &settings, &request, &sample_text)
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
    let parsed = parse_prompt_test_output(&state, &settings, request.stage, &response.text).await;
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
    state: &AppState,
    settings: &RuntimeSettings,
    request: &TestPromptRequest,
    sample_text: &str,
) -> Result<ChatRequest> {
    let language = PromptLanguageContext::new(
        &detect_document_language(sample_text),
        &settings.tagging.tag_output_language,
    );
    match request.stage {
        Stage::Tags => {
            let allowed = list_allowed_tag_names(&state.pool).await?;
            Ok(prompt_for_tags(
                sample_text,
                &allowed,
                settings.tagging.max_tags,
                &language,
            ))
        }
        Stage::Title => Ok(prompt_for_title(sample_text, &language)),
        Stage::Correspondent => {
            let allowed =
                list_allowed_named_entities(&state.pool, "paperless_correspondents").await?;
            Ok(prompt_for_choice(
                sample_text,
                "correspondent",
                &allowed,
                &language,
            ))
        }
        Stage::DocumentType => {
            let allowed =
                list_allowed_named_entities(&state.pool, "paperless_document_types").await?;
            Ok(prompt_for_choice(
                sample_text,
                "document type",
                &allowed,
                &language,
            ))
        }
        Stage::Fields => {
            let allowed = list_custom_fields(&state.pool)
                .await?
                .into_iter()
                .filter(|field| settings.fields.field_enabled(&field.name))
                .map(|field| field.name)
                .collect::<Vec<_>>();
            Ok(prompt_for_fields(
                sample_text,
                &allowed,
                settings.fields.max_fields,
                &language,
            ))
        }
        Stage::DocumentDate => Ok(ChatRequest {
            model: String::new(),
            system_prompt: "Extract the Paperless document date from explicit issue/invoice/letter date evidence. Return JSON only.".to_owned(),
            user_prompt: format!(
                "Language context: {} ({:.2}).\nDocument text:\n{}\n\nReturn JSON: {{\"date\":\"YYYY-MM-DD\",\"confidence\":0.0,\"evidence\":\"short source snippet\",\"warnings\":[]}}.",
                language.document_language,
                language.document_language_confidence,
                sample_text.chars().take(12_000).collect::<String>()
            ),
            temperature: 0.0,
        }),
        Stage::Ocr => Ok(ChatRequest {
            model: String::new(),
            system_prompt: String::new(),
            user_prompt: format!(
                "Test this OCR prompt against sample text. Return the best OCR text only.\n\nSample text:\n{}",
                sample_text.chars().take(12_000).collect::<String>()
            ),
            temperature: 0.0,
        }),
        Stage::OcrFix => Ok(ChatRequest {
            model: String::new(),
            system_prompt: String::new(),
            user_prompt: format!(
                "Test this OCR post-processing prompt against sample OCR text. Return corrected text only.\n\nOCR text:\n{}",
                sample_text.chars().take(12_000).collect::<String>()
            ),
            temperature: 0.0,
        }),
        Stage::Apply => Err(anyhow!(
            "prompt testing is not supported for stage {}",
            request.stage
        )),
    }
}

async fn parse_prompt_test_output(
    state: &AppState,
    settings: &RuntimeSettings,
    stage: Stage,
    text: &str,
) -> PromptTestParsed {
    match stage {
        Stage::Tags => match parse_tag_suggestion(text) {
            Ok(suggestion) => match validate_tag_suggestion(
                suggestion.clone(),
                &list_allowed_tag_names(&state.pool)
                    .await
                    .unwrap_or_default(),
                &settings.workflow.tags,
                &settings.tagging,
            ) {
                Ok(validated) => PromptTestParsed {
                    parsed: serde_json::to_value(validated).ok(),
                    validation_errors: Vec::new(),
                    warnings: Vec::new(),
                },
                Err(errors) => PromptTestParsed {
                    parsed: serde_json::to_value(suggestion).ok(),
                    validation_errors: errors.into_iter().map(|error| error.to_string()).collect(),
                    warnings: Vec::new(),
                },
            },
            Err(error) => PromptTestParsed {
                parsed: None,
                validation_errors: vec![error.to_string()],
                warnings: Vec::new(),
            },
        },
        Stage::Title => match parse_title_suggestion(text) {
            Ok(suggestion) => match validate_title_suggestion(suggestion.clone(), 160, 0.4) {
                Ok(validated) => PromptTestParsed {
                    parsed: serde_json::to_value(validated).ok(),
                    validation_errors: Vec::new(),
                    warnings: Vec::new(),
                },
                Err(errors) => PromptTestParsed {
                    parsed: serde_json::to_value(suggestion).ok(),
                    validation_errors: errors.into_iter().map(|error| error.to_string()).collect(),
                    warnings: Vec::new(),
                },
            },
            Err(error) => PromptTestParsed {
                parsed: None,
                validation_errors: vec![error.to_string()],
                warnings: Vec::new(),
            },
        },
        Stage::Correspondent | Stage::DocumentType => {
            let table = if stage == Stage::Correspondent {
                "paperless_correspondents"
            } else {
                "paperless_document_types"
            };
            match parse_choice_suggestion(text) {
                Ok(suggestion) => match validate_choice_suggestion(
                    suggestion.clone(),
                    &list_allowed_named_entities(&state.pool, table)
                        .await
                        .unwrap_or_default(),
                    0.4,
                ) {
                    Ok(validated) => PromptTestParsed {
                        parsed: serde_json::to_value(validated).ok(),
                        validation_errors: Vec::new(),
                        warnings: Vec::new(),
                    },
                    Err(errors) => PromptTestParsed {
                        parsed: serde_json::to_value(suggestion).ok(),
                        validation_errors: errors
                            .into_iter()
                            .map(|error| error.to_string())
                            .collect(),
                        warnings: Vec::new(),
                    },
                },
                Err(error) => PromptTestParsed {
                    parsed: None,
                    validation_errors: vec![error.to_string()],
                    warnings: Vec::new(),
                },
            }
        }
        Stage::Fields => match parse_field_suggestion(text) {
            Ok(suggestion) => {
                let allowed = list_custom_fields(&state.pool)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|field| settings.fields.field_enabled(&field.name))
                    .map(|field| field.name)
                    .collect::<Vec<_>>();
                match validate_field_suggestion(
                    suggestion.clone(),
                    &allowed,
                    settings.fields.max_fields,
                    settings.fields.confidence_threshold,
                ) {
                    Ok(validated) => PromptTestParsed {
                        parsed: serde_json::to_value(validated).ok(),
                        validation_errors: Vec::new(),
                        warnings: Vec::new(),
                    },
                    Err(errors) => PromptTestParsed {
                        parsed: serde_json::to_value(suggestion).ok(),
                        validation_errors: errors
                            .into_iter()
                            .map(|error| error.to_string())
                            .collect(),
                        warnings: Vec::new(),
                    },
                }
            }
            Err(error) => PromptTestParsed {
                parsed: None,
                validation_errors: vec![error.to_string()],
                warnings: Vec::new(),
            },
        },
        Stage::DocumentDate => {
            let language = detect_document_language(text);
            match extract_issue_date_suggestion(text, &language) {
                Some(suggestion) => match validate_document_date_suggestion(
                    suggestion.clone(),
                    settings.metadata.document_date_confidence_threshold,
                ) {
                    Ok(validated) => PromptTestParsed {
                        parsed: serde_json::to_value(validated).ok(),
                        validation_errors: Vec::new(),
                        warnings: suggestion.warnings,
                    },
                    Err(errors) => PromptTestParsed {
                        parsed: serde_json::to_value(suggestion).ok(),
                        validation_errors: errors
                            .into_iter()
                            .map(|error| error.to_string())
                            .collect(),
                        warnings: Vec::new(),
                    },
                },
                None => PromptTestParsed {
                    parsed: None,
                    validation_errors: vec!["no document date candidate found".to_owned()],
                    warnings: Vec::new(),
                },
            }
        }
        Stage::Ocr | Stage::OcrFix => PromptTestParsed {
            parsed: Some(json!({ "content": text })),
            validation_errors: Vec::new(),
            warnings: Vec::new(),
        },
        Stage::Apply => PromptTestParsed {
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
        .host_str()
        .ok_or_else(|| ApiError::bad_request("URL is missing a host"))?
        .to_owned();
    let port = parsed.port_or_known_default().unwrap_or(0);
    // `lookup_host` accepts both literal IPs (in `Host`) and DNS names.
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|error| ApiError::bad_request(format!("failed to resolve host: {error}")))?
        .collect();
    if addrs.is_empty() {
        return Err(ApiError::bad_request("host did not resolve to any address"));
    }
    for addr in &addrs {
        if is_private_or_local_ip(addr.ip()) {
            return Err(ApiError::bad_request(
                "URL resolves to a private, loopback, or link-local address",
            ));
        }
    }
    Ok(parsed)
}

fn is_private_or_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_or_local_ipv4(v4),
        IpAddr::V6(v6) => is_private_or_local_ipv6(v6),
    }
}

fn is_private_or_local_ipv4(ip: Ipv4Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() {
        return true;
    }
    // is_private already covers 10/8, 172.16/12, 192.168/16.
    if ip.is_private() {
        return true;
    }
    // is_link_local covers 169.254/16.
    if ip.is_link_local() {
        return true;
    }
    // RFC6598 shared-address space 100.64.0.0/10.
    let octets = ip.octets();
    if octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000 {
        return true;
    }
    false
}

fn is_private_or_local_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    // Mapped IPv4: re-evaluate the embedded v4.
    if let Some(v4) = ip.to_ipv4_mapped()
        && is_private_or_local_ipv4(v4)
    {
        return true;
    }
    // 4-in-6 deprecated form.
    if let Some(v4) = ip.to_ipv4()
        && is_private_or_local_ipv4(v4)
    {
        return true;
    }
    let segments = ip.segments();
    // Link-local fe80::/10
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // Unique-local fc00::/7
    if (segments[0] & 0xfe00) == 0xfc00 {
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
    if provider.kind != AiProviderKind::Ollama {
        return Err(ApiError::bad_request(
            "installed model discovery is only available for Ollama providers",
        ));
    }
    validate_outbound_url(&provider.base_url)
        .await
        .map_err(|error| {
            ApiError::bad_request(format!(
                "Ollama provider base URL rejected: {}",
                error.message
            ))
        })?;
    let client = OllamaClient::new_with_timeout(
        &provider.base_url,
        provider_secret(&state, &provider).await?,
        std::time::Duration::from_secs(10),
    )?;
    let models = client
        .list_models()
        .await
        .map_err(|error| ApiError::internal(format!("Ollama model discovery failed: {error}")))?;
    Ok(Json(OllamaInstalledModelsResponse {
        provider: provider.name,
        models: models.into_iter().map(OllamaInstalledModel::from).collect(),
    }))
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
    let bucket_index_of =
        |bucket: DateTime<Utc>| -> Option<usize> { labels.iter().position(|(b, _)| *b == bucket) };
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
struct PageQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn inventory(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ReadInventory)?;
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let offset = query.offset.unwrap_or(0).max(0);
    let settings = get_runtime_settings(&state.pool).await?;
    let items = list_inventory(&state.pool, limit, offset)
        .await?
        .into_iter()
        .map(|item| inventory_item_with_debug(item, &settings))
        .collect::<Result<Vec<_>>>()?;
    Ok(Json(json!({ "items": items })))
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
    let run_id = create_run_with_jobs(
        &state.pool,
        document_id,
        &stages,
        mode,
        "manual",
        &auth.0.actor_type,
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
async fn queue_tags_batch(
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
        Stage::Tags,
        settings.workflow.mode,
        &auth.0.actor_type,
        &settings.workflow.rules,
        None,
    )
    .await?;
    Span::current().record("queued", created);
    info!(queued = created, "queued missing tagging documents");
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
    let mut created = 0;
    for stage in settings.workflow.enabled_stages.iter().copied() {
        created += queue_missing_stage(
            &state.pool,
            stage,
            settings.workflow.mode,
            &auth.0.actor_type,
            &settings.workflow.rules,
            None,
        )
        .await?;
    }
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
    if expected.as_bytes().len() != provided_hash.as_bytes().len()
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
    async fn validate_outbound_url_rejects_rfc1918() {
        let err = validate_outbound_url("https://10.0.0.5/api")
            .await
            .expect_err("RFC1918 must be rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
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
        let err = validate_outbound_url("http://169.254.169.254/latest/meta-data/")
            .await
            .expect_err("link-local rejected");
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
