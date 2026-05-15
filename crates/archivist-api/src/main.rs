use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use archivist_ai::{
    AiResponse, AnthropicClient, ChatRequest, OllamaClient, OllamaModel, OpenAiCompatibleClient,
    PromptLanguageContext, TextProvider, parse_choice_suggestion, parse_field_suggestion,
    parse_tag_suggestion, parse_title_suggestion, prompt_for_choice, prompt_for_fields,
    prompt_for_tags, prompt_for_title,
};
use archivist_config::AppConfig;
use archivist_core::{
    AiProviderKind, AuditEventInput, DashboardRange, DocumentChatSource, DocumentPatch, Permission,
    ProcessingMode, ProviderUsageStats, Role, RuntimeSettings, Stage, build_document_chat_prompt,
    detect_document_language, document_chat_snippet, document_chat_terms, roles_have_permission,
    score_document_chat_source, validate_choice_suggestion, validate_field_suggestion,
    validate_tag_suggestion, validate_title_suggestion,
};
use archivist_db::{
    AuthUser, DbPool, DocumentChatCandidate, OidcUserInput, ReviewItemRecord, append_audit,
    connect, consume_oidc_login_state, create_document_chat_session, create_oidc_login_state,
    create_run_with_jobs, create_session, create_user_with_roles, document_chat_session_visible,
    find_api_token, find_session, find_user_for_login, get_backlog_counts,
    get_dashboard_live_status, get_dashboard_stats, get_runtime_settings, has_any_user, hash_token,
    insert_document_chat_message, insert_document_chat_sources, list_allowed_named_entities,
    list_allowed_tag_names, list_audit_events, list_custom_fields, list_document_chat_messages,
    list_document_chat_sessions, list_inventory, list_prompt_usage, list_prompts, list_reviews,
    list_secret_references, list_sessions, list_users, metrics_snapshot as db_metrics_snapshot,
    migrate, queue_missing_stage, record_login_failure, record_login_success, resolve_secret,
    review_decision, revoke_session_by_admin, search_document_chat_candidates, set_user_enabled,
    set_user_roles, update_runtime_settings, update_user_password_hash, upsert_encrypted_secret,
    upsert_inventory_item, upsert_oidc_user, upsert_paperless_custom_field,
    upsert_paperless_named_entity, upsert_paperless_tag,
};
use archivist_paperless::PaperlessClient;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, Params};
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
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
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
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
    };
    let app = router(state);
    let addr: SocketAddr = config
        .http_addr
        .parse()
        .context("parse ARCHIVIST_HTTP_ADDR")?;
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "paperless archivist API listening");
    axum::serve(listener, app).await?;
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
    let protected = Router::new()
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .route("/auth/change-password", post(change_password))
        .route("/auth/sessions", get(sessions))
        .route("/auth/sessions/{id}/revoke", post(revoke_session_endpoint))
        .route("/settings", get(settings).put(update_settings))
        .route("/settings/test-paperless", post(test_paperless))
        .route("/model-providers/test", post(test_provider))
        .route(
            "/model-providers/{name}/models",
            post(model_provider_models),
        )
        .route("/secret-references", get(secret_references))
        .route("/prompts", get(prompts).post(create_prompt_endpoint))
        .route("/prompts/usage", get(prompt_usage))
        .route("/prompts/test", post(test_prompt_endpoint))
        .route("/prompts/{id}/activate", post(activate_prompt_endpoint))
        .route("/paperless/sync-metadata", post(sync_paperless))
        .route("/dashboard", get(dashboard))
        .route("/dashboard/live", get(dashboard_live))
        .route("/workflow/mode", put(update_workflow_mode))
        .route("/inventory", get(inventory))
        .route(
            "/chat/sessions",
            get(chat_sessions).post(create_chat_session),
        )
        .route("/chat/sessions/{id}", get(chat_messages))
        .route("/chat/sessions/{id}/messages", post(post_chat_message))
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
        .route("/audit", get(audit_events))
        .route("/audit/export.csv", get(audit_export))
        .route("/users", get(users).post(create_user))
        .route("/users/{id}/enable", post(enable_user))
        .route("/users/{id}/disable", post(disable_user))
        .route("/users/{id}/roles", post(update_user_roles_endpoint))
        .route("/users/{id}/reset-password", post(reset_user_password))
        .route("/api-tokens", get(api_tokens).post(create_api_token))
        .route("/api-tokens/{id}", delete(revoke_api_token))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let static_dir = state.config.static_dir.clone();
    let spa = ServeDir::new(&static_dir)
        .not_found_service(ServeFile::new(format!("{static_dir}/index.html")));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/api/auth/login", post(login))
        .route("/api/auth/paperless-login", post(paperless_login))
        .route("/api/auth/oidc/config", get(oidc_config))
        .route("/api/auth/oidc/login", get(oidc_login))
        .route("/api/auth/oidc/callback", get(oidc_callback))
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
            "paperless_archivist_audit_events {}\n"
        ),
        snapshot.jobs_queued,
        snapshot.jobs_running,
        snapshot.jobs_failed,
        snapshot.jobs_succeeded,
        snapshot.reviews_pending,
        snapshot.runs_active,
        snapshot.audit_events
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
    Query(query): Query<OidcCallbackQuery>,
) -> ApiResult<Response> {
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

    record_login_success(&state.pool, user.id).await?;
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
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
    let user = find_user_for_login(&state.pool, &request.username).await?;
    let Some(user) = user else {
        record_login_failure(&state.pool, None, &request.username).await?;
        return Err(ApiError::unauthorized("invalid credentials"));
    };
    if user
        .locked_until
        .is_some_and(|locked_until| locked_until > Utc::now())
    {
        return Err(ApiError::unauthorized("invalid credentials"));
    }
    if !user.enabled || !verify_password(&user, &request.password)? {
        record_login_failure(&state.pool, Some(user.id), &request.username).await?;
        return Err(ApiError::unauthorized("invalid credentials"));
    }

    record_login_success(&state.pool, user.id).await?;
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
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
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

    record_login_success(&state.pool, user.id).await?;
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
    auth: Authenticated,
) -> ApiResult<impl IntoResponse> {
    if let (Some(session_id), Some(user_id)) = (auth.0.session_id, auth.0.user_id) {
        archivist_db::revoke_session(&state.pool, session_id, user_id).await?;
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
    provider_secrets: Option<HashMap<String, String>>,
}

async fn update_settings(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(mut request): Json<UpdateSettingsRequest>,
) -> ApiResult<Json<RuntimeSettings>> {
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id = require_user_session(&auth.0, "settings updates require a user session")?;
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
    update_runtime_settings(&state.pool, &request.settings, actor_id).await?;
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
                .map(|field| field.name)
                .collect::<Vec<_>>();
            Ok(prompt_for_fields(
                sample_text,
                &allowed,
                settings.fields.max_fields,
                &language,
            ))
        }
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
        let client = paperless_client(&state.pool, &state.config).await?;
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
    let result = test_ai_provider(&state, &provider).await;
    match result {
        Ok(value) => Ok(Json(json!({ "ok": true, "details": value }))),
        Err(error) => Ok(Json(json!({ "ok": false, "error": error.to_string() }))),
    }
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

async fn sync_paperless(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
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
        },
    )
    .await?;
    Ok(Json(summary))
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
    let mut stats = get_dashboard_stats(&state.pool, range, &counts).await?;
    let settings = get_runtime_settings(&state.pool).await?;
    enrich_provider_usage_costs(&mut stats.provider_usage, &settings);
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

async fn update_workflow_mode(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<UpdateWorkflowModeRequest>,
) -> ApiResult<Json<RuntimeSettings>> {
    require(&auth.0, Permission::WriteSettings)?;
    let actor_id = require_user_session(&auth.0, "workflow mode updates require a user session")?;
    let mut settings = get_runtime_settings(&state.pool).await?;
    settings.workflow.mode = request.mode;
    update_runtime_settings(&state.pool, &settings, actor_id).await?;
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
    Ok(Json(
        json!({ "items": list_inventory(&state.pool, limit, offset).await? }),
    ))
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

async fn trigger_document(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(document_id): Path<i32>,
    Json(request): Json<TriggerRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteRuns)?;
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
    Ok(Json(json!({ "run_id": run_id })))
}

async fn queue_ocr_batch(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
    let settings = get_runtime_settings(&state.pool).await?;
    let created = queue_missing_stage(
        &state.pool,
        Stage::Ocr,
        settings.workflow.mode,
        &auth.0.actor_type,
        &settings.workflow.rules,
    )
    .await?;
    Ok(Json(json!({ "queued": created })))
}

async fn queue_tags_batch(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
    let settings = get_runtime_settings(&state.pool).await?;
    let created = queue_missing_stage(
        &state.pool,
        Stage::Tags,
        settings.workflow.mode,
        &auth.0.actor_type,
        &settings.workflow.rules,
    )
    .await?;
    Ok(Json(json!({ "queued": created })))
}

async fn queue_full_batch(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::WriteBatches)?;
    let settings = get_runtime_settings(&state.pool).await?;
    let mut created = 0;
    for stage in settings.workflow.enabled_stages.iter().copied() {
        created += queue_missing_stage(
            &state.pool,
            stage,
            settings.workflow.mode,
            &auth.0.actor_type,
            &settings.workflow.rules,
        )
        .await?;
    }
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
    Ok(Json(json!({
        "items": list_reviews(&state.pool, query.status.as_deref(), query.limit.unwrap_or(100)).await?
    })))
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
        },
    )
    .await?;

    Ok(Json(json!({
        "ok": failed.is_empty(),
        "succeeded": applied,
        "failed": failed
    })))
}

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
    review_decision(&state.pool, id, "approved", None, actor_id).await?;
    apply_review_patch(&state, id, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

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
    review_decision(&state.pool, id, "rejected", None, actor_id).await?;
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
    require(&auth.0, Permission::ReadAudit)?;
    let events = list_audit_events(&state.pool, 10_000).await?;
    let mut body =
        "id,created_at,event_type,actor_type,actor_id,paperless_document_id,outcome,error_message,metadata\n"
            .to_owned();
    for event in events {
        let row = [
            event.id.to_string(),
            event.created_at.to_rfc3339(),
            event.event_type,
            event.actor_type,
            event.actor_id.unwrap_or_default(),
            event
                .paperless_document_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            event.outcome,
            event.error_message.unwrap_or_default(),
            event
                .metadata
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ]
        .into_iter()
        .map(|value| csv_escape(&value))
        .collect::<Vec<_>>()
        .join(",");
        body.push_str(&row);
        body.push('\n');
    }
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"));
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"paperless-archivist-audit.csv\""),
    );
    Ok(response)
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
}

async fn create_api_token(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<CreateApiTokenRequest>,
) -> ApiResult<Json<Value>> {
    require(&auth.0, Permission::ManageUsers)?;
    let actor_id = require_user_session(&auth.0, "API token creation requires a user session")?;
    validate_api_token_scopes(&request.scopes)?;
    let token = format!("pa_{}", random_token());
    let token_hash = hash_token(&token);
    let id = archivist_db::create_api_token(
        &state.pool,
        &request.name,
        &token_hash,
        &request.scopes,
        actor_id,
        None,
    )
    .await?;
    Ok(Json(json!({ "id": id, "token": token })))
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

async fn apply_review_patch(state: &AppState, review_id: Uuid, actor_id: Uuid) -> Result<()> {
    let Some(review) = archivist_db::pending_review_for_apply(&state.pool, review_id).await? else {
        return Ok(());
    };
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
    client
        .patch_document(review.paperless_document_id, &patch)
        .await?;
    archivist_db::mark_review_applied(&state.pool, review_id, actor_id).await?;
    Ok(())
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
    let documents = client.list_documents().await?;

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
    tx.commit().await?;

    Ok(json!({
        "tags": tags.len(),
        "correspondents": correspondents.len(),
        "document_types": document_types.len(),
        "custom_fields": custom_fields.len(),
        "documents": documents.len()
    }))
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
    let secret_id = settings
        .paperless
        .token_secret_id
        .ok_or_else(|| anyhow!("Paperless token is not configured"))?;
    let token = resolve_secret(pool, &config.secret_key, secret_id)
        .await?
        .ok_or_else(|| anyhow!("Paperless token secret reference does not exist"))?;
    PaperlessClient::new(
        &settings.paperless.base_url,
        token,
        settings.paperless.timeout_seconds,
    )
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
    if auth.csrf_secret_hash.as_deref() != Some(provided_hash.as_str()) {
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

fn validate_api_token_scopes(scopes: &[String]) -> Result<(), ApiError> {
    const ALLOWED: &[&str] = &[
        "runs:read",
        "runs:write",
        "inventory:read",
        "batches:write",
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

    #[test]
    fn validates_api_token_scopes() {
        assert!(
            validate_api_token_scopes(&["runs:read".to_owned(), "users:manage".to_owned()]).is_ok()
        );

        let empty_error = validate_api_token_scopes(&[]).expect_err("empty scopes are rejected");
        assert_eq!(empty_error.status, StatusCode::BAD_REQUEST);

        let invalid_error =
            validate_api_token_scopes(&["admin:*".to_owned()]).expect_err("unknown scopes fail");
        assert_eq!(invalid_error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn maps_manage_users_to_dedicated_scope() {
        assert_eq!(
            scope_for_permission(Permission::ManageUsers),
            "users:manage"
        );
        assert_eq!(scope_for_permission(Permission::UseChat), "chat:write");
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
