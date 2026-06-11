use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use aes_gcm::aead::{Aead, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{Context, Result, anyhow};
use archivist_core::{
    AiArtifactStorageMode, AuditEventInput, BacklogCounts, DashboardBacklogPoint,
    DashboardComparison, DashboardCostBucket, DashboardLiveFailure, DashboardLiveJob,
    DashboardLiveLlmEvent, DashboardLiveRun, DashboardLiveStatus, DashboardRange,
    DashboardStageStatus, DashboardStats, DashboardStatusCount, DashboardTimeBucket,
    DocumentChatSource, DocumentInventoryItem, DuplicateDocument, DuplicateGroup,
    LanguageDetection, NeedsAttentionItem, ProcessingMode, ProviderUsageStats, QualityStats, Role,
    RuntimeSettings, ServiceProcessingStatus, Stage, WorkflowRules, WorkflowSafetyStatus,
    redact_sensitive_json,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, Timelike, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use uuid::Uuid;

pub type DbPool = PgPool;

pub async fn connect(database_url: &str, max_connections: u32) -> Result<DbPool> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await
        .context("connect to PostgreSQL")
}

pub async fn migrate(pool: &DbPool) -> Result<()> {
    let migrations_dir =
        std::env::var("ARCHIVIST_MIGRATIONS_DIR").unwrap_or_else(|_| "migrations".to_owned());
    sqlx::migrate::Migrator::new(Path::new(&migrations_dir))
        .await
        .with_context(|| format!("load database migrations from {migrations_dir}"))?
        .run(pool)
        .await
        .context("run database migrations")
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(digest)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: Uuid,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub enabled: bool,
    pub failed_login_count: i32,
    pub locked_until: Option<DateTime<Utc>>,
    pub roles: Vec<Role>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPrincipal {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub username: String,
    pub roles: Vec<Role>,
    pub csrf_secret_hash: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub id: Uuid,
    pub user_id: Uuid,
    pub username: String,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcLoginState {
    pub nonce: String,
    pub pkce_verifier: String,
    pub return_to: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OidcUserInput<'a> {
    pub provider: &'a str,
    pub subject: &'a str,
    pub username: &'a str,
    /// Must be the VERIFIED email (token had `email_verified=true`) — it
    /// drives account linking and gets persisted onto the user row.
    pub email: Option<&'a str>,
    pub disabled_password_hash: &'a str,
    pub roles: &'a [Role],
    pub allow_username_link: bool,
    /// Gate for the email-match linking branch, mirroring
    /// `allow_username_link`: linking grants the OIDC subject the matched
    /// account's roles permanently, so it must be an explicit opt-in.
    pub allow_email_link: bool,
    /// Degraded ID-token claims (#299): the caller could not derive a
    /// trustworthy identity (no `preferred_username`, no verified email), so
    /// a RETURNING user keeps their current roles instead of having them
    /// replaced by `roles` (which were computed from the raw subject only).
    pub preserve_existing_roles: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiTokenPrincipal {
    pub token_id: Uuid,
    pub name: String,
    pub scopes: Vec<String>,
    pub user_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiTokenView {
    pub id: Uuid,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_by: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserListItem {
    pub id: Uuid,
    pub username: String,
    pub email: Option<String>,
    pub enabled: bool,
    pub roles: Vec<Role>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretReferenceView {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub configured: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: Uuid,
    pub run_id: Uuid,
    pub paperless_document_id: i32,
    pub stage: Stage,
    pub mode: ProcessingMode,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItemRecord {
    pub id: Uuid,
    pub run_id: Uuid,
    pub job_id: Option<Uuid>,
    pub paperless_document_id: i32,
    pub stage: Stage,
    pub status: String,
    pub suggested_patch: Value,
    pub edited_patch: Option<Value>,
    pub validation_warnings: Value,
    pub debug_context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paperless_title: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEventRecord {
    pub id: Uuid,
    pub event_type: String,
    pub actor_type: String,
    pub actor_id: Option<String>,
    pub paperless_document_id: Option<i32>,
    pub outcome: String,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub metadata: Option<Value>,
    pub prev_event_hash: Option<String>,
    pub event_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditIntegrityReport {
    pub ok: bool,
    pub checked_events: i64,
    pub legacy_events: i64,
    pub latest_event_hash: Option<String>,
    pub broken_event_id: Option<Uuid>,
    pub broken_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionResult {
    pub audit_events_deleted: i64,
    pub ai_artifacts_deleted: i64,
    pub ocr_page_cache_deleted: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRecord {
    pub id: Uuid,
    pub stage: Stage,
    pub name: String,
    pub version: i32,
    pub content: String,
    pub output_schema: Option<Value>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptUsageRecord {
    pub prompt_id: Uuid,
    pub run_count: i64,
    pub job_count: i64,
    pub last_used_at: Option<DateTime<Utc>>,
    pub avg_duration_ms: f64,
    pub last_provider: Option<String>,
    pub last_model: Option<String>,
}

/// One row of the prompt A/B experiment evaluation: the review-outcome
/// breakdown for all metadata artifacts stamped with a given
/// `prompt_experiment_group` (see [`get_active_prompt_with_experiment`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptExperimentRecord {
    pub group: String,
    pub total: i64,
    pub approved: i64,
    pub rejected: i64,
    pub edited: i64,
    pub applied: i64,
    pub mean_confidence: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChatSessionRecord {
    pub id: Uuid,
    pub title: String,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChatMessageRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub role: String,
    pub content: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub metadata: Option<Value>,
    pub sources: Vec<DocumentChatSource>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChatCandidate {
    pub paperless_document_id: i32,
    pub title: Option<String>,
    pub original_file_name: Option<String>,
    pub current_tags: Vec<String>,
    pub metadata_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomFieldRecord {
    pub id: i32,
    pub name: String,
    pub data_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub jobs_queued: i64,
    pub jobs_running: i64,
    pub jobs_failed: i64,
    pub jobs_succeeded: i64,
    pub reviews_pending: i64,
    pub runs_active: i64,
    pub audit_events: i64,
    pub selector_runs_total: i64,
    pub selector_documents_queued_total: i64,
    pub job_retries_scheduled_total: i64,
    pub model_errors_total: i64,
    pub apply_success_total: i64,
    pub apply_failure_total: i64,
    pub apply_latency_ms_count: i64,
    pub apply_latency_ms_sum: i64,
    pub apply_latency_ms_p95: i64,
    pub ocr_latency_ms_count: i64,
    pub ocr_latency_ms_p95: i64,
    pub metadata_latency_ms_count: i64,
    pub metadata_latency_ms_p95: i64,
    pub oldest_queued_age_seconds: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryCandidate {
    pub run_id: Uuid,
    pub job_id: Option<Uuid>,
    pub paperless_document_id: i32,
    pub stage: Option<Stage>,
    pub status: String,
    pub lease_owner: Option<String>,
    pub lease_until: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverySummary {
    pub stale_leases_requeued: i64,
    pub stuck_runs_failed: i64,
    pub stuck_runs_completed: i64,
}

pub async fn has_any_user(pool: &DbPool) -> Result<bool> {
    let row = sqlx::query("select exists(select 1 from users) as exists")
        .fetch_one(pool)
        .await?;
    row.try_get("exists").context("read users existence")
}

pub async fn create_user_with_roles(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
    roles: &[Role],
    actor: Option<Uuid>,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query(
        r#"
        insert into users (username, email, password_hash)
        values ($1, $2, $3)
        returning id
        "#,
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;

    for role in roles {
        sqlx::query("insert into user_roles (user_id, role) values ($1, $2)")
            .bind(id)
            .bind(role.to_string())
            .execute(&mut *tx)
            .await?;
    }

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "user.created".to_owned(),
            actor_type: actor.map_or_else(|| "system".to_owned(), |_| "user".to_owned()),
            actor_id: actor.map(|id| id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "user_id": id, "username": username, "roles": roles })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(id)
}

pub async fn list_users(pool: &DbPool) -> Result<Vec<UserListItem>> {
    let rows = sqlx::query(
        r#"
        select u.id, u.username, u.email, u.enabled, u.last_login_at, u.created_at,
               coalesce(array_agg(ur.role order by ur.role) filter (where ur.role is not null), '{}') as roles
          from users u
          left join user_roles ur on ur.user_id = u.id
         group by u.id
         order by u.username
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(user_from_row).collect()
}

fn user_from_row(row: PgRow) -> Result<UserListItem> {
    let roles: Vec<String> = row.try_get("roles")?;
    Ok(UserListItem {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        email: row.try_get("email")?,
        enabled: row.try_get("enabled")?,
        roles: roles
            .iter()
            .map(|role| role.parse())
            .collect::<std::result::Result<Vec<_>, _>>()?,
        last_login_at: row.try_get("last_login_at")?,
        created_at: row.try_get("created_at")?,
    })
}

pub async fn find_user_for_login(
    pool: &DbPool,
    username_or_email: &str,
) -> Result<Option<AuthUser>> {
    let row = sqlx::query(
        r#"
        select u.id, u.username, u.email, u.password_hash, u.enabled, u.failed_login_count,
               u.locked_until,
               coalesce(array_agg(ur.role order by ur.role) filter (where ur.role is not null), '{}') as roles
          from users u
          left join user_roles ur on ur.user_id = u.id
         where lower(u.username) = lower($1)
            or lower(coalesce(u.email, '')) = lower($1)
         group by u.id
        "#,
    )
    .bind(username_or_email)
    .fetch_optional(pool)
    .await?;

    row.map(auth_user_from_row).transpose()
}

pub async fn find_auth_user_by_id(pool: &DbPool, user_id: Uuid) -> Result<Option<AuthUser>> {
    let row = sqlx::query(
        r#"
        select u.id, u.username, u.email, u.password_hash, u.enabled, u.failed_login_count,
               u.locked_until,
               coalesce(array_agg(ur.role order by ur.role) filter (where ur.role is not null), '{}') as roles
          from users u
          left join user_roles ur on ur.user_id = u.id
         where u.id = $1
         group by u.id
        "#,
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    row.map(auth_user_from_row).transpose()
}

fn auth_user_from_row(row: PgRow) -> Result<AuthUser> {
    let roles: Vec<String> = row.try_get("roles")?;
    Ok(AuthUser {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        email: row.try_get("email")?,
        password_hash: row.try_get("password_hash")?,
        enabled: row.try_get("enabled")?,
        failed_login_count: row.try_get("failed_login_count")?,
        locked_until: row.try_get("locked_until")?,
        roles: roles
            .iter()
            .map(|role| role.parse())
            .collect::<std::result::Result<Vec<_>, _>>()?,
    })
}

pub async fn record_login_success(
    pool: &DbPool,
    user_id: Uuid,
    source_ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        update users
           set last_login_at = now(),
               failed_login_count = 0,
               locked_until = null,
               updated_at = now()
         where id = $1
        "#,
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    // Note: the "auth.login_success" / "auth.paperless_login_success" /
    // "auth.oidc_login_success" audit events are emitted by the API layer
    // (so they can carry username + extra metadata). We only update the
    // users row here; the success event itself carries source_ip / user_agent
    // via append_audit at the call site.
    let _ = (source_ip, user_agent);
    tx.commit().await?;
    Ok(())
}

pub async fn record_login_failure(
    pool: &DbPool,
    user_id: Option<Uuid>,
    username: &str,
    source_ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    if let Some(user_id) = user_id {
        sqlx::query(
            r#"
            update users
               set failed_login_count = failed_login_count + 1,
                   locked_until = case
                     when failed_login_count + 1 >= 10 then now() + interval '15 minutes'
                     else locked_until
                   end,
                   updated_at = now()
             where id = $1
            "#,
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    }
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "auth.login_failed".to_owned(),
            actor_type: "anonymous".to_owned(),
            actor_id: Some(username.to_owned()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: None,
            outcome: "failed".to_owned(),
            error_message: Some("invalid credentials".to_owned()),
            source_ip: source_ip.map(str::to_owned),
            user_agent: user_agent.map(str::to_owned),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn create_oidc_login_state(
    pool: &DbPool,
    state_hash: &str,
    nonce: &str,
    pkce_verifier: &str,
    return_to: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from oidc_login_states where expires_at <= now()")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        r#"
        insert into oidc_login_states (state_hash, nonce, pkce_verifier, return_to, expires_at)
        values ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(state_hash)
    .bind(nonce)
    .bind(pkce_verifier)
    .bind(return_to)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn consume_oidc_login_state(
    pool: &DbPool,
    state_hash: &str,
) -> Result<Option<OidcLoginState>> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from oidc_login_states where expires_at <= now()")
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        r#"
        delete from oidc_login_states
         where state_hash = $1
           and expires_at > now()
        returning nonce, pkce_verifier, return_to
        "#,
    )
    .bind(state_hash)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    row.map(|row| {
        Ok(OidcLoginState {
            nonce: row.try_get("nonce")?,
            pkce_verifier: row.try_get("pkce_verifier")?,
            return_to: row.try_get("return_to")?,
        })
    })
    .transpose()
}

pub async fn upsert_oidc_user(pool: &DbPool, input: OidcUserInput<'_>) -> Result<AuthUser> {
    let mut tx = pool.begin().await?;
    let mut linked_existing = false;
    let mut created = false;

    let user_id = if let Some(row) = sqlx::query(
        r#"
        select id
          from users
         where external_auth_provider = $1
           and external_subject = $2
        "#,
    )
    .bind(input.provider)
    .bind(input.subject)
    .fetch_optional(&mut *tx)
    .await?
    {
        row.try_get("id")?
    } else if let Some(row) = sqlx::query(
        r#"
        select id
          from users
         where external_auth_provider is null
           and external_subject is null
           and (
             ($3::boolean and lower(username) = lower($1))
             or ($4::boolean and $2::text is not null and lower(coalesce(email, '')) = lower($2::text))
           )
         order by created_at
         limit 1
        "#,
    )
    .bind(input.username)
    .bind(input.email)
    .bind(input.allow_username_link)
    .bind(input.allow_email_link)
    .fetch_optional(&mut *tx)
    .await?
    {
        let id = row.try_get("id")?;
        sqlx::query(
            r#"
            update users
               set external_auth_provider = $2,
                   external_subject = $3,
                   updated_at = now()
             where id = $1
            "#,
        )
        .bind(id)
        .bind(input.provider)
        .bind(input.subject)
        .execute(&mut *tx)
        .await?;
        linked_existing = true;
        id
    } else {
        created = true;
        insert_oidc_user(&mut tx, &input).await?
    };

    if let Some(email) = input.email {
        let owner = sqlx::query("select id from users where lower(email) = lower($1) limit 1")
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?
            .map(|row| row.try_get::<Uuid, _>("id"))
            .transpose()?;
        if owner.is_none_or(|owner_id| owner_id == user_id) {
            sqlx::query("update users set email = $2, updated_at = now() where id = $1")
                .bind(user_id)
                .bind(email)
                .execute(&mut *tx)
                .await?;
        }
    }

    // Role resolution (#289). On the FIRST link of an existing local account
    // keep the operator's local grants and add the IdP-computed roles, so
    // linking doesn't unexpectedly demote. For a new or RETURNING OIDC user the
    // roles are authoritative-from-IdP: REPLACE with the freshly-computed
    // `input.roles` so removal from ARCHIVIST_OIDC_ADMIN_USERS demotes on the
    // next login (the previous additive merge left stale Admin rows forever).
    // Exception (#299): with `preserve_existing_roles` the ID token was
    // degraded, the computed roles are untrustworthy, and a returning user
    // keeps whatever they already have.
    let previous_roles = if created {
        Vec::new()
    } else {
        load_user_roles_tx(&mut tx, user_id).await?
    };
    let mut roles = if linked_existing {
        let mut merged = previous_roles.clone();
        for role in input.roles {
            if !merged.contains(role) {
                merged.push(role.clone());
            }
        }
        merged
    } else if !created && input.preserve_existing_roles {
        previous_roles.clone()
    } else {
        input.roles.to_vec()
    };
    if roles.is_empty() {
        roles.push(Role::Viewer);
    }
    // Last-admin lockout protection (#299): never let an OIDC role refresh
    // demote the only remaining enabled admin — `ensure_bootstrap_admin` only
    // runs on an empty users table, so that state would be unrecoverable
    // in-band.
    let mut last_admin_protected = false;
    if previous_roles.contains(&Role::Admin)
        && !roles.contains(&Role::Admin)
        && !other_enabled_admin_exists_tx(&mut tx, user_id).await?
    {
        tracing::warn!(
            user_id = %user_id,
            username = input.username,
            "refusing to demote the last remaining enabled admin during OIDC role refresh"
        );
        roles.push(Role::Admin);
        last_admin_protected = true;
    }
    let roles_changed = sorted_role_names(&previous_roles) != sorted_role_names(&roles);
    if roles_changed {
        replace_user_roles_tx(&mut tx, user_id, &roles).await?;
    }

    if !created && !linked_existing && roles_changed {
        // #307: a returning OIDC user's roles were rewritten — the prod
        // admin demotion left zero audit trail, so record before/after.
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: "user.roles_replaced".to_owned(),
                actor_type: "system".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: Some(json!({ "user_id": user_id, "roles": previous_roles })),
                after: Some(json!({ "user_id": user_id, "roles": roles })),
                metadata: Some(json!({
                    "username": input.username,
                    "provider": input.provider,
                    "external_subject_hash": short_hash(input.subject),
                    "last_admin_protected": last_admin_protected
                })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }

    if created || linked_existing {
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: if created {
                    "user.oidc_created".to_owned()
                } else {
                    "user.oidc_linked".to_owned()
                },
                actor_type: "system".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: None,
                after: Some(json!({
                    "user_id": user_id,
                    "username": input.username,
                    "provider": input.provider,
                    "roles": roles
                })),
                metadata: Some(json!({ "external_subject_hash": short_hash(input.subject) })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }

    tx.commit().await?;
    find_auth_user_by_id(pool, user_id)
        .await?
        .ok_or_else(|| anyhow!("OIDC user disappeared after upsert"))
}

async fn insert_oidc_user(
    tx: &mut Transaction<'_, Postgres>,
    input: &OidcUserInput<'_>,
) -> Result<Uuid> {
    let mut username = input.username.to_owned();
    let suffix = short_hash(input.subject);
    let mut email = input.email;
    if let Some(email_value) = email {
        let email_taken = sqlx::query("select 1 from users where lower(email) = lower($1) limit 1")
            .bind(email_value)
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
        if email_taken {
            email = None;
        }
    }

    for attempt in 0..3 {
        let row = sqlx::query(
            r#"
            insert into users (
              username, email, password_hash, external_auth_provider, external_subject
            )
            values ($1, $2, $3, $4, $5)
            on conflict (username) do nothing
            returning id
            "#,
        )
        .bind(&username)
        .bind(email)
        .bind(input.disabled_password_hash)
        .bind(input.provider)
        .bind(input.subject)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(row) = row {
            return row.try_get("id").context("read inserted OIDC user id");
        }

        username = if attempt == 0 {
            format!("{}-{}", input.username, &suffix[..8])
        } else {
            format!("{}-{}{}", input.username, &suffix[..8], attempt)
        };
    }

    Err(anyhow!("could not allocate unique username for OIDC user"))
}

async fn load_user_roles_tx(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<Vec<Role>> {
    let rows = sqlx::query("select role from user_roles where user_id = $1 order by role")
        .bind(user_id)
        .fetch_all(&mut **tx)
        .await?;
    rows.into_iter()
        .map(|row| {
            row.try_get::<String, _>("role")?
                .parse()
                .map_err(Into::into)
        })
        .collect()
}

/// Whether any ENABLED user other than `user_id` holds the admin role — the
/// guard for last-admin demotion (#299). Disabled admins don't count: they
/// cannot log in to recover the system.
async fn other_enabled_admin_exists_tx(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<bool> {
    let row = sqlx::query(
        r#"
        select 1
          from user_roles ur
          join users u on u.id = ur.user_id
         where ur.role = $1
           and ur.user_id <> $2
           and u.enabled
         limit 1
        "#,
    )
    .bind(Role::Admin.to_string())
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.is_some())
}

/// Order-insensitive role-set fingerprint for change detection.
fn sorted_role_names(roles: &[Role]) -> Vec<String> {
    let mut names = roles.iter().map(Role::to_string).collect::<Vec<_>>();
    names.sort();
    names
}

async fn replace_user_roles_tx(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    roles: &[Role],
) -> Result<()> {
    sqlx::query("delete from user_roles where user_id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    for role in roles {
        sqlx::query("insert into user_roles (user_id, role) values ($1, $2)")
            .bind(user_id)
            .bind(role.to_string())
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

fn short_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub async fn create_session(
    pool: &DbPool,
    user_id: Uuid,
    session_hash: &str,
    csrf_secret_hash: &str,
    expires_at: DateTime<Utc>,
) -> Result<Uuid> {
    let id = sqlx::query(
        r#"
        insert into sessions (user_id, session_hash, csrf_secret_hash, expires_at)
        values ($1, $2, $3, $4)
        returning id
        "#,
    )
    .bind(user_id)
    .bind(session_hash)
    .bind(csrf_secret_hash)
    .bind(expires_at)
    .fetch_one(pool)
    .await?
    .try_get("id")?;
    Ok(id)
}

pub async fn find_session(pool: &DbPool, session_hash: &str) -> Result<Option<SessionPrincipal>> {
    let row = sqlx::query(
        r#"
        select s.id as session_id, s.user_id, s.csrf_secret_hash, s.expires_at,
               u.username,
               coalesce(array_agg(ur.role order by ur.role) filter (where ur.role is not null), '{}') as roles
          from sessions s
          join users u on u.id = s.user_id
          left join user_roles ur on ur.user_id = u.id
         where s.session_hash = $1
           and s.revoked_at is null
           and s.expires_at > now()
           and u.enabled = true
         group by s.id, u.username
        "#,
    )
    .bind(session_hash)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = row {
        sqlx::query("update sessions set last_seen_at = now() where id = $1")
            .bind(row.try_get::<Uuid, _>("session_id")?)
            .execute(pool)
            .await?;
        let roles: Vec<String> = row.try_get("roles")?;
        Ok(Some(SessionPrincipal {
            session_id: row.try_get("session_id")?,
            user_id: row.try_get("user_id")?,
            username: row.try_get("username")?,
            roles: roles
                .iter()
                .map(|role| role.parse())
                .collect::<std::result::Result<Vec<_>, _>>()?,
            csrf_secret_hash: row.try_get("csrf_secret_hash")?,
            expires_at: row.try_get("expires_at")?,
        }))
    } else {
        Ok(None)
    }
}

pub async fn revoke_session(
    pool: &DbPool,
    session_id: Uuid,
    actor_id: Uuid,
    source_ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("update sessions set revoked_at = now() where id = $1 and revoked_at is null")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "auth.logout".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "session_id": session_id })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: source_ip.map(str::to_owned),
            user_agent: user_agent.map(str::to_owned),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_sessions(pool: &DbPool, user_id: Option<Uuid>) -> Result<Vec<SessionView>> {
    let rows = if let Some(user_id) = user_id {
        sqlx::query(
            r#"
            select s.id, s.user_id, u.username, s.expires_at, s.revoked_at, s.last_seen_at, s.created_at
              from sessions s
              join users u on u.id = s.user_id
             where s.user_id = $1
             order by s.created_at desc
            "#,
        )
        .bind(user_id)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            r#"
            select s.id, s.user_id, u.username, s.expires_at, s.revoked_at, s.last_seen_at, s.created_at
              from sessions s
              join users u on u.id = s.user_id
             order by s.created_at desc
             limit 500
            "#,
        )
        .fetch_all(pool)
        .await?
    };

    rows.into_iter()
        .map(|row| {
            Ok(SessionView {
                id: row.try_get("id")?,
                user_id: row.try_get("user_id")?,
                username: row.try_get("username")?,
                expires_at: row.try_get("expires_at")?,
                revoked_at: row.try_get("revoked_at")?,
                last_seen_at: row.try_get("last_seen_at")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

pub async fn revoke_session_by_admin(
    pool: &DbPool,
    session_id: Uuid,
    actor_id: Uuid,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("update sessions set revoked_at = now() where id = $1 and revoked_at is null")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "session.revoked".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "session_id": session_id })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_user_enabled(
    pool: &DbPool,
    user_id: Uuid,
    enabled: bool,
    actor_id: Uuid,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let before = sqlx::query("select enabled from users where id = $1")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!("user does not exist"))?
        .try_get::<bool, _>("enabled")?;

    sqlx::query("update users set enabled = $2, updated_at = now() where id = $1")
        .bind(user_id)
        .bind(enabled)
        .execute(&mut *tx)
        .await?;
    if !enabled {
        sqlx::query(
            "update sessions set revoked_at = now() where user_id = $1 and revoked_at is null",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    }
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "user.enabled_changed".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: Some(json!({ "user_id": user_id, "enabled": before })),
            after: Some(json!({ "user_id": user_id, "enabled": enabled })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_user_roles(
    pool: &DbPool,
    user_id: Uuid,
    roles: &[Role],
    actor_id: Uuid,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let before_rows = sqlx::query("select role from user_roles where user_id = $1 order by role")
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;
    let before = before_rows
        .into_iter()
        .map(|row| row.try_get::<String, _>("role"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    sqlx::query("delete from user_roles where user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for role in roles {
        sqlx::query("insert into user_roles (user_id, role) values ($1, $2)")
            .bind(user_id)
            .bind(role.to_string())
            .execute(&mut *tx)
            .await?;
    }
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "user.roles_changed".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: Some(json!({ "user_id": user_id, "roles": before })),
            after: Some(json!({ "user_id": user_id, "roles": roles })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn update_user_password_hash(
    pool: &DbPool,
    user_id: Uuid,
    password_hash: &str,
    actor_id: Uuid,
    event_type: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        update users
           set password_hash = $2,
               password_changed_at = now(),
               failed_login_count = 0,
               locked_until = null,
               updated_at = now()
         where id = $1
        "#,
    )
    .bind(user_id)
    .bind(password_hash)
    .execute(&mut *tx)
    .await?;
    sqlx::query("update sessions set revoked_at = now() where user_id = $1 and revoked_at is null")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: event_type.to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "user_id": user_id, "sessions_revoked": true })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn find_api_token(pool: &DbPool, token_hash: &str) -> Result<Option<ApiTokenPrincipal>> {
    let row = sqlx::query(
        r#"
        update api_tokens
           set last_used_at = now()
         where token_hash = $1
           and revoked_at is null
           and (expires_at is null or expires_at > now())
           -- Neutralize tokens whose creator has been disabled: set_user_enabled
           -- revokes sessions but not API tokens, so without this a disabled
           -- operator's token kept full access. #271
           and exists (
             select 1 from users u
              where u.id = api_tokens.created_by and u.enabled = true
           )
        returning id, name, scopes, created_by
        "#,
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        Ok(ApiTokenPrincipal {
            token_id: row.try_get("id")?,
            name: row.try_get("name")?,
            scopes: row.try_get("scopes")?,
            user_id: row.try_get("created_by")?,
        })
    })
    .transpose()
}

pub async fn list_api_tokens(pool: &DbPool) -> Result<Vec<ApiTokenView>> {
    let rows = sqlx::query(
        r#"
        select id, name, scopes, created_by, expires_at, revoked_at, last_used_at, created_at
          from api_tokens
         order by created_at desc
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(ApiTokenView {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                scopes: row.try_get("scopes")?,
                created_by: row.try_get("created_by")?,
                expires_at: row.try_get("expires_at")?,
                revoked_at: row.try_get("revoked_at")?,
                last_used_at: row.try_get("last_used_at")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

pub async fn create_api_token(
    pool: &DbPool,
    name: &str,
    token_hash: &str,
    scopes: &[String],
    created_by: Uuid,
    expires_at: Option<DateTime<Utc>>,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query(
        r#"
        insert into api_tokens (name, token_hash, scopes, created_by, expires_at)
        values ($1, $2, $3, $4, $5)
        returning id
        "#,
    )
    .bind(name)
    .bind(token_hash)
    .bind(scopes)
    .bind(created_by)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "api_token.created".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(created_by.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(
                json!({ "id": id, "name": name, "scopes": scopes, "expires_at": expires_at }),
            ),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn revoke_api_token(pool: &DbPool, id: Uuid, actor_id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("update api_tokens set revoked_at = now() where id = $1 and revoked_at is null")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "api_token.revoked".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "id": id })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn rotate_api_token(
    pool: &DbPool,
    id: Uuid,
    token_hash: &str,
    actor_id: Uuid,
    expires_at: Option<DateTime<Utc>>,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let existing = sqlx::query(
        r#"
        select name, scopes
          from api_tokens
         where id = $1
           and revoked_at is null
        "#,
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| anyhow!("API token not found or already revoked"))?;
    let name: String = existing.try_get("name")?;
    let scopes: Vec<String> = existing.try_get("scopes")?;
    sqlx::query("update api_tokens set revoked_at = now() where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let new_id: Uuid = sqlx::query(
        r#"
        insert into api_tokens (name, token_hash, scopes, created_by, expires_at)
        values ($1, $2, $3, $4, $5)
        returning id
        "#,
    )
    .bind(format!("{name} rotated"))
    .bind(token_hash)
    .bind(&scopes)
    .bind(actor_id)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "api_token.rotated".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: Some(json!({ "id": id })),
            after: Some(json!({ "id": new_id, "source_id": id, "name": name, "scopes": scopes, "expires_at": expires_at })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(new_id)
}

pub async fn get_runtime_settings(pool: &DbPool) -> Result<RuntimeSettings> {
    let row = sqlx::query("select value from settings where key = 'runtime'")
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else {
        return Ok(RuntimeSettings::default().normalized());
    };
    let value: Value = row.try_get("value")?;
    serde_json::from_value::<RuntimeSettings>(value)
        .map(RuntimeSettings::normalized)
        .context("decode runtime settings")
}

pub async fn update_runtime_settings(
    pool: &DbPool,
    settings: &RuntimeSettings,
    actor_id: Uuid,
) -> Result<()> {
    let after = serde_json::to_value(settings)?;
    let mut tx = pool.begin().await?;
    let before = sqlx::query("select value from settings where key = 'runtime'")
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|row| row.try_get::<Value, _>("value").ok());
    sqlx::query(
        r#"
        insert into settings (key, value, updated_by, updated_at)
        values ('runtime', $1, $2, now())
        on conflict (key)
        do update set value = excluded.value,
                      updated_by = excluded.updated_by,
                      updated_at = now()
        "#,
    )
    .bind(&after)
    .bind(actor_id)
    .execute(&mut *tx)
    .await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "settings.updated".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before,
            after: Some(after),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_prompts(pool: &DbPool) -> Result<Vec<PromptRecord>> {
    let rows = sqlx::query(
        r#"
        select id, stage, name, version, content, output_schema, active, created_at
          from prompts
         order by stage, name, version desc
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(prompt_from_row).collect()
}

pub async fn get_active_prompt(pool: &DbPool, stage: Stage) -> Result<Option<PromptRecord>> {
    let row = sqlx::query(
        r#"
        select id, stage, name, version, content, output_schema, active, created_at
          from prompts
         where stage = $1 and active = true and experiment_group is null
         order by created_at desc
         limit 1
        "#,
    )
    .bind(stage.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(prompt_from_row).transpose()
}

/// A/B-experiment-aware variant of [`get_active_prompt`].
///
/// When two active prompts exist for the same (stage, name) — one with
/// `experiment_group='A'`, one with `experiment_group='B'` — picks the
/// variant deterministically from `run_id`. Falls back to the
/// experiment-group-less default (i.e. `get_active_prompt`'s row)
/// when no A/B pair is configured. Returns the variant marker
/// alongside the prompt so the worker can stamp it into
/// `ai_artifacts.normalized.prompt_experiment_group` for downstream
/// accuracy analysis.
pub async fn get_active_prompt_with_experiment(
    pool: &DbPool,
    stage: Stage,
    run_id: Uuid,
) -> Result<Option<(PromptRecord, Option<String>)>> {
    let rows = sqlx::query(
        r#"
        select id, stage, name, version, content, output_schema, active, created_at,
               experiment_group
          from prompts
         where stage = $1 and active = true
         order by case when experiment_group is null then 0 else 1 end,
                  experiment_group nulls first,
                  created_at desc
        "#,
    )
    .bind(stage.to_string())
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }

    let mut default_row: Option<PgRow> = None;
    let mut group_a: Option<PgRow> = None;
    let mut group_b: Option<PgRow> = None;
    for row in rows {
        let group: Option<String> = row.try_get("experiment_group").ok();
        match group.as_deref() {
            None if default_row.is_none() => default_row = Some(row),
            Some("A") => group_a = Some(row),
            Some("B") => group_b = Some(row),
            _ => {}
        }
    }

    let bucket = run_id.as_u128() % 2;
    match (group_a, group_b, default_row) {
        (Some(a), Some(b), _) => {
            // Both groups configured → deterministic 50/50 split on run_id.
            let (row, label) = if bucket == 0 { (a, "A") } else { (b, "B") };
            Ok(Some((prompt_from_row(row)?, Some(label.to_owned()))))
        }
        (_, _, Some(row)) => Ok(Some((prompt_from_row(row)?, None))),
        (Some(row), None, None) => Ok(Some((prompt_from_row(row)?, Some("A".to_owned())))),
        (None, Some(row), None) => Ok(Some((prompt_from_row(row)?, Some("B".to_owned())))),
        (None, None, None) => Ok(None),
    }
}

pub async fn list_prompt_usage(pool: &DbPool) -> Result<Vec<PromptUsageRecord>> {
    let rows = sqlx::query(
        r#"
        select ai.prompt_id,
               count(distinct ai.run_id)::bigint as run_count,
               count(distinct ai.job_id)::bigint as job_count,
               max(ai.created_at) as last_used_at,
               coalesce(avg(ai.duration_ms), 0)::double precision as avg_duration_ms,
               (
                 select latest.provider
                   from ai_artifacts latest
                  where latest.prompt_id = ai.prompt_id
                  order by latest.created_at desc
                  limit 1
               ) as last_provider,
               (
                 select latest.model
                   from ai_artifacts latest
                  where latest.prompt_id = ai.prompt_id
                  order by latest.created_at desc
                  limit 1
               ) as last_model
          from ai_artifacts ai
         where ai.prompt_id is not null
         group by ai.prompt_id
         order by max(ai.created_at) desc
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(PromptUsageRecord {
                prompt_id: row.try_get("prompt_id")?,
                run_count: row.try_get("run_count")?,
                job_count: row.try_get("job_count")?,
                last_used_at: row.try_get("last_used_at")?,
                avg_duration_ms: row.try_get("avg_duration_ms")?,
                last_provider: row.try_get("last_provider")?,
                last_model: row.try_get("last_model")?,
            })
        })
        .collect()
}

/// Aggregate prompt A/B-experiment outcomes by `prompt_experiment_group`.
///
/// The group label is stamped into `ai_artifacts.normalized_output ->>
/// 'prompt_experiment_group'` by the metadata worker. We dedupe to the latest
/// artifact per (run, job) — guarding against retries — and join those jobs to
/// their `review_items` to count review statuses per group. `mean_confidence`
/// is the average of the per-artifact mean over the available sub-suggestion
/// confidences (title, document_type, correspondent, document_date, tags,
/// fields). Read-only; no migration.
pub async fn list_prompt_experiments(pool: &DbPool) -> Result<Vec<PromptExperimentRecord>> {
    let rows = sqlx::query(
        r#"
        with art_groups as (
            select distinct on (run_id, job_id)
                   run_id,
                   job_id,
                   normalized_output->>'prompt_experiment_group' as grp,
                   (
                     select avg(c)
                       from (values
                         ((normalized_output#>>'{title,confidence}')::double precision),
                         ((normalized_output#>>'{document_type,confidence}')::double precision),
                         ((normalized_output#>>'{correspondent,confidence}')::double precision),
                         ((normalized_output#>>'{document_date,confidence}')::double precision),
                         ((normalized_output#>>'{tags,confidence}')::double precision),
                         ((normalized_output#>>'{fields,confidence}')::double precision)
                       ) as t(c)
                      where c is not null
                   ) as conf
              from ai_artifacts
             where normalized_output ? 'prompt_experiment_group'
               and normalized_output->>'prompt_experiment_group' is not null
             order by run_id, job_id, created_at desc
        ),
        counts as (
            select ag.grp,
                   count(ri.id) as total,
                   count(ri.id) filter (where ri.status = 'approved') as approved,
                   count(ri.id) filter (where ri.status = 'rejected') as rejected,
                   count(ri.id) filter (where ri.status = 'edited') as edited,
                   count(ri.id) filter (where ri.status = 'applied') as applied
              from art_groups ag
              left join review_items ri
                on ri.run_id = ag.run_id and ri.job_id = ag.job_id
             group by ag.grp
        ),
        conf as (
            select grp, avg(conf)::double precision as mean_confidence
              from art_groups
             group by grp
        )
        select c.grp as grp,
               c.total as total,
               c.approved as approved,
               c.rejected as rejected,
               c.edited as edited,
               c.applied as applied,
               cf.mean_confidence as mean_confidence
          from counts c
          join conf cf using (grp)
         order by c.grp
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(PromptExperimentRecord {
                group: row.try_get("grp")?,
                total: row.try_get("total")?,
                approved: row.try_get("approved")?,
                rejected: row.try_get("rejected")?,
                edited: row.try_get("edited")?,
                applied: row.try_get("applied")?,
                mean_confidence: row.try_get("mean_confidence")?,
            })
        })
        .collect()
}

pub async fn create_prompt(
    pool: &DbPool,
    stage: Stage,
    name: &str,
    content: &str,
    output_schema: Option<Value>,
    actor_id: Uuid,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let version: i32 = sqlx::query(
        "select coalesce(max(version), 0) + 1 as version from prompts where stage = $1 and name = $2",
    )
    .bind(stage.to_string())
    .bind(name)
    .fetch_one(&mut *tx)
    .await?
    .try_get("version")?;
    let id: Uuid = sqlx::query(
        r#"
        insert into prompts (stage, name, version, content, output_schema, active, created_by)
        values ($1, $2, $3, $4, $5, false, $6)
        returning id
        "#,
    )
    .bind(stage.to_string())
    .bind(name)
    .bind(version)
    .bind(content)
    .bind(&output_schema)
    .bind(actor_id)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "prompt.created".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(
                json!({ "prompt_id": id, "stage": stage, "name": name, "version": version }),
            ),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn activate_prompt(pool: &DbPool, prompt_id: Uuid, actor_id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("select stage, name, version from prompts where id = $1")
        .bind(prompt_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!("prompt does not exist"))?;
    let stage: String = row.try_get("stage")?;
    let name: String = row.try_get("name")?;
    let version: i32 = row.try_get("version")?;

    sqlx::query("update prompts set active = false where stage = $1 and name = $2")
        .bind(&stage)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update prompts set active = true where id = $1")
        .bind(prompt_id)
        .execute(&mut *tx)
        .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "prompt.activated".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(
                json!({ "prompt_id": prompt_id, "stage": stage, "name": name, "version": version }),
            ),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

fn prompt_from_row(row: PgRow) -> Result<PromptRecord> {
    let stage: String = row.try_get("stage")?;
    Ok(PromptRecord {
        id: row.try_get("id")?,
        stage: stage.parse()?,
        name: row.try_get("name")?,
        version: row.try_get("version")?,
        content: row.try_get("content")?,
        output_schema: row.try_get("output_schema")?,
        active: row.try_get("active")?,
        created_at: row.try_get("created_at")?,
    })
}

pub async fn list_secret_references(pool: &DbPool) -> Result<Vec<SecretReferenceView>> {
    let rows = sqlx::query(
        r#"
        select id, name, kind, reference, created_at, updated_at
          from secret_references
         order by name
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let reference: Value = row.try_get("reference")?;
            Ok(SecretReferenceView {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                kind: row.try_get("kind")?,
                configured: !reference.is_null(),
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect()
}

pub async fn upsert_encrypted_secret(
    pool: &DbPool,
    secret_key: &SecretString,
    name: &str,
    secret: &SecretString,
    actor_id: Uuid,
) -> Result<Uuid> {
    let encrypted = encrypt_secret(secret_key, secret.expose_secret())?;
    let reference = json!({ "ciphertext": encrypted });
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query(
        r#"
        insert into secret_references (name, kind, reference, created_by, updated_by)
        values ($1, 'encrypted_value', $2, $3, $3)
        on conflict (name)
        do update set kind = excluded.kind,
                      reference = excluded.reference,
                      updated_by = excluded.updated_by,
                      updated_at = now()
        returning id
        "#,
    )
    .bind(name)
    .bind(reference)
    .bind(actor_id)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "secret.changed".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(
                json!({ "secret_reference_id": id, "name": name, "kind": "encrypted_value" }),
            ),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn resolve_secret(
    pool: &DbPool,
    secret_key: &SecretString,
    secret_id: Uuid,
) -> Result<Option<SecretString>> {
    let Some(row) = sqlx::query("select kind, reference from secret_references where id = $1")
        .bind(secret_id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let kind: String = row.try_get("kind")?;
    let reference: Value = row.try_get("reference")?;
    let value = match kind.as_str() {
        "encrypted_value" => {
            let ciphertext = reference
                .get("ciphertext")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("encrypted secret reference is missing ciphertext"))?;
            decrypt_secret(secret_key, ciphertext)?
        }
        "env" => {
            let name = reference
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("env secret reference is missing name"))?;
            std::env::var(name).context("read secret from environment")?
        }
        "mounted_file" | "docker_secret" | "kubernetes_secret" => {
            let path = reference
                .get("path")
                .and_then(Value::as_str)
                .or_else(|| reference.get("name").and_then(Value::as_str))
                .ok_or_else(|| anyhow!("file secret reference is missing path/name"))?;
            let resolved = if kind == "docker_secret" && !path.starts_with('/') {
                format!("/run/secrets/{path}")
            } else {
                path.to_owned()
            };
            // File-backed secrets sit on the async hot path; use tokio's non-blocking read so we
            // never stall an executor thread on disk I/O.
            tokio::fs::read_to_string(resolved).await?.trim().to_owned()
        }
        other => return Err(anyhow!("unsupported secret reference kind: {other}")),
    };
    Ok(Some(SecretString::from(value)))
}

fn encrypt_secret(secret_key: &SecretString, plaintext: &str) -> Result<String> {
    let key_bytes = Sha256::digest(secret_key.expose_secret().as_bytes());
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_bytes())
        .map_err(|_| anyhow!("encrypt secret"))?;
    let mut packed = nonce_bytes.to_vec();
    packed.extend(ciphertext);
    Ok(BASE64.encode(packed))
}

fn decrypt_secret(secret_key: &SecretString, ciphertext: &str) -> Result<String> {
    let packed = BASE64
        .decode(ciphertext)
        .context("decode encrypted secret")?;
    if packed.len() < 13 {
        return Err(anyhow!("encrypted secret is too short"));
    }
    let (nonce, body) = packed.split_at(12);
    let key_bytes = Sha256::digest(secret_key.expose_secret().as_bytes());
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce), body)
        .map_err(|_| anyhow!("decrypt secret"))?;
    String::from_utf8(plaintext).context("secret is not utf-8")
}

pub async fn upsert_paperless_tag(
    tx: &mut Transaction<'_, Postgres>,
    id: i32,
    name: &str,
    slug: Option<&str>,
    color: Option<&str>,
    is_workflow: bool,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into paperless_tags (id, name, slug, color, is_workflow, last_seen_at, updated_at)
        values ($1, $2, $3, $4, $5, now(), now())
        on conflict (id)
        do update set name = excluded.name,
                      slug = excluded.slug,
                      color = excluded.color,
                      is_workflow = excluded.is_workflow,
                      last_seen_at = now(),
                      updated_at = now()
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(slug)
    .bind(color)
    .bind(is_workflow)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn upsert_paperless_named_entity(
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
    id: i32,
    name: &str,
) -> Result<()> {
    let table = match table {
        "paperless_correspondents" => "paperless_correspondents",
        "paperless_document_types" => "paperless_document_types",
        "paperless_custom_fields" => "paperless_custom_fields",
        _ => return Err(anyhow!("unsupported metadata table: {table}")),
    };
    let sql = format!(
        r#"
        insert into {table} (id, name, last_seen_at, updated_at)
        values ($1, $2, now(), now())
        on conflict (id)
        do update set name = excluded.name,
                      last_seen_at = now(),
                      updated_at = now()
        "#
    );
    // SAFETY: `sql` is a static literal built above with no user-controlled
    // interpolation; only bind parameters carry caller data.
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .bind(name)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn upsert_paperless_custom_field(
    tx: &mut Transaction<'_, Postgres>,
    id: i32,
    name: &str,
    data_type: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into paperless_custom_fields (id, name, data_type, last_seen_at, updated_at)
        values ($1, $2, $3, now(), now())
        on conflict (id)
        do update set name = excluded.name,
                      data_type = excluded.data_type,
                      last_seen_at = now(),
                      updated_at = now()
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(data_type)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryUpsert {
    pub paperless_document_id: i32,
    pub title: Option<String>,
    pub original_file_name: Option<String>,
    pub current_tags: Vec<String>,
    pub current_tag_ids: Vec<i32>,
    pub correspondent_id: Option<i32>,
    pub document_type_id: Option<i32>,
    pub document_date: Option<String>,
    pub paperless_modified_at: Option<DateTime<Utc>>,
    pub has_ocr_completion_tag: bool,
    pub has_tagging_completion_tag: bool,
    pub has_full_completion_tag: bool,
}

pub async fn upsert_inventory_item(
    tx: &mut Transaction<'_, Postgres>,
    item: &InventoryUpsert,
) -> Result<()> {
    let ocr_status = if item.has_ocr_completion_tag {
        "succeeded"
    } else {
        "unknown"
    };
    let tagging_status = if item.has_tagging_completion_tag {
        "succeeded"
    } else {
        "unknown"
    };
    let complete = item.has_full_completion_tag;
    sqlx::query(
        r#"
        insert into document_inventory (
          paperless_document_id, title, original_file_name, current_tags, current_tag_ids,
          correspondent_id, document_type_id, document_date, paperless_modified_at,
          has_ocr_completion_tag, has_tagging_completion_tag, has_full_completion_tag,
          ocr_status, tagging_status, complete, last_seen_at, updated_at
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, now(), now())
        on conflict (paperless_document_id)
        do update set title = excluded.title,
                      original_file_name = excluded.original_file_name,
                      current_tags = excluded.current_tags,
                      current_tag_ids = excluded.current_tag_ids,
                      correspondent_id = excluded.correspondent_id,
                      document_type_id = excluded.document_type_id,
                      document_date = excluded.document_date,
                      paperless_modified_at = excluded.paperless_modified_at,
                      has_ocr_completion_tag = excluded.has_ocr_completion_tag,
                      has_tagging_completion_tag = excluded.has_tagging_completion_tag,
                      has_full_completion_tag = excluded.has_full_completion_tag,
                      ocr_status = case when excluded.has_ocr_completion_tag then 'succeeded' else document_inventory.ocr_status end,
                      tagging_status = case when excluded.has_tagging_completion_tag then 'succeeded' else document_inventory.tagging_status end,
                      complete = excluded.has_full_completion_tag,
                      last_seen_at = now(),
                      updated_at = now()
        "#,
    )
    .bind(item.paperless_document_id)
    .bind(&item.title)
    .bind(&item.original_file_name)
    .bind(&item.current_tags)
    .bind(&item.current_tag_ids)
    .bind(item.correspondent_id)
    .bind(item.document_type_id)
    .bind(&item.document_date)
    .bind(item.paperless_modified_at)
    .bind(item.has_ocr_completion_tag)
    .bind(item.has_tagging_completion_tag)
    .bind(item.has_full_completion_tag)
    .bind(ocr_status)
    .bind(tagging_status)
    .bind(complete)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn paperless_sync_cursor(
    pool: &DbPool,
    archive_name: &str,
) -> Result<Option<DateTime<Utc>>> {
    let row =
        sqlx::query("select last_delta_cursor from paperless_sync_state where archive_name = $1")
            .bind(archive_name)
            .fetch_optional(pool)
            .await?;
    Ok(row
        .map(|row| row.try_get("last_delta_cursor"))
        .transpose()?)
}

pub async fn update_paperless_sync_cursor(
    tx: &mut Transaction<'_, Postgres>,
    archive_name: &str,
    mode: &str,
    cursor: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into paperless_sync_state (archive_name, last_sync_at, last_delta_cursor, last_mode, updated_at)
        values ($1, now(), $2, $3, now())
        on conflict (archive_name)
        do update set last_sync_at = excluded.last_sync_at,
                      last_delta_cursor = excluded.last_delta_cursor,
                      last_mode = excluded.last_mode,
                      updated_at = now()
        "#,
    )
    .bind(archive_name)
    .bind(cursor)
    .bind(mode)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn claim_notification_delivery(
    pool: &DbPool,
    event_key: &str,
    cooldown_minutes: i32,
) -> Result<bool> {
    let row = sqlx::query(
        r#"
        insert into notification_state (event_key, last_sent_at, updated_at)
        values ($1, now(), now())
        on conflict (event_key)
        do update set last_sent_at = excluded.last_sent_at,
                      updated_at = now()
        where notification_state.last_sent_at < now() - make_interval(mins => $2)
        returning last_sent_at
        "#,
    )
    .bind(event_key)
    .bind(cooldown_minutes)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

pub async fn record_document_language(
    pool: &DbPool,
    paperless_document_id: i32,
    detection: &LanguageDetection,
    run_id: Option<Uuid>,
    job_id: Option<Uuid>,
    actor: &str,
) -> Result<()> {
    let existing = sqlx::query(
        r#"
        select detected_language, detected_language_confidence, detected_language_source
          from document_inventory
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .fetch_optional(pool)
    .await?;

    let existing_language = existing
        .as_ref()
        .and_then(|row| row.try_get::<Option<String>, _>("detected_language").ok())
        .flatten();
    let existing_confidence = existing
        .as_ref()
        .and_then(|row| {
            row.try_get::<Option<f32>, _>("detected_language_confidence")
                .ok()
        })
        .flatten();
    let should_update = match (&existing_language, existing_confidence) {
        (Some(language), Some(confidence))
            if language == &detection.language && confidence + 0.01 >= detection.confidence =>
        {
            false
        }
        _ => detection.confidence > 0.0 || existing_language.is_none(),
    };
    if !should_update {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        update document_inventory
           set detected_language = $2,
               detected_language_confidence = $3,
               detected_language_source = $4,
               detected_language_updated_at = now(),
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .bind(&detection.language)
    .bind(detection.confidence)
    .bind(&detection.source)
    .execute(&mut *tx)
    .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "document.language_detected".to_owned(),
            actor_type: actor.to_owned(),
            actor_id: None,
            run_id,
            job_id,
            paperless_document_id: Some(paperless_document_id),
            before: Some(json!({
                "language": existing_language,
                "confidence": existing_confidence
            })),
            after: Some(json!({
                "language": detection.language,
                "confidence": detection.confidence,
                "source": detection.source
            })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_allowed_tag_names(pool: &DbPool) -> Result<Vec<String>> {
    let rows =
        sqlx::query("select name from paperless_tags where is_workflow = false order by name")
            .fetch_all(pool)
            .await?;
    rows.into_iter()
        .map(|row| row.try_get("name").context("tag name"))
        .collect()
}

pub async fn list_allowed_named_entities(pool: &DbPool, table: &str) -> Result<Vec<String>> {
    let table = match table {
        "paperless_correspondents" => "paperless_correspondents",
        "paperless_document_types" => "paperless_document_types",
        _ => return Err(anyhow!("unsupported metadata table: {table}")),
    };
    // SAFETY: `table` is matched to a closed allow-list of literal table names above.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "select name from {table} order by name"
    )))
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get("name").context("entity name"))
        .collect()
}

pub async fn list_custom_fields(pool: &DbPool) -> Result<Vec<CustomFieldRecord>> {
    let rows = sqlx::query("select id, name, data_type from paperless_custom_fields order by name")
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(CustomFieldRecord {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                data_type: row.try_get("data_type")?,
            })
        })
        .collect()
}

pub async fn custom_field_ids_for_names(
    pool: &DbPool,
    names: &[String],
) -> Result<Vec<(String, i32, Option<String>)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "select name, id, data_type from paperless_custom_fields where lower(name) = any($1) order by name",
    )
    .bind(
        names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<Vec<_>>(),
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("name")?,
                row.try_get("id")?,
                row.try_get("data_type")?,
            ))
        })
        .collect()
}

pub async fn tag_ids_for_names(pool: &DbPool, names: &[String]) -> Result<Vec<i32>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let rows =
        sqlx::query("select id from paperless_tags where lower(name) = any($1) order by name")
            .bind(
                names
                    .iter()
                    .map(|name| name.to_ascii_lowercase())
                    .collect::<Vec<_>>(),
            )
            .fetch_all(pool)
            .await?;
    rows.into_iter()
        .map(|row| row.try_get("id").context("tag id"))
        .collect()
}

/// Like `tag_ids_for_names` but also returns the matched name alongside each id, so callers
/// can diff a requested name list against what was actually known in the local mirror
/// (e.g. to decide whether to create unknown tags in Paperless or drop them).
pub async fn tag_id_pairs_for_names(pool: &DbPool, names: &[String]) -> Result<Vec<(String, i32)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "select name, id from paperless_tags where lower(name) = any($1) order by name",
    )
    .bind(
        names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<Vec<_>>(),
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| Ok((row.try_get("name")?, row.try_get("id")?)))
        .collect()
}

pub async fn named_entity_id_for_name(
    pool: &DbPool,
    table: &str,
    name: &str,
) -> Result<Option<i32>> {
    let table = match table {
        "paperless_correspondents" => "paperless_correspondents",
        "paperless_document_types" => "paperless_document_types",
        _ => return Err(anyhow!("unsupported metadata table: {table}")),
    };
    // SAFETY: `table` is matched to a closed allow-list of literal table names above.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "select id from {table} where lower(name) = lower($1)"
    )))
    .bind(name)
    .fetch_optional(pool)
    .await?;
    row.map(|row| row.try_get("id").context("entity id"))
        .transpose()
}

pub async fn get_backlog_counts(pool: &DbPool) -> Result<BacklogCounts> {
    let row = sqlx::query(
        r#"
        select
          count(*)::bigint as total_documents,
          count(*) filter (where complete)::bigint as complete,
          count(*) filter (where ocr_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_ocr,
          count(*) filter (where tagging_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_tagging,
          count(*) filter (where title_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_title,
          count(*) filter (where correspondent_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_correspondent,
          count(*) filter (where document_type_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_document_type,
          count(*) filter (where document_date_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_document_date,
          count(*) filter (where fields_status not in ('succeeded', 'skipped', 'not_needed'))::bigint as missing_fields,
          count(*) filter (where needs_review or current_run_status = 'waiting_review')::bigint as waiting_review,
          count(*) filter (where ocr_status = 'failed' or tagging_status = 'failed' or title_status = 'failed' or correspondent_status = 'failed' or document_type_status = 'failed' or document_date_status = 'failed' or fields_status = 'failed')::bigint as failed,
          count(*) filter (where current_run_status in ('queued', 'running', 'applying'))::bigint as running,
          count(*) filter (where last_run_id is null)::bigint as never_processed
        from document_inventory
        "#,
    )
    .fetch_one(pool)
    .await?;

    Ok(BacklogCounts {
        total_documents: row.try_get("total_documents")?,
        complete: row.try_get("complete")?,
        missing_ocr: row.try_get("missing_ocr")?,
        missing_tagging: row.try_get("missing_tagging")?,
        missing_title: row.try_get("missing_title")?,
        missing_correspondent: row.try_get("missing_correspondent")?,
        missing_document_type: row.try_get("missing_document_type")?,
        missing_document_date: row.try_get("missing_document_date")?,
        missing_fields: row.try_get("missing_fields")?,
        waiting_review: row.try_get("waiting_review")?,
        failed: row.try_get("failed")?,
        running: row.try_get("running")?,
        never_processed: row.try_get("never_processed")?,
    })
}

pub async fn record_dashboard_snapshot(pool: &DbPool, counts: &BacklogCounts) -> Result<()> {
    sqlx::query(
        r#"
        insert into dashboard_snapshots (
          total_documents, complete, missing_ocr, missing_tagging, missing_title,
          missing_correspondent, missing_document_type, missing_document_date, missing_fields, waiting_review,
          failed, running, never_processed
        )
        select $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13
        where not exists (
          select 1 from dashboard_snapshots
           where captured_at >= now() - interval '5 minutes'
        )
        "#,
    )
    .bind(counts.total_documents)
    .bind(counts.complete)
    .bind(counts.missing_ocr)
    .bind(counts.missing_tagging)
    .bind(counts.missing_title)
    .bind(counts.missing_correspondent)
    .bind(counts.missing_document_type)
    .bind(counts.missing_document_date)
    .bind(counts.missing_fields)
    .bind(counts.waiting_review)
    .bind(counts.failed)
    .bind(counts.running)
    .bind(counts.never_processed)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ActivitySummary {
    jobs_created: i64,
    jobs_succeeded: i64,
    jobs_failed: i64,
}

pub async fn get_dashboard_stats(
    pool: &DbPool,
    range: DashboardRange,
    counts: &BacklogCounts,
    now: DateTime<Utc>,
    start: DateTime<Utc>,
) -> Result<DashboardStats> {
    // Snapshots are written by the worker tick loop (see archivist-worker::run_worker) so the
    // /dashboard read path no longer fires a write per poll. The 5-minute existence guard inside
    // `record_dashboard_snapshot` keeps the table de-duplicated regardless.
    let activity = activity_summary(pool, start, now).await?;
    let previous = if let Some(duration) = range.duration() {
        Some(activity_summary(pool, start - duration, start).await?)
    } else {
        None
    };
    let comparison = dashboard_comparison(pool, start, counts, activity, previous).await?;
    let job_status = status_counts(pool, StatusTable::Jobs).await?;
    let running_jobs = job_status
        .iter()
        .find(|item| item.status == "running")
        .map(|item| item.count)
        .unwrap_or_default();
    let completed_or_failed = activity.jobs_succeeded + activity.jobs_failed;
    let failure_rate = if completed_or_failed == 0 {
        0.0
    } else {
        activity.jobs_failed as f64 / completed_or_failed as f64
    };
    let completion_rate = if counts.total_documents == 0 {
        0.0
    } else {
        counts.complete as f64 / counts.total_documents as f64
    };
    let mttc_seconds = mttc_seconds_value(pool, start, now).await?;
    let p95_stage_duration_ms = p95_stage_duration_value(pool, start, now).await?;
    let cost_series = cost_series_tokens(pool, start, now, range).await?;

    Ok(DashboardStats {
        generated_at: now,
        selected_range: range.key().to_owned(),
        available_ranges: DashboardRange::options(),
        kpis: archivist_core::DashboardKpis {
            completion_rate,
            open_backlog: counts.total_documents - counts.complete,
            failure_rate,
            review_load: counts.waiting_review,
            running_jobs,
            throughput: activity.jobs_succeeded,
            cost_in_range_usd: None,
            mttc_seconds,
            p95_stage_duration_ms,
        },
        comparison,
        stage_status: stage_status(pool).await?,
        throughput_series: throughput_series(pool, start, now, range).await?,
        backlog_series: backlog_series(pool, start, now, range, counts).await?,
        job_status,
        run_status: status_counts(pool, StatusTable::PipelineRuns).await?,
        review_status: status_counts(pool, StatusTable::ReviewItems).await?,
        provider_usage: provider_usage(pool, start).await?,
        quality: quality_stats(pool, start).await?,
        cost_series,
        cost_breakdown_by_provider: Vec::new(),
    })
}

pub async fn get_dashboard_live_status(
    pool: &DbPool,
    settings: &RuntimeSettings,
) -> Result<DashboardLiveStatus> {
    let now = Utc::now();
    let active_runs = dashboard_live_runs(pool).await?;
    let active_jobs = dashboard_live_jobs(pool).await?;
    let recent_llm_events = dashboard_live_llm_events(pool).await?;
    let recent_failures = dashboard_live_failures(pool).await?;
    let latest_paperless_event = latest_paperless_audit_event(pool).await?;
    let workflow_safety = get_workflow_safety_status(pool, settings).await?;
    let selector_ready = settings.workflow.mode.auto_select_documents()
        && !workflow_safety.paused
        && workflow_safety
            .hourly_remaining
            .is_none_or(|remaining| remaining > 0)
        && workflow_safety
            .daily_remaining
            .is_none_or(|remaining| remaining > 0);
    let needs_attention = needs_attention_items(pool, &workflow_safety, &recent_failures).await?;

    Ok(DashboardLiveStatus {
        generated_at: now,
        workflow_mode: settings.workflow.mode,
        autopilot_enabled: selector_ready,
        workflow_safety: workflow_safety.clone(),
        selector: selector_processing_status(settings, &workflow_safety),
        next_selector_scan_at: selector_ready.then_some(now + chrono::Duration::seconds(60)),
        llm: llm_processing_status(&active_jobs, &recent_llm_events, &recent_failures),
        paperless: paperless_processing_status(
            &active_jobs,
            latest_paperless_event.as_ref(),
            &recent_failures,
        ),
        active_runs,
        active_jobs,
        recent_llm_events,
        recent_failures,
        needs_attention,
    })
}

pub async fn get_workflow_safety_status(
    pool: &DbPool,
    settings: &RuntimeSettings,
) -> Result<WorkflowSafetyStatus> {
    let hourly_used = auto_selector_runs_since(pool, "1 hour").await?;
    let daily_used = auto_selector_runs_since(pool, "1 day").await?;
    // Throughput caps follow the active provider's tuning when set, falling
    // back to the global workflow.* values otherwise. See
    // `RuntimeSettings::effective_tuning`.
    let tuning = settings.effective_tuning();
    Ok(WorkflowSafetyStatus {
        paused: settings.workflow.paused,
        dry_run: settings.workflow.dry_run,
        hourly_document_limit: tuning.hourly_document_limit,
        daily_document_limit: tuning.daily_document_limit,
        hourly_remaining: remaining_budget(tuning.hourly_document_limit, hourly_used),
        daily_remaining: remaining_budget(tuning.daily_document_limit, daily_used),
    })
}

async fn auto_selector_runs_since(pool: &DbPool, interval: &str) -> Result<i64> {
    sqlx::query_scalar(
        r#"
        select count(distinct paperless_document_id)::bigint
          from pipeline_runs
         where trigger_tag = 'auto-selector'
           and created_at >= now() - $1::interval
        "#,
    )
    .bind(interval)
    .fetch_one(pool)
    .await
    .context("count auto-selector runs")
}

fn remaining_budget(limit: Option<i64>, used: i64) -> Option<i64> {
    limit.map(|limit| (limit - used).max(0))
}

pub fn selector_document_budget(safety: &WorkflowSafetyStatus) -> Option<i64> {
    [safety.hourly_remaining, safety.daily_remaining]
        .into_iter()
        .flatten()
        .min()
}

fn selector_processing_status(
    settings: &RuntimeSettings,
    safety: &WorkflowSafetyStatus,
) -> ServiceProcessingStatus {
    if safety.paused {
        return ServiceProcessingStatus {
            state: "paused".to_owned(),
            title: "Auto selector paused".to_owned(),
            description: "Automatic document selection is paused. Manual queues remain available."
                .to_owned(),
            last_event_at: None,
        };
    }
    if !settings.workflow.mode.auto_select_documents() {
        return ServiceProcessingStatus {
            state: "idle".to_owned(),
            title: "Manual mode".to_owned(),
            description:
                "The selector is disabled because the workflow mode requires manual triggers."
                    .to_owned(),
            last_event_at: None,
        };
    }
    if selector_document_budget(safety).is_some_and(|remaining| remaining <= 0) {
        return ServiceProcessingStatus {
            state: "limited".to_owned(),
            title: "Auto selector limit reached".to_owned(),
            description: "Hourly or daily document limits are exhausted for the current window."
                .to_owned(),
            last_event_at: None,
        };
    }
    ServiceProcessingStatus {
        state: if safety.dry_run { "dry_run" } else { "running" }.to_owned(),
        title: if safety.dry_run {
            "Auto selector dry-run".to_owned()
        } else {
            "Auto selector ready".to_owned()
        },
        description: if safety.dry_run {
            "Documents can be selected and evaluated, but validated patches are not auto-applied."
                .to_owned()
        } else {
            "Automatic document selection is enabled and within configured safety limits."
                .to_owned()
        },
        last_event_at: None,
    }
}

async fn dashboard_live_runs(pool: &DbPool) -> Result<Vec<DashboardLiveRun>> {
    let rows = sqlx::query(
        r#"
        select id, paperless_document_id, mode, status, trigger_tag, stages,
               started_at, created_at, updated_at
          from pipeline_runs
         where status in ('queued', 'running', 'waiting_review', 'applying')
         order by updated_at desc
         limit 8
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let mode: String = row.try_get("mode")?;
            let stages: Value = row.try_get("stages")?;
            let id: Uuid = row.try_get("id")?;
            Ok(DashboardLiveRun {
                id,
                trace_id: id,
                paperless_document_id: row.try_get("paperless_document_id")?,
                mode: mode.parse()?,
                status: row.try_get("status")?,
                trigger_tag: row.try_get("trigger_tag")?,
                stages: serde_json::from_value(stages).unwrap_or_default(),
                started_at: row.try_get("started_at")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect()
}

async fn dashboard_live_jobs(pool: &DbPool) -> Result<Vec<DashboardLiveJob>> {
    let rows = sqlx::query(
        r#"
        select id, run_id, paperless_document_id, stage, status, attempts,
               max_attempts, lease_owner, lease_until, updated_at, error_message
          from jobs
         where status in ('queued', 'running', 'waiting_review')
         order by case status when 'running' then 0 when 'queued' then 1 else 2 end,
                  updated_at desc
         limit 16
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: String = row.try_get("stage")?;
            let run_id: Uuid = row.try_get("run_id")?;
            Ok(DashboardLiveJob {
                id: row.try_get("id")?,
                run_id,
                trace_id: run_id,
                paperless_document_id: row.try_get("paperless_document_id")?,
                stage: stage.parse()?,
                status: row.try_get("status")?,
                attempts: row.try_get("attempts")?,
                max_attempts: row.try_get("max_attempts")?,
                lease_owner: row.try_get("lease_owner")?,
                lease_until: row.try_get("lease_until")?,
                updated_at: row.try_get("updated_at")?,
                error_message: row.try_get("error_message")?,
            })
        })
        .collect()
}

async fn dashboard_live_llm_events(pool: &DbPool) -> Result<Vec<DashboardLiveLlmEvent>> {
    let rows = sqlx::query(
        r#"
        select id, run_id, job_id, stage, provider, model, duration_ms, created_at
          from ai_artifacts
         order by created_at desc
         limit 8
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: String = row.try_get("stage")?;
            Ok(DashboardLiveLlmEvent {
                id: row.try_get("id")?,
                run_id: row.try_get("run_id")?,
                job_id: row.try_get("job_id")?,
                stage: stage.parse()?,
                provider: row.try_get("provider")?,
                model: row.try_get("model")?,
                duration_ms: row.try_get("duration_ms")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

async fn dashboard_live_failures(pool: &DbPool) -> Result<Vec<DashboardLiveFailure>> {
    let rows = sqlx::query(
        r#"
        select id, run_id, paperless_document_id, stage, status, attempts,
               case
                 when status = 'queued' and run_after > now() then 'retry_scheduled'
                 when status = 'queued' then 'retry_ready'
                 else 'failed'
               end as failure_kind,
               coalesce(error_message, 'Job failed without details') as error_message,
               case when status = 'queued' then run_after else null end as next_attempt_at,
               updated_at
          from jobs
         where status = 'failed' or (status = 'queued' and error_message is not null)
         order by updated_at desc
         limit 8
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: String = row.try_get("stage")?;
            Ok(DashboardLiveFailure {
                id: row.try_get("id")?,
                run_id: row.try_get("run_id")?,
                paperless_document_id: row.try_get("paperless_document_id")?,
                stage: stage.parse()?,
                status: row.try_get("status")?,
                failure_kind: row.try_get("failure_kind")?,
                attempts: row.try_get("attempts")?,
                error_message: row.try_get("error_message")?,
                next_attempt_at: row.try_get("next_attempt_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
struct PaperlessAuditEvent {
    event_type: String,
    outcome: String,
    created_at: DateTime<Utc>,
    error_message: Option<String>,
}

async fn latest_paperless_audit_event(pool: &DbPool) -> Result<Option<PaperlessAuditEvent>> {
    let row = sqlx::query(
        r#"
        select event_type, outcome, created_at, error_message
          from audit_events
         where event_type in ('paperless.sync', 'document.patch_applied')
         order by created_at desc
         limit 1
        "#,
    )
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        Ok(PaperlessAuditEvent {
            event_type: row.try_get("event_type")?,
            outcome: row.try_get("outcome")?,
            created_at: row.try_get("created_at")?,
            error_message: row.try_get("error_message")?,
        })
    })
    .transpose()
}

fn llm_processing_status(
    active_jobs: &[DashboardLiveJob],
    recent_llm_events: &[DashboardLiveLlmEvent],
    recent_failures: &[DashboardLiveFailure],
) -> ServiceProcessingStatus {
    if let Some(job) = active_jobs.iter().find(|job| job.status == "running") {
        return ServiceProcessingStatus {
            state: "running".to_owned(),
            title: "LLM processing active".to_owned(),
            description: format!(
                "{} job for Paperless document {} is running.",
                job.stage, job.paperless_document_id
            ),
            last_event_at: Some(job.updated_at),
        };
    }

    if let Some(failure) = latest_hard_failure(recent_failures) {
        return ServiceProcessingStatus {
            state: "error".to_owned(),
            title: "Recent processing failure".to_owned(),
            description: failure.error_message.clone(),
            last_event_at: Some(failure.updated_at),
        };
    }

    if let Some(event) = recent_llm_events.first() {
        return ServiceProcessingStatus {
            state: "idle".to_owned(),
            title: "LLM idle".to_owned(),
            description: format!(
                "Last model call: {} / {} for {}.",
                event.provider, event.model, event.stage
            ),
            last_event_at: Some(event.created_at),
        };
    }

    ServiceProcessingStatus {
        state: "idle".to_owned(),
        title: "LLM idle".to_owned(),
        description: "No model activity recorded yet.".to_owned(),
        last_event_at: None,
    }
}

fn paperless_processing_status(
    active_jobs: &[DashboardLiveJob],
    latest_event: Option<&PaperlessAuditEvent>,
    recent_failures: &[DashboardLiveFailure],
) -> ServiceProcessingStatus {
    if let Some(job) = active_jobs.iter().find(|job| job.status == "running") {
        return ServiceProcessingStatus {
            state: "running".to_owned(),
            title: "Paperless processing active".to_owned(),
            description: format!(
                "Document {} is being read or updated for {}.",
                job.paperless_document_id, job.stage
            ),
            last_event_at: Some(job.updated_at),
        };
    }

    if let Some(event) = latest_event
        && event.outcome != "success"
    {
        return ServiceProcessingStatus {
            state: "error".to_owned(),
            title: "Recent Paperless action failed".to_owned(),
            description: event
                .error_message
                .clone()
                .unwrap_or_else(|| format!("{} ended with {}", event.event_type, event.outcome)),
            last_event_at: Some(event.created_at),
        };
    }

    if let Some(failure) = latest_hard_failure(recent_failures) {
        return ServiceProcessingStatus {
            state: "error".to_owned(),
            title: "Recent document processing failure".to_owned(),
            description: failure.error_message.clone(),
            last_event_at: Some(failure.updated_at),
        };
    }

    if let Some(event) = latest_event {
        return ServiceProcessingStatus {
            state: "idle".to_owned(),
            title: "Paperless idle".to_owned(),
            description: format!("Last Paperless action: {}.", event.event_type),
            last_event_at: Some(event.created_at),
        };
    }

    ServiceProcessingStatus {
        state: "idle".to_owned(),
        title: "Paperless idle".to_owned(),
        description: "No Paperless sync or patch activity recorded yet.".to_owned(),
        last_event_at: None,
    }
}

fn latest_hard_failure(recent_failures: &[DashboardLiveFailure]) -> Option<&DashboardLiveFailure> {
    recent_failures
        .iter()
        .find(|failure| failure.status == "failed" || failure.failure_kind == "failed")
}

pub async fn provider_usage(
    pool: &DbPool,
    start: DateTime<Utc>,
) -> Result<Vec<ProviderUsageStats>> {
    let rows = sqlx::query(
        r#"
        with artifacts as (
          select provider, model, stage, job_id, duration_ms, response
            from ai_artifacts
           where created_at >= $1
        ),
        -- Aggregate artifacts WITHOUT joining feedback, so request/token/
        -- latency stats are not multiplied by the number of feedback events
        -- per job (the fan-out bug). #260.
        usage as (
          select provider,
                 model,
                 stage,
                 count(*)::bigint as request_count,
                 coalesce(avg(duration_ms), 0)::double precision as avg_duration_ms,
                 coalesce(percentile_cont(0.95) within group (order by duration_ms), 0)::bigint as p95_duration_ms,
                 coalesce(sum(
                   case when response #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,prompt_tokens}')::bigint else 0 end +
                   case when response #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,input_tokens}')::bigint else 0 end +
                   -- Ollama reports tokens top-level, not under `usage`. Guard every
                   -- cast: releases up to v1.11.2 redacted numeric usage values to the
                   -- string "[REDACTED]", and prompt_eval_count may be an object.
                   case when response ->> 'prompt_eval_count' ~ '^[0-9]+$'
                        then (response ->> 'prompt_eval_count')::bigint else 0 end +
                   page_usage.input_tokens
                 ), 0)::bigint as input_tokens,
                 coalesce(sum(
                   case when response #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,completion_tokens}')::bigint else 0 end +
                   case when response #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,output_tokens}')::bigint else 0 end +
                   case when response ->> 'eval_count' ~ '^[0-9]+$'
                        then (response ->> 'eval_count')::bigint else 0 end +
                   page_usage.output_tokens
                 ), 0)::bigint as output_tokens
            from artifacts
            -- Pre-v1.12 OCR artifacts keep the per-page provider responses
            -- under `pages` without the flattened top-level `usage` block, so
            -- the top-level reads above count them as 0 tokens. Sum the nested
            -- per-page counters instead; artifacts that do carry a top-level
            -- `usage` (everything written since #259) are skipped so they are
            -- not double counted. #300
            cross join lateral (
              select
                coalesce(sum(
                  case when page.value #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,prompt_tokens}')::bigint else 0 end +
                  case when page.value #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,input_tokens}')::bigint else 0 end +
                  case when page.value ->> 'prompt_eval_count' ~ '^[0-9]+$' then (page.value ->> 'prompt_eval_count')::bigint else 0 end
                ), 0)::bigint as input_tokens,
                coalesce(sum(
                  case when page.value #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,completion_tokens}')::bigint else 0 end +
                  case when page.value #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,output_tokens}')::bigint else 0 end +
                  case when page.value ->> 'eval_count' ~ '^[0-9]+$' then (page.value ->> 'eval_count')::bigint else 0 end
                ), 0)::bigint as output_tokens
              from jsonb_array_elements(
                case when jsonb_typeof(artifacts.response -> 'pages') = 'array'
                          and artifacts.response -> 'usage' is null
                     then artifacts.response -> 'pages'
                     else '[]'::jsonb end
              ) as page(value)
            ) page_usage
           group by provider, model, stage
        ),
        -- Feedback aggregated separately, keyed by the distinct artifact
        -- (provider, model, stage, job_id) tuples so each feedback event is
        -- counted once per cell. Bounded to the same range as the artifacts.
        feedback as (
          select a.provider, a.model, a.stage,
                 count(distinct f.id)::bigint as feedback_count,
                 count(distinct f.id) filter (
                   where f.event_type in ('review.approved', 'review.edited')
                 )::bigint as positive_feedback,
                 count(distinct f.id) filter (
                   where f.event_type = 'review.rejected'
                 )::bigint as negative_feedback
            from (select distinct provider, model, stage, job_id from artifacts) a
            join audit_events f
              on f.job_id = a.job_id
             and f.event_type in ('review.approved', 'review.edited', 'review.rejected')
             and f.created_at >= $1
           group by a.provider, a.model, a.stage
        )
        select u.provider,
               u.model,
               u.stage,
               u.request_count,
               u.avg_duration_ms,
               u.p95_duration_ms,
               u.input_tokens,
               u.output_tokens,
               coalesce(fb.feedback_count, 0)::bigint as feedback_count,
               coalesce(fb.positive_feedback, 0)::bigint as positive_feedback,
               coalesce(fb.negative_feedback, 0)::bigint as negative_feedback
          from usage u
          left join feedback fb
            on fb.provider = u.provider and fb.model = u.model and fb.stage = u.stage
         order by u.request_count desc, u.provider, u.model, u.stage
         limit 50
        "#,
    )
    .bind(start)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(ProviderUsageStats {
                provider: row.try_get("provider")?,
                model: row.try_get("model")?,
                stage: row.try_get("stage")?,
                request_count: row.try_get("request_count")?,
                avg_duration_ms: row.try_get("avg_duration_ms")?,
                p95_duration_ms: row.try_get("p95_duration_ms")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                estimated_cost_usd: None,
                feedback_count: row.try_get("feedback_count")?,
                positive_feedback: row.try_get("positive_feedback")?,
                negative_feedback: row.try_get("negative_feedback")?,
                acceptance_rate: feedback_rate(
                    row.try_get("positive_feedback")?,
                    row.try_get("negative_feedback")?,
                ),
                latency_history: Vec::new(),
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ProviderBucketEntry {
    pub bucket: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub stage: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub avg_duration_ms: Option<f64>,
    pub request_count: i64,
}

pub async fn provider_bucket_entries(
    pool: &DbPool,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
) -> Result<Vec<ProviderBucketEntry>> {
    let granularity = range.granularity();
    let rows = sqlx::query(
        r#"
        select
          date_trunc($4, ai.created_at) as bucket,
          ai.provider,
          ai.model,
          ai.stage,
          coalesce(sum(
            case when response #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,prompt_tokens}')::bigint else 0 end +
            case when response #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,input_tokens}')::bigint else 0 end +
            -- Ollama reports tokens at the top level (prompt_eval_count /
            -- eval_count), not under `usage`. Guard the cast: prompt_eval_count
            -- may be redacted to a JSON object, so only sum plain integers.
            case when response ->> 'prompt_eval_count' ~ '^[0-9]+$'
                 then (response ->> 'prompt_eval_count')::bigint else 0 end +
            page_usage.input_tokens
          ), 0)::bigint as input_tokens,
          coalesce(sum(
            case when response #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,completion_tokens}')::bigint else 0 end +
            case when response #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,output_tokens}')::bigint else 0 end +
            case when response ->> 'eval_count' ~ '^[0-9]+$'
                 then (response ->> 'eval_count')::bigint else 0 end +
            page_usage.output_tokens
          ), 0)::bigint as output_tokens,
          avg(duration_ms)::double precision as avg_duration_ms,
          count(*)::bigint as request_count
        from ai_artifacts ai
        -- Pre-v1.12 OCR artifacts only carry per-page token counters under
        -- `pages`; sum those when no flattened top-level `usage` exists (which
        -- everything written since #259 has — those rows must not be double
        -- counted). #300
        cross join lateral (
          select
            coalesce(sum(
              case when page.value #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,prompt_tokens}')::bigint else 0 end +
              case when page.value #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,input_tokens}')::bigint else 0 end +
              case when page.value ->> 'prompt_eval_count' ~ '^[0-9]+$' then (page.value ->> 'prompt_eval_count')::bigint else 0 end
            ), 0)::bigint as input_tokens,
            coalesce(sum(
              case when page.value #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,completion_tokens}')::bigint else 0 end +
              case when page.value #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,output_tokens}')::bigint else 0 end +
              case when page.value ->> 'eval_count' ~ '^[0-9]+$' then (page.value ->> 'eval_count')::bigint else 0 end
            ), 0)::bigint as output_tokens
          from jsonb_array_elements(
            case when jsonb_typeof(ai.response -> 'pages') = 'array'
                      and ai.response -> 'usage' is null
                 then ai.response -> 'pages'
                 else '[]'::jsonb end
          ) as page(value)
        ) page_usage
        where ai.created_at >= $1
          and ai.created_at < $2
        group by 1, 2, 3, 4
        order by 1, 2, 3, 4
        "#,
    )
    .bind(start)
    .bind(now)
    .bind(granularity.interval())
    .bind(granularity.date_trunc())
    .fetch_all(pool)
    .await
    .context("query provider bucket entries")?;

    rows.into_iter()
        .map(|row| {
            Ok(ProviderBucketEntry {
                bucket: row.try_get("bucket")?,
                provider: row.try_get("provider")?,
                model: row.try_get("model")?,
                stage: row.try_get("stage")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                avg_duration_ms: row.try_get("avg_duration_ms")?,
                request_count: row.try_get("request_count")?,
            })
        })
        .collect()
}

// --- Statistics page aggregations -------------------------------------------
// Fine-grained, custom-range aggregations powering the dedicated Statistics
// page. The API handler assembles summary / by-provider / by-model / by-stage /
// time-series views from these rows in Rust (one DB round-trip each).

/// One (bucket × provider × model × stage) AI-usage cell.
///
/// No p95 here on purpose: percentiles cannot be re-aggregated across the
/// per-cell grain this query returns, so a per-cell p95 was computed but
/// never surfaced. Dropped rather than exposing a misleading number (#312);
/// the dashboard's `provider_usage` computes its p95 over the raw rows.
#[derive(Debug, Clone)]
pub struct StatisticsUsageRow {
    pub bucket: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub stage: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub avg_duration_ms: Option<f64>,
}

/// AI-usage cells over a custom `[from, to)` range, bucketed by `trunc`
/// (a validated date_trunc unit: hour/day/week/month). Token extraction mirrors
/// `provider_bucket_entries` — OpenAI `usage.*` plus Ollama top-level
/// `prompt_eval_count`/`eval_count`, with the redaction guard, plus the
/// per-page `pages[]` fallback for pre-v1.12 OCR artifacts (#300).
pub async fn statistics_usage_rows(
    pool: &DbPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    trunc: &str,
) -> Result<Vec<StatisticsUsageRow>> {
    let rows = sqlx::query(
        r#"
        select
          date_trunc($3, ai.created_at) as bucket,
          ai.provider,
          ai.model,
          ai.stage,
          count(*)::bigint as request_count,
          coalesce(sum(
            case when response #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,prompt_tokens}')::bigint else 0 end +
            case when response #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,input_tokens}')::bigint else 0 end +
            case when response ->> 'prompt_eval_count' ~ '^[0-9]+$'
                 then (response ->> 'prompt_eval_count')::bigint else 0 end +
            page_usage.input_tokens
          ), 0)::bigint as input_tokens,
          coalesce(sum(
            case when response #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,completion_tokens}')::bigint else 0 end +
            case when response #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,output_tokens}')::bigint else 0 end +
            case when response ->> 'eval_count' ~ '^[0-9]+$'
                 then (response ->> 'eval_count')::bigint else 0 end +
            page_usage.output_tokens
          ), 0)::bigint as output_tokens,
          avg(duration_ms)::double precision as avg_duration_ms
        from ai_artifacts ai
        -- Pre-v1.12 OCR artifacts only carry per-page token counters under
        -- `pages`; sum those when no flattened top-level `usage` exists (which
        -- everything written since #259 has — those rows must not be double
        -- counted). #300
        cross join lateral (
          select
            coalesce(sum(
              case when page.value #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,prompt_tokens}')::bigint else 0 end +
              case when page.value #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,input_tokens}')::bigint else 0 end +
              case when page.value ->> 'prompt_eval_count' ~ '^[0-9]+$' then (page.value ->> 'prompt_eval_count')::bigint else 0 end
            ), 0)::bigint as input_tokens,
            coalesce(sum(
              case when page.value #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,completion_tokens}')::bigint else 0 end +
              case when page.value #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (page.value #>> '{usage,output_tokens}')::bigint else 0 end +
              case when page.value ->> 'eval_count' ~ '^[0-9]+$' then (page.value ->> 'eval_count')::bigint else 0 end
            ), 0)::bigint as output_tokens
          from jsonb_array_elements(
            case when jsonb_typeof(ai.response -> 'pages') = 'array'
                      and ai.response -> 'usage' is null
                 then ai.response -> 'pages'
                 else '[]'::jsonb end
          ) as page(value)
        ) page_usage
        where ai.created_at >= $1
          and ai.created_at < $2
        group by 1, 2, 3, 4
        order by 1, 2, 3, 4
        "#,
    )
    .bind(from)
    .bind(to)
    .bind(trunc)
    .fetch_all(pool)
    .await
    .context("query statistics usage rows")?;

    rows.into_iter()
        .map(|row| {
            Ok(StatisticsUsageRow {
                bucket: row.try_get("bucket")?,
                provider: row.try_get("provider")?,
                model: row.try_get("model")?,
                stage: row.try_get("stage")?,
                request_count: row.try_get("request_count")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                avg_duration_ms: row.try_get("avg_duration_ms")?,
            })
        })
        .collect()
}

/// One (bucket × stage × status) pipeline-throughput cell from `jobs`.
#[derive(Debug, Clone)]
pub struct StatisticsThroughputRow {
    pub bucket: DateTime<Utc>,
    pub stage: String,
    pub status: String,
    pub job_count: i64,
}

/// Pipeline throughput over a custom `[from, to)` range, bucketed by `trunc`.
/// Buckets on `updated_at` (when the job reached its terminal status).
pub async fn statistics_throughput_rows(
    pool: &DbPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    trunc: &str,
) -> Result<Vec<StatisticsThroughputRow>> {
    let rows = sqlx::query(
        r#"
        select
          date_trunc($3, updated_at) as bucket,
          stage,
          status,
          count(*)::bigint as job_count
        from jobs
        where updated_at >= $1
          and updated_at < $2
          and status in ('succeeded', 'failed', 'cancelled')
        group by 1, 2, 3
        order by 1, 2, 3
        "#,
    )
    .bind(from)
    .bind(to)
    .bind(trunc)
    .fetch_all(pool)
    .await
    .context("query statistics throughput rows")?;

    rows.into_iter()
        .map(|row| {
            Ok(StatisticsThroughputRow {
                bucket: row.try_get("bucket")?,
                stage: row.try_get("stage")?,
                status: row.try_get("status")?,
                job_count: row.try_get("job_count")?,
            })
        })
        .collect()
}

pub fn dashboard_bucket_labels(
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
) -> Vec<(DateTime<Utc>, String)> {
    use chrono::TimeZone;
    let granularity = range.granularity();
    let start_trunc = truncate_to_granularity(start, granularity);
    let mut buckets = Vec::new();
    let mut cursor = start_trunc;
    while cursor < now {
        buckets.push((cursor, bucket_label(cursor, granularity)));
        cursor = match granularity {
            archivist_core::DashboardGranularity::Hour => cursor + ChronoDuration::hours(1),
            archivist_core::DashboardGranularity::Day => cursor + ChronoDuration::days(1),
            archivist_core::DashboardGranularity::Month => {
                let next_month = if cursor.month() == 12 {
                    Utc.with_ymd_and_hms(cursor.year() + 1, 1, 1, 0, 0, 0)
                        .single()
                } else {
                    Utc.with_ymd_and_hms(cursor.year(), cursor.month() + 1, 1, 0, 0, 0)
                        .single()
                };
                match next_month {
                    Some(value) => value,
                    None => break,
                }
            }
        };
    }
    buckets
}

fn truncate_to_granularity(
    timestamp: DateTime<Utc>,
    granularity: archivist_core::DashboardGranularity,
) -> DateTime<Utc> {
    use chrono::TimeZone;
    match granularity {
        archivist_core::DashboardGranularity::Hour => Utc
            .with_ymd_and_hms(
                timestamp.year(),
                timestamp.month(),
                timestamp.day(),
                timestamp.hour(),
                0,
                0,
            )
            .single()
            .unwrap_or(timestamp),
        archivist_core::DashboardGranularity::Day => Utc
            .with_ymd_and_hms(
                timestamp.year(),
                timestamp.month(),
                timestamp.day(),
                0,
                0,
                0,
            )
            .single()
            .unwrap_or(timestamp),
        archivist_core::DashboardGranularity::Month => Utc
            .with_ymd_and_hms(timestamp.year(), timestamp.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(timestamp),
    }
}

fn feedback_rate(positive: i64, negative: i64) -> Option<f64> {
    let total = positive + negative;
    (total > 0).then_some(positive as f64 / total as f64)
}

async fn quality_stats(pool: &DbPool, start: DateTime<Utc>) -> Result<QualityStats> {
    let row = sqlx::query(
        r#"
        select
          count(*) filter (where event_type in ('review.approved', 'review.edited', 'review.rejected'))::bigint as review_decisions,
          count(*) filter (where event_type = 'review.approved')::bigint as review_approved,
          count(*) filter (where event_type = 'review.edited')::bigint as review_edited,
          count(*) filter (where event_type = 'review.rejected')::bigint as review_rejected
          from audit_events
         where created_at >= $1
        "#,
    )
    .bind(start)
    .fetch_one(pool)
    .await?;
    let review_approved: i64 = row.try_get("review_approved")?;
    let review_edited: i64 = row.try_get("review_edited")?;
    let review_rejected: i64 = row.try_get("review_rejected")?;
    let warning_row = sqlx::query(
        r#"
        select
          count(*) filter (
            where validation_warnings::text ilike '%LowConfidence%'
               or validation_warnings::text ilike '%low confidence%'
               or validation_warnings::text ilike '%below threshold%'
          )::bigint as uncertainty_reviews,
          count(*) filter (
            where validation_warnings is not null
              and validation_warnings <> '[]'::jsonb
          )::bigint as validation_warning_reviews
          from review_items
         where created_at >= $1
        "#,
    )
    .bind(start)
    .fetch_one(pool)
    .await?;
    Ok(QualityStats {
        review_decisions: row.try_get("review_decisions")?,
        review_approved,
        review_edited,
        review_rejected,
        acceptance_rate: feedback_rate(review_approved + review_edited, review_rejected),
        uncertainty_reviews: warning_row.try_get("uncertainty_reviews")?,
        validation_warning_reviews: warning_row.try_get("validation_warning_reviews")?,
    })
}

pub async fn dashboard_range_start(
    pool: &DbPool,
    range: DashboardRange,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    if let Some(duration) = range.duration() {
        return Ok(now - duration);
    }
    let row = sqlx::query(
        r#"
        select least(
          coalesce((select min(created_at) from jobs), now()),
          coalesce((select min(created_at) from pipeline_runs), now()),
          coalesce((select min(captured_at) from dashboard_snapshots), now())
        ) as started_at
        "#,
    )
    .fetch_one(pool)
    .await?;
    row.try_get("started_at").context("dashboard range start")
}

async fn activity_summary(
    pool: &DbPool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<ActivitySummary> {
    let row = sqlx::query(
        r#"
        select
          count(*) filter (where created_at >= $1 and created_at < $2)::bigint as jobs_created,
          count(*) filter (where status = 'succeeded' and updated_at >= $1 and updated_at < $2)::bigint as jobs_succeeded,
          count(*) filter (where status = 'failed' and updated_at >= $1 and updated_at < $2)::bigint as jobs_failed
        from jobs
        "#,
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await?;
    Ok(ActivitySummary {
        jobs_created: row.try_get("jobs_created")?,
        jobs_succeeded: row.try_get("jobs_succeeded")?,
        jobs_failed: row.try_get("jobs_failed")?,
    })
}

async fn dashboard_comparison(
    pool: &DbPool,
    start: DateTime<Utc>,
    counts: &BacklogCounts,
    current: ActivitySummary,
    previous: Option<ActivitySummary>,
) -> Result<DashboardComparison> {
    let previous_open_backlog: Option<i64> = sqlx::query(
        r#"
        select total_documents - complete as open_backlog
          from dashboard_snapshots
         where captured_at < $1
         order by captured_at desc
         limit 1
        "#,
    )
    .bind(start)
    .fetch_optional(pool)
    .await?
    .map(|row| row.try_get::<i64, _>("open_backlog"))
    .transpose()?;
    Ok(compute_dashboard_comparison(
        counts,
        current,
        previous,
        previous_open_backlog,
    ))
}

/// Pure half of `dashboard_comparison` — composes a `DashboardComparison`
/// from the current activity summary, the optional previous-period summary
/// and an optional historical open-backlog snapshot. Extracted so the math
/// can be unit-tested without a pool.
fn compute_dashboard_comparison(
    counts: &BacklogCounts,
    current: ActivitySummary,
    previous: Option<ActivitySummary>,
    previous_open_backlog: Option<i64>,
) -> DashboardComparison {
    let previous_open_backlog =
        previous_open_backlog.unwrap_or(counts.total_documents - counts.complete);
    let previous = previous.unwrap_or(ActivitySummary {
        jobs_created: current.jobs_created,
        jobs_succeeded: current.jobs_succeeded,
        jobs_failed: current.jobs_failed,
    });
    DashboardComparison {
        jobs_created_delta: current.jobs_created - previous.jobs_created,
        jobs_succeeded_delta: current.jobs_succeeded - previous.jobs_succeeded,
        jobs_failed_delta: current.jobs_failed - previous.jobs_failed,
        open_backlog_delta: counts.total_documents - counts.complete - previous_open_backlog,
    }
}

/// Per-stage rollup for the dashboard Stage-Matrix.
///
/// v1.4.0 replaced the six per-field stages (title/document_type/correspondent/document_date/
/// tags/fields) with the consolidated `metadata` stage. To keep the matrix readable on both
/// fresh v1.4 installs and instances still draining v1.3 in-flight runs, we:
///
/// * Always emit `ocr` and `metadata` rows. `metadata_status` is read from the column added
///   in migration 0019. A fresh install with no documents yet still gets both rows with zero
///   counts — the dashboard never disappears.
/// * Emit a legacy per-field row only when at least one document has a non-`unknown` value in
///   that field column. Once the metadata-stage migration backfills the inventory (and new runs
///   stop writing the legacy columns), those rows collapse to zero and the `HAVING` clause
///   suppresses them. This lets v1.3 in-flight runs finish visibly without leaving permanent
///   ghost rows on the dashboard.
async fn stage_status(pool: &DbPool) -> Result<Vec<DashboardStageStatus>> {
    let rows = sqlx::query(
        r#"
        with stage_rows as (
          select 'ocr' as stage, ocr_status as status, current_run_status, true as always_show
            from document_inventory
          union all select 'metadata', metadata_status, current_run_status, true
            from document_inventory
          union all select 'title', title_status, current_run_status, false
            from document_inventory
          union all select 'document_type', document_type_status, current_run_status, false
            from document_inventory
          union all select 'correspondent', correspondent_status, current_run_status, false
            from document_inventory
          union all select 'document_date', document_date_status, current_run_status, false
            from document_inventory
          union all select 'tags', tagging_status, current_run_status, false
            from document_inventory
          union all select 'fields', fields_status, current_run_status, false
            from document_inventory
        ),
        counted as (
          select
            stage,
            bool_or(always_show) as always_show,
            count(*)::bigint as total,
            count(*) filter (where status in ('succeeded', 'skipped', 'not_needed'))::bigint as complete,
            count(*) filter (where status = 'failed')::bigint as failed,
            count(*) filter (where status = 'waiting_review' or current_run_status = 'waiting_review')::bigint as waiting_review,
            count(*) filter (where current_run_status in ('queued', 'running', 'applying') and status not in ('succeeded', 'skipped', 'not_needed', 'failed'))::bigint as running,
            count(*) filter (where status <> 'unknown')::bigint as touched
          from stage_rows
          group by stage
        )
        select stage, complete, failed, waiting_review, running,
               greatest(total - complete - failed - waiting_review - running, 0)::bigint as pending
          from counted
         where always_show
            or touched > 0
         order by case stage
           when 'ocr' then 1
           when 'metadata' then 2
           when 'title' then 3
           when 'document_type' then 4
           when 'correspondent' then 5
           when 'document_date' then 6
           when 'tags' then 7
           else 8
         end
        "#,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(DashboardStageStatus {
                stage: row.try_get("stage")?,
                complete: row.try_get("complete")?,
                pending: row.try_get("pending")?,
                failed: row.try_get("failed")?,
                waiting_review: row.try_get("waiting_review")?,
                running: row.try_get("running")?,
            })
        })
        .collect()
}

async fn throughput_series(
    pool: &DbPool,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
) -> Result<Vec<DashboardTimeBucket>> {
    let granularity = range.granularity();
    let rows = sqlx::query(
        r#"
        with buckets as (
          select generate_series(date_trunc($4, $1), $2, $3::interval) as bucket
        )
        select
          b.bucket,
          (select count(*)::bigint from jobs where created_at >= b.bucket and created_at < b.bucket + $3::interval) as jobs_created,
          (select count(*)::bigint from jobs where status = 'succeeded' and updated_at >= b.bucket and updated_at < b.bucket + $3::interval) as jobs_succeeded,
          (select count(*)::bigint from jobs where status = 'failed' and updated_at >= b.bucket and updated_at < b.bucket + $3::interval) as jobs_failed,
          (select count(*)::bigint from pipeline_runs where created_at >= b.bucket and created_at < b.bucket + $3::interval) as runs_created,
          (select count(*)::bigint from pipeline_runs where status = 'succeeded' and finished_at >= b.bucket and finished_at < b.bucket + $3::interval) as runs_succeeded,
          (select count(*)::bigint from pipeline_runs where status = 'failed' and finished_at >= b.bucket and finished_at < b.bucket + $3::interval) as runs_failed
        from buckets b
        order by b.bucket
        "#,
    )
    .bind(start)
    .bind(now)
    .bind(granularity.interval())
    .bind(granularity.date_trunc())
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let bucket: DateTime<Utc> = row.try_get("bucket")?;
            Ok(DashboardTimeBucket {
                label: bucket_label(bucket, granularity),
                bucket,
                jobs_created: row.try_get("jobs_created")?,
                jobs_succeeded: row.try_get("jobs_succeeded")?,
                jobs_failed: row.try_get("jobs_failed")?,
                runs_created: row.try_get("runs_created")?,
                runs_succeeded: row.try_get("runs_succeeded")?,
                runs_failed: row.try_get("runs_failed")?,
            })
        })
        .collect()
}

async fn backlog_series(
    pool: &DbPool,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
    counts: &BacklogCounts,
) -> Result<Vec<DashboardBacklogPoint>> {
    let granularity = range.granularity();
    let rows = sqlx::query(
        r#"
        with buckets as (
          select generate_series(date_trunc($4, $1), $2, $3::interval) as bucket
        )
        select b.bucket, s.total_documents, s.complete, s.failed, s.waiting_review, s.running
          from buckets b
          join lateral (
            select total_documents, complete, failed, waiting_review, running
              from dashboard_snapshots
             where captured_at >= b.bucket
               and captured_at < b.bucket + $3::interval
             order by captured_at desc
             limit 1
          ) s on true
         order by b.bucket
        "#,
    )
    .bind(start)
    .bind(now)
    .bind(granularity.interval())
    .bind(granularity.date_trunc())
    .fetch_all(pool)
    .await?;

    let mut points = rows
        .into_iter()
        .map(|row| {
            let bucket: DateTime<Utc> = row.try_get("bucket")?;
            let total_documents: i64 = row.try_get("total_documents")?;
            let complete: i64 = row.try_get("complete")?;
            Ok(DashboardBacklogPoint {
                label: bucket_label(bucket, granularity),
                bucket,
                total_documents,
                complete,
                open_backlog: total_documents - complete,
                failed: row.try_get("failed")?,
                waiting_review: row.try_get("waiting_review")?,
                running: row.try_get("running")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    apply_backlog_series_empty_state_fallback(&mut points, now, granularity, counts);

    Ok(points)
}

/// Pure helper that synthesises a single "now" backlog point from the live
/// `counts` snapshot when no `dashboard_snapshots` rows fall inside the
/// requested range. Extracted so the empty-state behaviour can be unit-tested.
fn apply_backlog_series_empty_state_fallback(
    points: &mut Vec<DashboardBacklogPoint>,
    now: DateTime<Utc>,
    granularity: archivist_core::DashboardGranularity,
    counts: &BacklogCounts,
) {
    if points.is_empty() {
        points.push(DashboardBacklogPoint {
            bucket: now,
            label: bucket_label(now, granularity),
            total_documents: counts.total_documents,
            complete: counts.complete,
            open_backlog: counts.total_documents - counts.complete,
            failed: counts.failed,
            waiting_review: counts.waiting_review,
            running: counts.running,
        });
    }
}

/// Tables that the dashboard groups by `status`. The variants are the only valid
/// inputs to [`status_counts`] — we use a closed Rust enum instead of a free-form
/// string so the table name can never originate from caller-controlled data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTable {
    Jobs,
    PipelineRuns,
    ReviewItems,
}

impl StatusTable {
    /// Static SQL identifier for this table. The returned value is a compile-time
    /// constant — safe to interpolate into queries.
    fn name(self) -> &'static str {
        match self {
            Self::Jobs => "jobs",
            Self::PipelineRuns => "pipeline_runs",
            Self::ReviewItems => "review_items",
        }
    }
}

async fn status_counts(pool: &DbPool, table: StatusTable) -> Result<Vec<DashboardStatusCount>> {
    // SAFETY: `table.name()` is a compile-time constant chosen from a closed enum.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        select status, count(*)::bigint as count
          from {table}
         group by status
         order by count desc, status
        "#,
        table = table.name(),
    )))
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(DashboardStatusCount {
                status: row.try_get("status")?,
                count: row.try_get("count")?,
            })
        })
        .collect()
}

async fn mttc_seconds_value(
    pool: &DbPool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Option<f64>> {
    // Previously this was `avg(finished_at - started_at)` over
    // pipeline_runs, which dwarfed every other dashboard signal: a run can
    // sit in `waiting_review`, in `applying` between user clicks, or pause
    // entirely if the worker is offline, and that wall-clock latency has
    // nothing to do with how long the system spent computing the answer.
    // Real-world deployments saw values like "123 h 53 m" — accurate for
    // wall clock, useless as a processing-time KPI.
    //
    // The honest measurement is the AI compute time per run: sum of
    // `ai_artifacts.duration_ms` across the run, averaged across runs that
    // finished in the window. This is what the user means by "how long
    // does processing a document take" — it ignores human-paced gaps and
    // tracks the actual work.
    let row = sqlx::query(
        r#"
        with per_run as (
          select a.run_id, sum(a.duration_ms)::double precision / 1000.0 as seconds
            from ai_artifacts a
            join pipeline_runs r on r.id = a.run_id
           where r.status = 'succeeded'
             and r.finished_at is not null
             and r.finished_at >= $1
             and r.finished_at < $2
             and a.duration_ms is not null
           group by a.run_id
        )
        select avg(seconds)::double precision as mttc from per_run
        "#,
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
    .context("query mttc")?;
    Ok(row.try_get::<Option<f64>, _>("mttc")?)
}

async fn p95_stage_duration_value(
    pool: &DbPool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Option<i64>> {
    let row = sqlx::query(
        r#"
        select percentile_cont(0.95) within group (order by duration_ms)::bigint as p95
          from ai_artifacts
         where duration_ms is not null
           and created_at >= $1
           and created_at < $2
        "#,
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
    .context("query p95 stage duration")?;
    Ok(row.try_get::<Option<i64>, _>("p95")?)
}

async fn cost_series_tokens(
    pool: &DbPool,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    range: DashboardRange,
) -> Result<Vec<DashboardCostBucket>> {
    let granularity = range.granularity();
    let rows = sqlx::query(
        r#"
        with buckets as (
          select generate_series(date_trunc($4, $1), $2, $3::interval) as bucket
        )
        select
          b.bucket,
          coalesce((
            select sum(
              case when response #>> '{usage,prompt_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,prompt_tokens}')::bigint else 0 end +
              case when response #>> '{usage,input_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,input_tokens}')::bigint else 0 end +
              -- Ollama reports tokens top-level, not under `usage`. Without
              -- this the dashboard cost series showed 0 tokens on Ollama-only
              -- installs while the Statistics page showed real numbers. #261.
              case when response ->> 'prompt_eval_count' ~ '^[0-9]+$' then (response ->> 'prompt_eval_count')::bigint else 0 end
            )::bigint
              from ai_artifacts
             where created_at >= b.bucket and created_at < b.bucket + $3::interval
          ), 0)::bigint as input_tokens,
          coalesce((
            select sum(
              case when response #>> '{usage,completion_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,completion_tokens}')::bigint else 0 end +
              case when response #>> '{usage,output_tokens}' ~ '^[0-9]+$' then (response #>> '{usage,output_tokens}')::bigint else 0 end +
              case when response ->> 'eval_count' ~ '^[0-9]+$' then (response ->> 'eval_count')::bigint else 0 end
            )::bigint
              from ai_artifacts
             where created_at >= b.bucket and created_at < b.bucket + $3::interval
          ), 0)::bigint as output_tokens,
          coalesce((
            select count(*)::bigint
              from ai_artifacts
             where created_at >= b.bucket and created_at < b.bucket + $3::interval
          ), 0)::bigint as request_count
        from buckets b
        order by b.bucket
        "#,
    )
    .bind(start)
    .bind(now)
    .bind(granularity.interval())
    .bind(granularity.date_trunc())
    .fetch_all(pool)
    .await
    .context("query cost series")?;

    rows.into_iter()
        .map(|row| {
            let bucket: DateTime<Utc> = row.try_get("bucket")?;
            Ok(DashboardCostBucket {
                label: bucket_label(bucket, granularity),
                bucket,
                cost_usd: None,
                request_count: row.try_get("request_count")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
            })
        })
        .collect()
}

async fn needs_attention_items(
    pool: &DbPool,
    safety: &WorkflowSafetyStatus,
    recent_failures: &[DashboardLiveFailure],
) -> Result<Vec<NeedsAttentionItem>> {
    // Pull the two pool-dependent counts first; the rest of the composition
    // is pure and lives in `compose_needs_attention_items` so it can be
    // unit-tested without a database. See the tests module.
    let stuck_runs: i64 = sqlx::query_scalar(
        r#"
        select count(*)::bigint
          from pipeline_runs
         where status = 'running'
           and updated_at < now() - interval '10 minutes'
        "#,
    )
    .fetch_one(pool)
    .await
    .context("count stuck runs")?;
    let stale_leases: i64 = sqlx::query_scalar(
        r#"
        select count(*)::bigint
          from jobs
         where status = 'running'
           and lease_until is not null
           and lease_until < now()
        "#,
    )
    .fetch_one(pool)
    .await
    .context("count stale leases")?;
    let blocked = count_blocked_queued_jobs(pool).await?;
    let active_cooldowns = list_active_provider_cooldowns(pool).await?;

    Ok(compose_needs_attention_items(
        stuck_runs,
        stale_leases,
        safety,
        recent_failures,
        &blocked,
        &active_cooldowns,
    ))
}

/// Pure composition of `NeedsAttentionItem`s from a snapshot of the inputs
/// `needs_attention_items` would otherwise gather from the database. Extracted
/// so the ordering and threshold logic can be unit-tested without a pool.
fn compose_needs_attention_items(
    stuck_runs: i64,
    stale_leases: i64,
    safety: &WorkflowSafetyStatus,
    recent_failures: &[DashboardLiveFailure],
    blocked: &BlockedQueuedCounts,
    active_cooldowns: &[AiProviderCooldown],
) -> Vec<NeedsAttentionItem> {
    let mut items = Vec::new();

    if stuck_runs > 0 {
        items.push(NeedsAttentionItem {
            kind: "stuck_runs".to_owned(),
            severity: "critical".to_owned(),
            title: format!("{stuck_runs} stuck run(s)"),
            description: "Pipeline runs have not progressed in the last 10 minutes.".to_owned(),
            action_key: Some("dashboard.alerts.action.recover_runs".to_owned()),
            count: Some(stuck_runs),
        });
    }

    if stale_leases > 0 {
        items.push(NeedsAttentionItem {
            kind: "stale_leases".to_owned(),
            severity: "warning".to_owned(),
            title: format!("{stale_leases} stale lease(s)"),
            description:
                "Workers hold expired leases. Requeue to let healthy workers pick them up."
                    .to_owned(),
            action_key: Some("dashboard.alerts.action.requeue_leases".to_owned()),
            count: Some(stale_leases),
        });
    }

    // Blocked queued jobs: the claim_jobs filter refuses these because
    // an earlier-stage job in the same run is failed or waiting_review.
    // Surface count + offer the operator-side "unblock" action, which
    // re-queues failed predecessors with attempts=0 so subsequent ticks
    // can pick them up again.
    if blocked.total > 0 {
        items.push(NeedsAttentionItem {
            kind: "blocked_jobs".to_owned(),
            severity: if blocked.blocked_by_failed > 0 {
                "critical".to_owned()
            } else {
                "warning".to_owned()
            },
            title: format!("{} blockierte Job(s)", blocked.total),
            description: format!(
                "{} durch fehlgeschlagene Vorgänger-Stages, {} durch laufende Reviews. \
                 Entsperren stellt die Vorgänger zurück in die Queue, ohne den Run zu verwerfen.",
                blocked.blocked_by_failed, blocked.blocked_by_review
            ),
            action_key: Some("dashboard.alerts.action.unblock_jobs".to_owned()),
            count: Some(blocked.total),
        });
    }

    // Active provider cooldown: the worker hit a usage-cap 429 and
    // suspended the provider. While the cooldown holds, claims for jobs
    // routed to that provider release the lease without burning a retry
    // — so this is mostly informational, unless the operator just
    // upgraded the plan and wants to lift the cooldown manually.
    if !active_cooldowns.is_empty() {
        let provider_list = active_cooldowns
            .iter()
            .map(|c| c.provider_name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        items.push(NeedsAttentionItem {
            kind: "provider_cooldown".to_owned(),
            severity: "critical".to_owned(),
            title: format!("{} provider quota cooldown", active_cooldowns.len()),
            description: format!(
                "AI providers paused due to usage-cap 429: {provider_list}. \
                 Worker will skip jobs routed to these providers until cooldown expires."
            ),
            action_key: Some("dashboard.alerts.action.clear_provider_cooldown".to_owned()),
            count: Some(active_cooldowns.len() as i64),
        });
    }

    if quota_below_threshold(safety.hourly_remaining, safety.hourly_document_limit) {
        items.push(NeedsAttentionItem {
            kind: "quota_low".to_owned(),
            severity: "warning".to_owned(),
            title: "Hourly quota almost exhausted".to_owned(),
            description: "Automatic selection will pause when the hourly limit is reached."
                .to_owned(),
            action_key: Some("dashboard.alerts.action.adjust_limits".to_owned()),
            count: safety.hourly_remaining,
        });
    }
    if quota_below_threshold(safety.daily_remaining, safety.daily_document_limit) {
        items.push(NeedsAttentionItem {
            kind: "quota_low".to_owned(),
            severity: "warning".to_owned(),
            title: "Daily quota almost exhausted".to_owned(),
            description: "Automatic selection will pause when the daily limit is reached."
                .to_owned(),
            action_key: Some("dashboard.alerts.action.adjust_limits".to_owned()),
            count: safety.daily_remaining,
        });
    }

    let hard_failure_count = recent_failures
        .iter()
        .filter(|item| item.failure_kind == "failed")
        .count() as i64;
    if hard_failure_count >= 3 {
        items.push(NeedsAttentionItem {
            kind: "provider_error".to_owned(),
            severity: "warning".to_owned(),
            title: format!("{hard_failure_count} recent failure(s)"),
            description: "Multiple jobs failed recently. Inspect logs or provider availability."
                .to_owned(),
            action_key: Some("dashboard.alerts.action.inspect_failures".to_owned()),
            count: Some(hard_failure_count),
        });
    }

    if safety.dry_run {
        items.push(NeedsAttentionItem {
            kind: "dry_run_active".to_owned(),
            severity: "info".to_owned(),
            title: "Dry-run mode is active".to_owned(),
            description:
                "Validated patches will not be applied to Paperless until dry-run is disabled."
                    .to_owned(),
            action_key: Some("dashboard.alerts.action.disable_dry_run".to_owned()),
            count: None,
        });
    }

    items.sort_by_key(|item| match item.severity.as_str() {
        "critical" => 0,
        "warning" => 1,
        "info" => 2,
        _ => 3,
    });

    items
}

fn quota_below_threshold(remaining: Option<i64>, limit: Option<i64>) -> bool {
    match (remaining, limit) {
        (Some(remaining), Some(limit)) if limit > 0 => {
            let threshold = (limit as f64 * 0.1).ceil() as i64;
            remaining <= threshold.max(1)
        }
        _ => false,
    }
}

fn bucket_label(
    bucket: DateTime<Utc>,
    granularity: archivist_core::DashboardGranularity,
) -> String {
    match granularity {
        archivist_core::DashboardGranularity::Hour => bucket.format("%H:%M").to_string(),
        archivist_core::DashboardGranularity::Day => bucket.format("%d.%m.").to_string(),
        archivist_core::DashboardGranularity::Month => bucket.format("%Y-%m").to_string(),
    }
}

/// Filter shape for `list_inventory` / `count_inventory`. All fields are
/// optional; the empty default matches every row. Built up by the
/// `/api/inventory` handler from query-string parameters and passed through
/// to the SQL layer, which translates the populated fields into WHERE
/// clauses dynamically.
#[derive(Debug, Clone, Default)]
pub struct InventoryQuery {
    pub id: Option<i32>,
    /// ILIKE match on `title` OR `original_file_name`. The handler wraps
    /// the user-supplied string with `%…%` before binding, so callers
    /// pass plain text.
    pub q: Option<String>,
    pub ocr_status: Vec<String>,
    pub metadata_status: Vec<String>,
    pub run_status: Vec<String>,
    /// All listed tag names must be present on `current_tags` (AND).
    pub tags_include: Vec<String>,
    /// None of the listed tag names may be present on `current_tags`.
    pub tags_exclude: Vec<String>,
    pub language: Option<String>,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub has_error: Option<bool>,
    pub needs_review: Option<bool>,
}

impl InventoryQuery {
    fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.q.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.ocr_status.is_empty()
            && self.metadata_status.is_empty()
            && self.run_status.is_empty()
            && self.tags_include.is_empty()
            && self.tags_exclude.is_empty()
            && self.language.is_none()
            && self.date_from.is_none()
            && self.date_to.is_none()
            && self.has_error.is_none()
            && self.needs_review.is_none()
    }
}

/// Push the `WHERE` predicates derived from an [`InventoryQuery`] onto a
/// `QueryBuilder`. The caller is expected to have appended the table
/// reference (and any leading clauses) before calling. Each pushed
/// predicate is prefixed with ` AND ` so the caller's clause acts as the
/// implicit `1=1`. Returns early without writing anything when the query
/// is empty.
fn push_inventory_filters(builder: &mut QueryBuilder<Postgres>, query: &InventoryQuery) {
    if let Some(id) = query.id {
        builder.push(" and paperless_document_id = ").push_bind(id);
    }
    if let Some(text) = query.q.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let like = format!("%{}%", text);
        builder
            .push(" and (title ilike ")
            .push_bind(like.clone())
            .push(" or original_file_name ilike ")
            .push_bind(like)
            .push(")");
    }
    if !query.ocr_status.is_empty() {
        builder
            .push(" and ocr_status = any(")
            .push_bind(&query.ocr_status)
            .push(")");
    }
    if !query.metadata_status.is_empty() {
        builder
            .push(" and metadata_status = any(")
            .push_bind(&query.metadata_status)
            .push(")");
    }
    if !query.run_status.is_empty() {
        builder
            .push(" and coalesce(current_run_status, '') = any(")
            .push_bind(&query.run_status)
            .push(")");
    }
    if !query.tags_include.is_empty() {
        builder
            .push(" and current_tags @> ")
            .push_bind(&query.tags_include);
    }
    if !query.tags_exclude.is_empty() {
        builder
            .push(" and not (current_tags && ")
            .push_bind(&query.tags_exclude)
            .push(")");
    }
    if let Some(lang) = query.language.as_ref().filter(|s| !s.is_empty()) {
        builder
            .push(" and detected_language = ")
            .push_bind(lang.clone());
    }
    if let Some(from) = query.date_from.as_ref().filter(|s| !s.is_empty()) {
        builder
            .push(" and document_date >= ")
            .push_bind(from.clone());
    }
    if let Some(to) = query.date_to.as_ref().filter(|s| !s.is_empty()) {
        builder.push(" and document_date <= ").push_bind(to.clone());
    }
    if let Some(has_error) = query.has_error {
        if has_error {
            builder.push(" and last_error is not null");
        } else {
            builder.push(" and last_error is null");
        }
    }
    if let Some(needs_review) = query.needs_review {
        builder.push(" and needs_review = ").push_bind(needs_review);
    }
}

pub async fn list_inventory(
    pool: &DbPool,
    query: &InventoryQuery,
    limit: i64,
    offset: i64,
) -> Result<Vec<DocumentInventoryItem>> {
    let mut builder = QueryBuilder::<Postgres>::new(
        "select paperless_document_id, title, original_file_name, current_tags, ocr_status, \
         metadata_status, tagging_status, title_status, correspondent_status, \
         document_type_status, document_date_status, fields_status, current_run_status, \
         last_run_id, last_error, next_required_stage, needs_review, complete, \
         document_date, detected_language, detected_language_confidence, \
         detected_language_source, last_seen_at \
         from document_inventory where 1=1",
    );
    push_inventory_filters(&mut builder, query);
    builder
        .push(" order by paperless_document_id desc limit ")
        .push_bind(limit)
        .push(" offset ")
        .push_bind(offset);
    let rows = builder.build().fetch_all(pool).await?;

    rows.into_iter()
        .map(|row| {
            Ok(DocumentInventoryItem {
                paperless_document_id: row.try_get("paperless_document_id")?,
                title: row.try_get("title")?,
                original_file_name: row.try_get("original_file_name")?,
                current_tags: row.try_get("current_tags")?,
                ocr_status: row.try_get("ocr_status")?,
                metadata_status: row.try_get("metadata_status")?,
                tagging_status: row.try_get("tagging_status")?,
                title_status: row.try_get("title_status")?,
                correspondent_status: row.try_get("correspondent_status")?,
                document_type_status: row.try_get("document_type_status")?,
                document_date_status: row.try_get("document_date_status")?,
                fields_status: row.try_get("fields_status")?,
                current_run_status: row.try_get("current_run_status")?,
                last_run_id: row.try_get("last_run_id")?,
                last_error: row.try_get("last_error")?,
                next_required_stage: row.try_get("next_required_stage")?,
                needs_review: row.try_get("needs_review")?,
                complete: row.try_get("complete")?,
                document_date: row.try_get("document_date")?,
                detected_language: row.try_get("detected_language")?,
                detected_language_confidence: row.try_get("detected_language_confidence")?,
                detected_language_source: row.try_get("detected_language_source")?,
                last_seen_at: row.try_get("last_seen_at")?,
            })
        })
        .collect()
}

/// Count inventory rows matching the same filters as `list_inventory`. Used
/// by `/api/inventory` to compute `total` so the frontend's "Showing N of M"
/// counter reflects the FILTERED total, not the unfiltered table size.
pub async fn count_inventory(pool: &DbPool, query: &InventoryQuery) -> Result<i64> {
    if query.is_empty() {
        // Fast path for the unfiltered case — avoids the QueryBuilder
        // overhead and lets Postgres serve the answer from the relation
        // count cache rather than scanning.
        let count: i64 = sqlx::query_scalar("select count(*)::bigint from document_inventory")
            .fetch_one(pool)
            .await?;
        return Ok(count);
    }
    let mut builder =
        QueryBuilder::<Postgres>::new("select count(*)::bigint from document_inventory where 1=1");
    push_inventory_filters(&mut builder, query);
    let count: i64 = builder.build_query_scalar().fetch_one(pool).await?;
    Ok(count)
}

pub async fn create_document_chat_session(
    pool: &DbPool,
    title: &str,
    created_by: Option<Uuid>,
) -> Result<Uuid> {
    let id = sqlx::query(
        r#"
        insert into document_chat_sessions (title, created_by)
        values ($1, $2)
        returning id
        "#,
    )
    .bind(title)
    .bind(created_by)
    .fetch_one(pool)
    .await?
    .try_get("id")?;
    Ok(id)
}

pub async fn document_chat_session_visible(
    pool: &DbPool,
    session_id: Uuid,
    user_id: Option<Uuid>,
    include_all: bool,
) -> Result<bool> {
    let row = sqlx::query(
        r#"
        select exists(
          select 1
            from document_chat_sessions
           where id = $1
             and ($2::boolean or created_by = $3)
        ) as visible
        "#,
    )
    .bind(session_id)
    .bind(include_all)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    row.try_get("visible")
        .context("read chat session visibility")
}

pub async fn list_document_chat_sessions(
    pool: &DbPool,
    user_id: Option<Uuid>,
    include_all: bool,
    limit: i64,
) -> Result<Vec<DocumentChatSessionRecord>> {
    let rows = sqlx::query(
        r#"
        select id, title, created_by, created_at, updated_at
          from document_chat_sessions
         where $1::boolean or created_by = $2
         order by updated_at desc
         limit $3
        "#,
    )
    .bind(include_all)
    .bind(user_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(DocumentChatSessionRecord {
                id: row.try_get("id")?,
                title: row.try_get("title")?,
                created_by: row.try_get("created_by")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect()
}

pub async fn insert_document_chat_message(
    pool: &DbPool,
    session_id: Uuid,
    role: &str,
    content: &str,
    provider: Option<&str>,
    model: Option<&str>,
    metadata: Option<Value>,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        r#"
        insert into document_chat_messages (session_id, role, content, provider, model, metadata)
        values ($1, $2, $3, $4, $5, $6)
        returning id
        "#,
    )
    .bind(session_id)
    .bind(role)
    .bind(content)
    .bind(provider)
    .bind(model)
    .bind(metadata)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;

    sqlx::query("update document_chat_sessions set updated_at = now() where id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(id)
}

pub async fn insert_document_chat_sources(
    pool: &DbPool,
    message_id: Uuid,
    sources: &[DocumentChatSource],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    for source in sources {
        sqlx::query(
            r#"
            insert into document_chat_sources (
              message_id, paperless_document_id, title, snippet, score, source_kind
            )
            values ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(message_id)
        .bind(source.paperless_document_id)
        .bind(&source.title)
        .bind(&source.snippet)
        .bind(source.score)
        .bind(&source.source_kind)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn list_document_chat_messages(
    pool: &DbPool,
    session_id: Uuid,
) -> Result<Vec<DocumentChatMessageRecord>> {
    let rows = sqlx::query(
        r#"
        select id, session_id, role, content, provider, model, metadata, created_at
          from document_chat_messages
         where session_id = $1
         order by created_at
        "#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    let mut messages = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        messages.push(DocumentChatMessageRecord {
            id,
            session_id: row.try_get("session_id")?,
            role: row.try_get("role")?,
            content: row.try_get("content")?,
            provider: row.try_get("provider")?,
            model: row.try_get("model")?,
            metadata: row.try_get("metadata")?,
            sources: list_document_chat_sources(pool, id).await?,
            created_at: row.try_get("created_at")?,
        });
    }
    Ok(messages)
}

pub async fn list_document_chat_sources(
    pool: &DbPool,
    message_id: Uuid,
) -> Result<Vec<DocumentChatSource>> {
    let rows = sqlx::query(
        r#"
        select paperless_document_id, title, snippet, score, source_kind
          from document_chat_sources
         where message_id = $1
         order by score desc, created_at
        "#,
    )
    .bind(message_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(DocumentChatSource {
                paperless_document_id: row.try_get("paperless_document_id")?,
                title: row.try_get("title")?,
                snippet: row.try_get("snippet")?,
                score: row.try_get("score")?,
                source_kind: row.try_get("source_kind")?,
            })
        })
        .collect()
}

pub async fn search_document_chat_candidates(
    pool: &DbPool,
    query: &str,
    document_ids: Option<&[i32]>,
    limit: i64,
) -> Result<Vec<DocumentChatCandidate>> {
    let document_ids = document_ids.map(|ids| ids.to_vec());
    let rows = sqlx::query(
        r#"
        select paperless_document_id, title, original_file_name, current_tags,
               -- Blend the pg_trgm title/file/tags similarity with a
               -- full-text rank over the persisted OCR body (#217). Both
               -- terms live in [0, 1]: pg_trgm similarity is bounded by
               -- construction, and ts_rank with normalization flag 32
               -- (rank/(rank+1)) bounds the body score to [0, 1). Their
               -- sum is clamped back into [0, 1] so the combined score
               -- stays comparable with the metadata-only scores callers
               -- already feed into score_document_chat_source.
               least(
                 1.0,
                 greatest(
                   similarity(coalesce(title, ''), $1),
                   similarity(coalesce(original_file_name, ''), $1),
                   similarity(array_to_string(current_tags, ' '), $1)
                 )
                 + ts_rank(ocr_body_tsv, websearch_to_tsquery('simple', $1), 32)
               )::double precision as metadata_score
          from document_inventory
         where ($2::integer[] is null or paperless_document_id = any($2))
           and (
             $2::integer[] is not null
             or greatest(
               similarity(coalesce(title, ''), $1),
               similarity(coalesce(original_file_name, ''), $1),
               similarity(array_to_string(current_tags, ' '), $1)
             ) > 0
             or ocr_body_tsv @@ websearch_to_tsquery('simple', $1)
           )
         order by metadata_score desc, last_seen_at desc
         limit $3
        "#,
    )
    .bind(query)
    .bind(document_ids)
    .bind(limit.clamp(1, 100))
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(DocumentChatCandidate {
                paperless_document_id: row.try_get("paperless_document_id")?,
                title: row.try_get("title")?,
                original_file_name: row.try_get("original_file_name")?,
                current_tags: row.try_get("current_tags")?,
                metadata_score: row.try_get("metadata_score")?,
            })
        })
        .collect()
}

/// Computes the cross-run job priority for an auto-selected document.
///
/// Newer Paperless document ids win (smaller priority value). Saturating math keeps the
/// result in `[1, 1_000_000]` so even synthetic doc ids beyond a million never collide with
/// the manual-trigger priority of 0.
pub fn age_derived_priority(paperless_document_id: i32) -> i64 {
    1_000_000_i64
        .saturating_sub(paperless_document_id as i64)
        .max(1)
}

pub async fn create_run_with_jobs(
    pool: &DbPool,
    paperless_document_id: i32,
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
) -> Result<Uuid> {
    create_run_with_jobs_with_priority(
        pool,
        paperless_document_id,
        stages,
        mode,
        trigger_tag,
        actor,
        None,
    )
    .await
}

/// Variant of [`create_run_with_jobs`] that lets callers stamp an explicit cross-run priority on
/// every job. `None` falls back to the age-derived priority (newer doc -> claimed first).
///
/// Manual triggers should pass `Some(0)`; auto-selector / delta-sync paths should pass `None`
/// (or [`age_derived_priority`]). Job payload carries TWO priority values:
///
///   * `priority`        — cross-run ordering (smaller wins)
///   * `stage_priority`  — within-run stage ordering (smaller wins)
///
/// Splitting them in v1.4.0 lets the age-derived value live in `priority` without breaking the
/// existing claim_jobs subquery that enforces stage ordering via the second column.
pub async fn create_run_with_jobs_with_priority(
    pool: &DbPool,
    paperless_document_id: i32,
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
    priority: Option<i64>,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let run_id = create_run_with_jobs_on_tx(
        &mut tx,
        paperless_document_id,
        stages,
        mode,
        trigger_tag,
        actor,
        priority,
    )
    .await?;
    tx.commit().await?;
    Ok(run_id)
}

/// Bulk re-run: create one run (with the given stages) per document in a single transaction.
///
/// Used by the `/api/batches/rerun` endpoint so operators can re-trigger a hand-picked set of
/// "succeeded-but-wrong" documents in one shot instead of one trigger at a time. The active-run
/// guard inside [`create_run_with_jobs_on_tx`] still applies per document, so ids that already
/// have an in-flight run are silently reused (no duplicate run). Returns the number of documents
/// processed (the size of the input set; each contributes exactly one run, new or reused).
pub async fn create_runs_for_documents(
    pool: &DbPool,
    document_ids: &[i32],
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
    priority: Option<i64>,
) -> Result<i64> {
    if document_ids.is_empty() {
        return Ok(0);
    }
    // Amortise one transaction across the whole batch instead of a begin+commit per document.
    let mut tx = pool.begin().await?;
    let mut queued: i64 = 0;
    for &document_id in document_ids {
        create_run_with_jobs_on_tx(
            &mut tx,
            document_id,
            stages,
            mode,
            trigger_tag,
            actor,
            priority,
        )
        .await?;
        queued += 1;
    }
    tx.commit().await?;
    Ok(queued)
}

/// Per-document run/job creation against an existing transaction. Callers own the begin/commit so
/// batch backfills can amortise one transaction across many documents instead of paying a full
/// begin+commit per doc. The active-run guard and idempotent inventory upsert are unchanged.
async fn create_run_with_jobs_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    paperless_document_id: i32,
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
    priority: Option<i64>,
) -> Result<Uuid> {
    if stages.is_empty() {
        return Err(anyhow!("cannot create a run without stages"));
    }

    if let Some(row) = sqlx::query(
        r#"
        select id from pipeline_runs
         where paperless_document_id = $1
           and status in ('queued', 'running', 'waiting_review', 'applying')
         order by created_at desc
         limit 1
        "#,
    )
    .bind(paperless_document_id)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(row.try_get("id")?);
    }

    let cross_run_priority =
        priority.unwrap_or_else(|| age_derived_priority(paperless_document_id));

    let stages_json = serde_json::to_value(stages)?;
    let run_id: Uuid = sqlx::query(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
        values ($1, $2, $3, 'queued', $4)
        returning id
        "#,
    )
    .bind(paperless_document_id)
    .bind(mode.to_string())
    .bind(trigger_tag)
    .bind(stages_json)
    .fetch_one(&mut **tx)
    .await?
    .try_get("id")?;

    for (index, stage) in stages.iter().enumerate() {
        sqlx::query(
            r#"
            insert into jobs (run_id, paperless_document_id, stage, status, payload)
            values ($1, $2, $3, 'queued', $4)
            "#,
        )
        .bind(run_id)
        .bind(paperless_document_id)
        .bind(stage.to_string())
        .bind(json!({
            "priority": cross_run_priority,
            "stage_priority": ((index as i32) + 1) * 10,
        }))
        .execute(&mut **tx)
        .await?;
    }

    sqlx::query(
        r#"
        insert into document_inventory (paperless_document_id, current_run_status, last_run_id, updated_at)
        values ($1, 'queued', $2, now())
        on conflict (paperless_document_id)
        do update set current_run_status = 'queued',
                      last_run_id = excluded.last_run_id,
                      updated_at = now()
        "#,
    )
    .bind(paperless_document_id)
    .bind(run_id)
    .execute(&mut **tx)
    .await?;

    append_audit_tx(
        tx,
        AuditEventInput {
            event_type: "run.created".to_owned(),
            actor_type: actor.to_owned(),
            actor_id: None,
            run_id: Some(run_id),
            job_id: None,
            paperless_document_id: Some(paperless_document_id),
            before: None,
            after: Some(json!({ "stages": stages, "mode": mode, "trigger_tag": trigger_tag })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    Ok(run_id)
}

pub async fn queue_missing_stage(
    pool: &DbPool,
    stage: Stage,
    mode: ProcessingMode,
    actor: &str,
    rules: &WorkflowRules,
    max_documents: Option<i64>,
) -> Result<i64> {
    let column = status_column_for_stage(stage)?;
    let include_tags = WorkflowRules::normalized_tags(&rules.include_tags);
    let exclude_tags = WorkflowRules::normalized_tags(&rules.exclude_tags);
    // Eligibility is fully expressible in SQL for this function, so push the budget as `limit $3`
    // and avoid materialising the entire candidate set in Rust.
    let limit_clause = match max_documents {
        Some(_) => "limit $3",
        None => "",
    };
    let query = format!(
        r#"
        select paperless_document_id
          from document_inventory
         where {column} not in ('succeeded', 'skipped', 'not_needed')
           and coalesce(current_run_status, '') not in ('queued', 'running', 'waiting_review', 'applying')
           and ($1::text[] = '{{}}' or current_tags && $1::text[])
           and not (current_tags && $2::text[])
         order by paperless_document_id
         {limit_clause}
        "#
    );
    // SAFETY: `query` is assembled from static fragments plus the validated
    // `limit_clause`; all caller data flows through bind parameters below.
    let mut builder = sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(&include_tags)
        .bind(&exclude_tags);
    if let Some(limit) = max_documents {
        builder = builder.bind(limit);
    }
    let rows = builder.fetch_all(pool).await?;

    // Amortise one transaction across the whole batch instead of a begin+commit per document.
    let mut tx = pool.begin().await?;
    let mut created = 0;
    for row in rows {
        let document_id: i32 = row.try_get("paperless_document_id")?;
        // Age-derived priority — newer documents jump ahead of older ones in claim_jobs.
        // "manual-batch" is the operator-initiated bulk path, but we still rank by age so
        // a fresh scan doesn't get blocked behind a backfill triggered minutes earlier.
        create_run_with_jobs_on_tx(
            &mut tx,
            document_id,
            &[stage],
            mode,
            "manual-batch",
            actor,
            Some(age_derived_priority(document_id)),
        )
        .await?;
        created += 1;
    }
    tx.commit().await?;
    Ok(created)
}

pub async fn queue_missing_pipeline(
    pool: &DbPool,
    enabled_stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
    rules: &WorkflowRules,
    max_documents: Option<i64>,
) -> Result<i64> {
    let include_tags = WorkflowRules::normalized_tags(&rules.include_tags);
    let exclude_tags = WorkflowRules::normalized_tags(&rules.exclude_tags);
    // Eligibility depends on which stages are enabled (Rust-side filter), so we fetch in
    // capped chunks of ~2x budget keyset-paginated by paperless_document_id rather than push
    // a brittle predicate into SQL. When the budget is None, fetch everything in one shot.
    let chunk_size = max_documents.map(|limit| limit.saturating_mul(2).max(16));

    // Amortise one transaction across every chunk + per-doc insert instead of a begin+commit per
    // document. Pagination SELECTs run on the same tx so just-queued rows stay consistently
    // excluded, mirroring the previous per-doc-commit behaviour.
    let mut tx = pool.begin().await?;
    let mut created: i64 = 0;
    let mut last_seen: i32 = i32::MIN;
    loop {
        if max_documents.is_some_and(|limit| created >= limit) {
            break;
        }
        let limit_clause = match chunk_size {
            Some(_) => "limit $4",
            None => "",
        };
        let query = format!(
            r#"
            select paperless_document_id,
                   ocr_status,
                   metadata_status,
                   tagging_status,
                   title_status,
                   correspondent_status,
                   document_type_status,
                   document_date_status,
                   fields_status,
                   has_ocr_completion_tag,
                   has_tagging_completion_tag,
                   has_full_completion_tag
              from document_inventory
             where coalesce(current_run_status, '') not in ('queued', 'running', 'waiting_review', 'applying')
               and ($1::text[] = '{{}}' or current_tags && $1::text[])
               and not (current_tags && $2::text[])
               and paperless_document_id > $3
             order by paperless_document_id
             {limit_clause}
            "#
        );
        // SAFETY: `query` is assembled from static fragments plus the validated
        // `limit_clause`; all caller data flows through bind parameters below.
        let mut builder = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(&include_tags)
            .bind(&exclude_tags)
            .bind(last_seen);
        if let Some(size) = chunk_size {
            builder = builder.bind(size);
        }
        let rows = builder.fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            break;
        }
        let fetched = rows.len();
        for row in rows {
            let document_id: i32 = row.try_get("paperless_document_id")?;
            last_seen = document_id.max(last_seen);
            if max_documents.is_some_and(|limit| created >= limit) {
                break;
            }
            let stages = missing_pipeline_stages_for_inventory(
                enabled_stages,
                InventoryStageState {
                    ocr_status: row.try_get("ocr_status")?,
                    metadata_status: row.try_get("metadata_status")?,
                    tagging_status: row.try_get("tagging_status")?,
                    title_status: row.try_get("title_status")?,
                    correspondent_status: row.try_get("correspondent_status")?,
                    document_type_status: row.try_get("document_type_status")?,
                    document_date_status: row.try_get("document_date_status")?,
                    fields_status: row.try_get("fields_status")?,
                    has_ocr_completion_tag: row.try_get("has_ocr_completion_tag")?,
                    has_tagging_completion_tag: row.try_get("has_tagging_completion_tag")?,
                    has_full_completion_tag: row.try_get("has_full_completion_tag")?,
                },
            );
            if stages.is_empty() {
                continue;
            }

            // Age-derived priority — newer Paperless documents drain through the full
            // pipeline (OCR -> Metadata) before older queued documents.
            create_run_with_jobs_on_tx(
                &mut tx,
                document_id,
                &stages,
                mode,
                trigger_tag,
                actor,
                Some(age_derived_priority(document_id)),
            )
            .await?;
            created += 1;
        }
        // No budget set means we already fetched everything once.
        if chunk_size.is_none() {
            break;
        }
        // No more rows possible than we fetched in this round.
        if chunk_size.is_some_and(|size| fetched < size as usize) {
            break;
        }
    }
    tx.commit().await?;
    Ok(created)
}

struct InventoryStageState {
    ocr_status: String,
    metadata_status: String,
    tagging_status: String,
    title_status: String,
    correspondent_status: String,
    document_type_status: String,
    document_date_status: String,
    fields_status: String,
    has_ocr_completion_tag: bool,
    has_tagging_completion_tag: bool,
    has_full_completion_tag: bool,
}

fn missing_pipeline_stages_for_inventory(
    enabled_stages: &[Stage],
    state: InventoryStageState,
) -> Vec<Stage> {
    if state.has_full_completion_tag {
        return Vec::new();
    }

    enabled_stages
        .iter()
        .copied()
        .filter(|stage| match stage {
            Stage::Ocr => !state.has_ocr_completion_tag && stage_needs_work(&state.ocr_status),
            // The consolidated stage subsumes the six per-field stages. A document needs the
            // metadata stage if its dedicated metadata_status column needs work OR any of the
            // six legacy per-field columns still report work. Honoring the legacy columns
            // lets v1.3 inventory snapshots (created before metadata_status existed) still
            // flow through the v1.4 selector without a backfill migration.
            Stage::Metadata => {
                !state.has_tagging_completion_tag
                    && (stage_needs_work(&state.metadata_status)
                        || stage_needs_work(&state.tagging_status)
                        || stage_needs_work(&state.title_status)
                        || stage_needs_work(&state.correspondent_status)
                        || stage_needs_work(&state.document_type_status)
                        || stage_needs_work(&state.document_date_status)
                        || stage_needs_work(&state.fields_status))
            }
            Stage::Apply => false,
        })
        .collect()
}

fn stage_needs_work(status: &str) -> bool {
    !matches!(status, "succeeded" | "skipped" | "not_needed")
}

pub async fn claim_jobs(
    pool: &DbPool,
    limit: i64,
    lease_owner: &str,
    lease_seconds: i64,
) -> Result<Vec<JobRecord>> {
    // v1.4.0: `priority` now carries the cross-run (age-derived) value while `stage_priority`
    // enforces within-run stage ordering. The inner subquery uses stage_priority so all jobs
    // of one run share the same `priority` value without losing OCR -> Metadata ordering. The
    // outer ORDER BY claims newer documents first (smaller priority), then earlier stages
    // (smaller stage_priority), then FIFO as a tiebreaker. The retry bias (failed jobs first)
    // stays first in the order so a stuck retry never starves out.
    // Both `priority` and `stage_priority` are STORED generated columns (0019/0030), so the
    // partial `idx_jobs_claim` (priority, stage_priority, run_after, created_at) where
    // status='queued' backs this ordering — the column names and values are unchanged.
    // The claim and its run/inventory follow-ups run in one TX so a crash between them can't
    // leave jobs `running` while their run/inventory rows stay `queued`.
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        r#"
        with claimed as (
          select id,
                 status as prior_status,
                 lease_owner as prior_lease_owner,
                 attempts as prior_attempts
            from jobs
           where ((status = 'queued' and run_after <= now())
              or (status = 'running' and lease_until < now()))
             and not exists (
               select 1
                 from jobs prev
                where prev.run_id = jobs.run_id
                  and prev.stage_priority < jobs.stage_priority
                  and prev.status in ('queued', 'running', 'waiting_review', 'failed')
             )
           order by case when error_message is not null and attempts > 0 then 0 else 1 end,
                    priority,
                    stage_priority,
                    run_after,
                    created_at
           for update skip locked
           limit $1
        ),
        updated as (
          update jobs j
             set status = 'running',
                 lease_owner = $2,
                 lease_until = now() + make_interval(secs => $3),
                 attempts = attempts + 1,
                 updated_at = now()
            from claimed
           where j.id = claimed.id
          returning j.id, j.run_id, j.paperless_document_id, j.stage, j.status,
                    j.attempts, j.max_attempts, j.payload,
                    claimed.prior_status, claimed.prior_lease_owner, claimed.prior_attempts
        )
        select u.id, u.run_id, u.paperless_document_id, u.stage, r.mode, u.status,
               u.attempts, u.max_attempts, u.payload,
               u.prior_status, u.prior_lease_owner, u.prior_attempts
          from updated u
          join pipeline_runs r on r.id = u.run_id
        "#,
    )
    .bind(limit)
    .bind(lease_owner)
    .bind(lease_seconds as f64)
    .fetch_all(&mut *tx)
    .await?;

    let mut jobs = Vec::new();
    // (job_id, run_id, document_id, prior_lease_owner, prior_attempts) for stale-lease reclaims.
    let mut reclaimed: Vec<(Uuid, Uuid, i32, Option<String>, i32)> = Vec::new();
    for row in rows {
        let stage: String = row.try_get("stage")?;
        let mode: String = row.try_get("mode")?;
        let prior_status: String = row.try_get("prior_status")?;
        let job = JobRecord {
            id: row.try_get("id")?,
            run_id: row.try_get("run_id")?,
            paperless_document_id: row.try_get("paperless_document_id")?,
            stage: stage.parse()?,
            mode: mode.parse()?,
            status: row.try_get("status")?,
            attempts: row.try_get("attempts")?,
            max_attempts: row.try_get("max_attempts")?,
            payload: row.try_get("payload")?,
        };
        // A prior status of `running` means we reclaimed a job whose lease expired (worker
        // crash/OOM). The `attempts + 1` above silently burns one of `max_attempts` with no
        // terminal outcome, so leave a breadcrumb to keep that lost attempt attributable.
        if prior_status == "running" {
            let prior_lease_owner: Option<String> = row.try_get("prior_lease_owner")?;
            let prior_attempts: i32 = row.try_get("prior_attempts")?;
            tracing::warn!(
                job_id = %job.id,
                run_id = %job.run_id,
                stage = %job.stage,
                prior_lease_owner = prior_lease_owner.as_deref().unwrap_or("<unknown>"),
                attempts = job.attempts,
                "reclaiming job with expired lease; previous attempt consumed without a terminal outcome"
            );
            reclaimed.push((
                job.id,
                job.run_id,
                job.paperless_document_id,
                prior_lease_owner,
                prior_attempts,
            ));
        }
        jobs.push(job);
    }

    for (job_id, run_id, document_id, prior_lease_owner, prior_attempts) in &reclaimed {
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: "job.lease_reclaimed".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: Some(lease_owner.to_owned()),
                run_id: Some(*run_id),
                job_id: Some(*job_id),
                paperless_document_id: Some(*document_id),
                before: None,
                after: None,
                metadata: Some(json!({
                    "prior_lease_owner": prior_lease_owner,
                    "prior_attempts": prior_attempts,
                    "new_lease_owner": lease_owner,
                })),
                outcome: "warning".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }

    if !jobs.is_empty() {
        // Coalesce the per-job mark_run_running follow-up into a pair of bulk UPDATEs so the
        // claim path issues O(1) queries per batch instead of O(N).
        let mut run_ids: Vec<Uuid> = jobs.iter().map(|job| job.run_id).collect();
        run_ids.sort_unstable();
        run_ids.dedup();
        let mut document_ids: Vec<i32> = jobs.iter().map(|job| job.paperless_document_id).collect();
        document_ids.sort_unstable();
        document_ids.dedup();

        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'running',
                   started_at = coalesce(started_at, now()),
                   updated_at = now()
             where id = any($1::uuid[])
               and status in ('queued', 'running', 'waiting_review')
            "#,
        )
        .bind(&run_ids)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'running',
                   updated_at = now()
             where paperless_document_id = any($1::int[])
               and current_run_status in ('queued', 'running', 'waiting_review')
            "#,
        )
        .bind(&document_ids)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(jobs)
}

/// Extend the lease on a job we are actively processing. Long multi-page OCR
/// jobs can outlive the lease window granted by `claim_jobs`, which would let a
/// second replica reclaim the stale lease and double-apply the work. The
/// processing worker calls this after each unit of progress to push
/// `lease_until` forward by the same window. Returns `true` when our lease was
/// still held (a row matched) and `false` when the lease was lost — the caller
/// should stop in that case so it can't fight a replica that already took over.
pub async fn bump_job_lease(
    pool: &DbPool,
    job_id: Uuid,
    lease_owner: &str,
    lease_seconds: i64,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        update jobs
           set lease_until = now() + make_interval(secs => $3),
               updated_at = now()
         where id = $1
           and lease_owner = $2
           and status = 'running'
        "#,
    )
    .bind(job_id)
    .bind(lease_owner)
    .bind(lease_seconds as f64)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Mark a single run + inventory row as running. `claim_jobs` issues equivalent updates in bulk;
/// this helper exists for callers outside the claim path that legitimately need to flip exactly
/// one run.
#[allow(dead_code)]
pub async fn mark_run_running(pool: &DbPool, run_id: Uuid, document_id: i32) -> Result<()> {
    sqlx::query(
        r#"
        update pipeline_runs
           set status = 'running',
               started_at = coalesce(started_at, now()),
               updated_at = now()
         where id = $1 and status in ('queued', 'running', 'waiting_review')
        "#,
    )
    .bind(run_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        update document_inventory
           set current_run_status = 'running',
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(document_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a job succeeded, but only if `lease_owner` still owns the lease. Returns
/// `true` when our row matched (we owned the lease and applied the completion)
/// and `false` when no row matched — the lease was lost to another replica that
/// reclaimed it. Callers must treat `false` as "lease lost, skip" rather than an
/// error: writing unconditionally would let a worker that already lost its lease
/// double-apply over the replica that legitimately took over.
pub async fn complete_job(
    pool: &DbPool,
    job: &JobRecord,
    lease_owner: &str,
    result: Value,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        r#"
        update jobs
           set status = 'succeeded',
               result = $2,
               lease_owner = null,
               lease_until = null,
               error_message = null,
               updated_at = now()
         where id = $1
           and lease_owner = $3
        "#,
    )
    .bind(job.id)
    .bind(&result)
    .bind(lease_owner)
    .execute(&mut *tx)
    .await?;

    // Lease lost: another replica reclaimed the stale lease and owns this job
    // now. Roll back without touching inventory/run state so we don't fight the
    // worker that took over.
    if updated.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }

    set_inventory_stage_status_tx(
        &mut tx,
        job.paperless_document_id,
        job.stage,
        "succeeded",
        None,
        false,
        Some(job.run_id),
    )
    .await?;

    if no_remaining_jobs_tx(&mut tx, job.run_id).await? {
        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'succeeded', finished_at = now(), updated_at = now()
             where id = $1
            "#,
        )
        .bind(job.run_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'succeeded',
                   complete = true,
                   updated_at = now()
             where paperless_document_id = $1
            "#,
        )
        .bind(job.paperless_document_id)
        .execute(&mut *tx)
        .await?;
    } else {
        // Reset pipeline_runs.status back to 'queued' when stage-N succeeded
        // but stage-N+1 is still pending. Without this, runs that went through
        // a stage via the direct (non-review) full_auto path stay stuck on
        // 'running' forever, which surfaces as "N stuck run(s) — pipeline runs
        // have not progressed in the last 10 minutes" on the dashboard even
        // though the next-stage jobs ARE waiting in the queue and will be
        // claimed normally. Mirrors mark_review_auto_applied behavior.
        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'queued', updated_at = now()
             where id = $1
               and status not in ('succeeded', 'failed', 'cancelled')
            "#,
        )
        .bind(job.run_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'queued', updated_at = now()
             where paperless_document_id = $1
               and last_run_id = $2
            "#,
        )
        .bind(job.paperless_document_id)
        .bind(job.run_id)
        .execute(&mut *tx)
        .await?;
    }

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "job.succeeded".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: None,
            after: Some(result),
            metadata: Some(json!({ "stage": job.stage })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn is_last_active_job(pool: &DbPool, run_id: Uuid, current_job_id: Uuid) -> Result<bool> {
    let row = sqlx::query(
        r#"
        select not exists(
          select 1 from jobs
           where run_id = $1
             and id <> $2
             and status in ('queued', 'running', 'waiting_review')
        ) as is_last
        "#,
    )
    .bind(run_id)
    .bind(current_job_id)
    .fetch_one(pool)
    .await?;
    row.try_get("is_last").context("read last active job state")
}

/// Mark a job failed (or schedule a retry), but only if `lease_owner` still owns
/// the lease. Returns `true` when our row matched and `false` when the lease was
/// lost to a replica that reclaimed it. As with `complete_job`, callers must
/// treat `false` as "lease lost, skip" — a worker that lost its lease must not
/// overwrite the state of the replica that took over.
pub async fn fail_job(
    pool: &DbPool,
    job: &JobRecord,
    lease_owner: &str,
    error: &str,
    retryable: bool,
) -> Result<bool> {
    let retry = retryable && job.attempts < job.max_attempts;
    let status = if retry { "queued" } else { "failed" };
    let base_delay = (2_i64.pow(job.attempts.clamp(0, 6) as u32)) * 30;
    // +/-25% uniform jitter avoids thundering-herd retries when many workers
    // hit the same transient upstream failure (e.g. provider restart).
    let jitter = (rand::random::<f64>() - 0.5) * 0.5 * base_delay as f64;
    let delay_seconds = ((base_delay as f64) + jitter).max(1.0);
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        r#"
        update jobs
           set status = $2,
               error_message = $3,
               lease_owner = null,
               lease_until = null,
               run_after = case when $2 = 'queued' then now() + make_interval(secs => $4) else run_after end,
               updated_at = now()
         where id = $1
           and lease_owner = $5
        "#,
    )
    .bind(job.id)
    .bind(status)
    .bind(error)
    .bind(delay_seconds)
    .bind(lease_owner)
    .execute(&mut *tx)
    .await?;

    // Lease lost: a replica reclaimed the stale lease. Roll back without
    // cancelling siblings or flipping run state so we don't clobber the worker
    // that took over.
    if updated.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }

    if !retry {
        set_inventory_stage_status_tx(
            &mut tx,
            job.paperless_document_id,
            job.stage,
            "failed",
            Some(error),
            false,
            Some(job.run_id),
        )
        .await?;
        sqlx::query(
            "update pipeline_runs set status = 'failed', error_message = $2, finished_at = now(), updated_at = now() where id = $1",
        )
        .bind(job.run_id)
        .bind(error)
        .execute(&mut *tx)
        .await?;
        // A permanent failure aborts the whole run, so cancel the sibling jobs in the same TX.
        // Mirrors the reject path: leaving them `queued` makes them unclaimable (the claim guard
        // blocks them behind the failed stage) yet still scanned on every poll, inflating
        // `jobs_queued` forever.
        sqlx::query(
            r#"
            update jobs
               set status = 'cancelled',
                   lease_owner = null,
                   lease_until = null,
                   updated_at = now()
             where run_id = $1
               and id <> $2
               and status in ('queued', 'running', 'waiting_review')
            "#,
        )
        .bind(job.run_id)
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
    }

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: if retry {
                "job.retry_scheduled"
            } else {
                "job.failed"
            }
            .to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: None,
            after: Some(json!({ "status": status, "retry": retry })),
            metadata: Some(json!({ "stage": job.stage })),
            outcome: if retry { "retry" } else { "failed" }.to_owned(),
            error_message: Some(error.to_owned()),
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    if retry {
        increment_metric_counter_tx(&mut tx, "job_retries_scheduled_total", 1).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Insert a review item for `job` and flip job/run/inventory to
/// `waiting_review`. Like `complete_job`/`fail_job`, the job update is fenced
/// on `lease_owner`: a worker whose lease was reclaimed gets `Ok(None)` and
/// must stop instead of inserting review items for a job another replica now
/// owns. A job already in `waiting_review` still matches for the same owner,
/// so one stage can create several per-field items back to back.
pub async fn create_review_item(
    pool: &DbPool,
    job: &JobRecord,
    suggested_patch: Value,
    validation_warnings: Value,
    lease_owner: &str,
) -> Result<Option<Uuid>> {
    let mut tx = pool.begin().await?;
    let fenced = sqlx::query(
        r#"
        update jobs
           set status = 'waiting_review', updated_at = now()
         where id = $1
           and lease_owner = $2
           and status in ('running', 'waiting_review')
        "#,
    )
    .bind(job.id)
    .bind(lease_owner)
    .execute(&mut *tx)
    .await?;
    if fenced.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(None);
    }

    let id: Uuid = sqlx::query(
        r#"
        insert into review_items (run_id, job_id, paperless_document_id, stage, status, suggested_patch, validation_warnings)
        values ($1, $2, $3, $4, 'pending', $5, $6)
        returning id
        "#,
    )
    .bind(job.run_id)
    .bind(job.id)
    .bind(job.paperless_document_id)
    .bind(job.stage.to_string())
    .bind(&suggested_patch)
    .bind(&validation_warnings)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;

    sqlx::query(
        "update pipeline_runs set status = 'waiting_review', updated_at = now() where id = $1",
    )
    .bind(job.run_id)
    .execute(&mut *tx)
    .await?;
    set_inventory_stage_status_tx(
        &mut tx,
        job.paperless_document_id,
        job.stage,
        "waiting_review",
        None,
        true,
        Some(job.run_id),
    )
    .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "review.created".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: None,
            after: Some(json!({ "review_id": id, "stage": job.stage })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Some(id))
}

pub async fn list_reviews(
    pool: &DbPool,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<ReviewItemRecord>> {
    let rows = if let Some(status) = status {
        sqlx::query(
            r#"
            select ri.id, ri.run_id, ri.job_id, ri.paperless_document_id, ri.stage, ri.status,
                   ri.suggested_patch, ri.edited_patch, ri.validation_warnings, ri.created_at,
                   di.title as paperless_title,
                   jsonb_build_object(
                     'detected_language', di.detected_language,
                     'detected_language_confidence', di.detected_language_confidence,
                     'detected_language_source', di.detected_language_source,
                     'current_run_status', di.current_run_status,
                     'last_error', di.last_error,
                     'next_required_stage', di.next_required_stage
                   ) as debug_context
              from review_items ri
              left join document_inventory di
                on di.paperless_document_id = ri.paperless_document_id
             where ri.status = $1
             order by ri.created_at desc
             limit $2
            "#,
        )
        .bind(status)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            r#"
            select ri.id, ri.run_id, ri.job_id, ri.paperless_document_id, ri.stage, ri.status,
                   ri.suggested_patch, ri.edited_patch, ri.validation_warnings, ri.created_at,
                   di.title as paperless_title,
                   jsonb_build_object(
                     'detected_language', di.detected_language,
                     'detected_language_confidence', di.detected_language_confidence,
                     'detected_language_source', di.detected_language_source,
                     'current_run_status', di.current_run_status,
                     'last_error', di.last_error,
                     'next_required_stage', di.next_required_stage
                   ) as debug_context
              from review_items ri
              left join document_inventory di
                on di.paperless_document_id = ri.paperless_document_id
             order by ri.created_at desc
             limit $1
            "#,
        )
        .bind(limit)
        .fetch_all(pool)
        .await?
    };

    rows.into_iter()
        .map(|row| {
            let stage: String = row.try_get("stage")?;
            Ok(ReviewItemRecord {
                id: row.try_get("id")?,
                run_id: row.try_get("run_id")?,
                job_id: row.try_get("job_id")?,
                paperless_document_id: row.try_get("paperless_document_id")?,
                stage: stage.parse()?,
                status: row.try_get("status")?,
                suggested_patch: row.try_get("suggested_patch")?,
                edited_patch: row.try_get("edited_patch")?,
                validation_warnings: row.try_get("validation_warnings")?,
                debug_context: row.try_get("debug_context")?,
                paperless_title: row.try_get("paperless_title").ok(),
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

/// Count review items matching the same optional `status` filter used by
/// [`list_reviews`]. Lets the API report an honest total alongside a clamped page.
pub async fn count_reviews(pool: &DbPool, status: Option<&str>) -> Result<i64> {
    let count = if let Some(status) = status {
        sqlx::query_scalar::<_, i64>(r#"select count(*) from review_items where status = $1"#)
            .bind(status)
            .fetch_one(pool)
            .await?
    } else {
        sqlx::query_scalar::<_, i64>(r#"select count(*) from review_items"#)
            .fetch_one(pool)
            .await?
    };
    Ok(count)
}

pub async fn review_decision(
    pool: &DbPool,
    review_id: Uuid,
    status: &str,
    edited_patch: Option<Value>,
    actor_id: Uuid,
) -> Result<()> {
    if !matches!(status, "approved" | "rejected" | "edited") {
        return Err(anyhow!("invalid review decision status"));
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        update review_items
           set status = $2,
               edited_patch = coalesce($3, edited_patch),
               reviewed_by = $4,
               reviewed_at = now()
         where id = $1 and status = 'pending'
        returning run_id, job_id, paperless_document_id, stage, suggested_patch, edited_patch
        "#,
    )
    .bind(review_id)
    .bind(status)
    .bind(&edited_patch)
    .bind(actor_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| anyhow!("review item is not pending or does not exist"))?;

    let run_id: Uuid = row.try_get("run_id")?;
    let job_id: Option<Uuid> = row.try_get("job_id")?;
    let document_id: i32 = row.try_get("paperless_document_id")?;
    let stage_text: String = row.try_get("stage")?;
    let stage: Stage = stage_text.parse()?;
    let suggested_patch: Value = row.try_get("suggested_patch")?;
    let stored_edited_patch: Option<Value> = row.try_get("edited_patch")?;

    if status == "rejected" {
        sqlx::query(
            r#"
            update jobs
               set status = 'cancelled',
                   lease_owner = null,
                   lease_until = null,
                   updated_at = now()
             where run_id = $1
               and status in ('queued', 'running', 'waiting_review')
            "#,
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        set_inventory_stage_status_tx(
            &mut tx,
            document_id,
            stage,
            "rejected",
            None,
            false,
            Some(run_id),
        )
        .await?;
        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'rejected',
                   finished_at = now(),
                   updated_at = now()
             where id = $1
            "#,
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'rejected',
                   updated_at = now()
             where paperless_document_id = $1
            "#,
        )
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    }

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: format!("review.{status}"),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: Some(run_id),
            job_id,
            paperless_document_id: Some(document_id),
            before: Some(suggested_patch),
            after: edited_patch.or(stored_edited_patch),
            metadata: Some(json!({ "review_id": review_id, "stage": stage_text })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Atomically claim an approved/edited review item for application by moving
/// it to the intermediate `applying` status, returning the record with its
/// *prior* status (so a failed apply can revert precisely). Returns `None`
/// when no row is in an applyable status — i.e. another apply (a second
/// operator, or the autopilot drain) already owns it. This is the fence that
/// prevents a document being PATCHed to Paperless twice. #253.
pub async fn claim_review_for_apply(
    pool: &DbPool,
    review_id: Uuid,
) -> Result<Option<ReviewItemRecord>> {
    let row = sqlx::query(
        r#"
        with prev as (
          select status from review_items where id = $1
        )
        update review_items
           -- Re-stamp reviewed_at so the stale-applying recovery sweep (which
           -- keys its 300s timer off reviewed_at) measures from the claim, not
           -- the original approval — otherwise a slow apply could be reverted
           -- mid-flight and double-applied. #295
           set status = 'applying', reviewed_at = now()
         where id = $1 and status in ('approved', 'edited')
        returning id, run_id, job_id, paperless_document_id, stage,
                  (select status from prev) as status,
                  suggested_patch, edited_patch, validation_warnings, created_at
        "#,
    )
    .bind(review_id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        let stage: String = row.try_get("stage")?;
        Ok(ReviewItemRecord {
            id: row.try_get("id")?,
            run_id: row.try_get("run_id")?,
            job_id: row.try_get("job_id")?,
            paperless_document_id: row.try_get("paperless_document_id")?,
            stage: stage.parse()?,
            status: row.try_get("status")?,
            suggested_patch: row.try_get("suggested_patch")?,
            edited_patch: row.try_get("edited_patch")?,
            validation_warnings: row.try_get("validation_warnings")?,
            debug_context: None,
            paperless_title: None,
            created_at: row.try_get("created_at")?,
        })
    })
    .transpose()
}

pub async fn mark_review_applied(pool: &DbPool, review_id: Uuid, actor_id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    // Gate on `applying`: the caller claimed the row via `claim_review_for_apply`
    // before patching Paperless, so a terminal transition is only valid from
    // that owned state. A missing row means another actor already finished it.
    let Some(row) = sqlx::query(
        r#"
        update review_items
           set status = 'applied',
               reviewed_by = coalesce(reviewed_by, $2),
               reviewed_at = coalesce(reviewed_at, now())
         where id = $1 and status = 'applying'
        returning run_id, job_id, paperless_document_id, stage
        "#,
    )
    .bind(review_id)
    .bind(actor_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(());
    };

    let job_id: Option<Uuid> = row.try_get("job_id")?;
    if let Some(job_id) = job_id {
        sqlx::query("update jobs set status = 'succeeded', updated_at = now() where id = $1")
            .bind(job_id)
            .execute(&mut *tx)
            .await?;
    }

    let stage: Stage = row.try_get::<String, _>("stage")?.parse()?;
    let document_id: i32 = row.try_get("paperless_document_id")?;
    let run_id: Uuid = row.try_get("run_id")?;
    set_inventory_stage_status_tx(
        &mut tx,
        document_id,
        stage,
        "succeeded",
        None,
        false,
        Some(run_id),
    )
    .await?;

    if no_remaining_jobs_tx(&mut tx, run_id).await? {
        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'succeeded',
                   finished_at = now(),
                   updated_at = now()
             where id = $1
            "#,
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'succeeded',
                   complete = true,
                   updated_at = now()
             where paperless_document_id = $1
            "#,
        )
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'queued',
                   updated_at = now()
             where id = $1
            "#,
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'queued',
                   updated_at = now()
             where paperless_document_id = $1
            "#,
        )
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    }

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "review.applied".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: Some(run_id),
            job_id,
            paperless_document_id: Some(document_id),
            before: None,
            after: Some(json!({ "review_id": review_id })),
            metadata: Some(json!({ "stage": stage })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// List `pending` review_items for the autopilot drain.
///
/// Oldest-first so the operator-visible backlog is whittled down from the
/// front (and any "stuck for hours" rows leave the dashboard first). The
/// returned shape is identical to [`pending_review_for_apply`] so the worker
/// drain can reuse the same apply path.
pub async fn list_pending_review_items_for_autopilot_drain(
    pool: &DbPool,
    limit: i64,
) -> Result<Vec<ReviewItemRecord>> {
    let rows = sqlx::query(
        r#"
        select id, run_id, job_id, paperless_document_id, stage, status,
               suggested_patch, edited_patch, validation_warnings, created_at
          from review_items
         where status = 'pending'
         order by created_at asc
         limit $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: String = row.try_get("stage")?;
            Ok(ReviewItemRecord {
                id: row.try_get("id")?,
                run_id: row.try_get("run_id")?,
                job_id: row.try_get("job_id")?,
                paperless_document_id: row.try_get("paperless_document_id")?,
                stage: stage.parse()?,
                status: row.try_get("status")?,
                suggested_patch: row.try_get("suggested_patch")?,
                edited_patch: row.try_get("edited_patch")?,
                validation_warnings: row.try_get("validation_warnings")?,
                debug_context: None,
                paperless_title: None,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

/// Atomically claim a pending review item for autopilot drain.
///
/// Flips the row from `pending` → `approved` and stamps a `review.approved`
/// audit event with `actor_type = "worker"` and `trigger = "autopilot_drain"`
/// in the metadata so post-hoc analysis can distinguish these from
/// human-initiated approvals. Returns the claimed record when the transition
/// succeeded, or `Ok(None)` if the row was no longer pending (raced by a
/// human reviewer or another worker tick).
pub async fn claim_pending_review_for_autopilot_drain(
    pool: &DbPool,
    review_id: Uuid,
) -> Result<Option<ReviewItemRecord>> {
    let mut tx = pool.begin().await?;
    // Claim straight into `applying` (not `approved`): the drain is about to
    // PATCH Paperless, so the row must be in the owned state that blocks a
    // concurrent human apply from patching the same document. #253.
    let Some(row) = sqlx::query(
        r#"
        update review_items
           set status = 'applying',
               reviewed_at = now()
         where id = $1 and status = 'pending'
        returning id, run_id, job_id, paperless_document_id, stage, status,
                  suggested_patch, edited_patch, validation_warnings, created_at
        "#,
    )
    .bind(review_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(None);
    };

    let stage_text: String = row.try_get("stage")?;
    let stage: Stage = stage_text.parse()?;
    let record = ReviewItemRecord {
        id: row.try_get("id")?,
        run_id: row.try_get("run_id")?,
        job_id: row.try_get("job_id")?,
        paperless_document_id: row.try_get("paperless_document_id")?,
        stage,
        status: row.try_get("status")?,
        suggested_patch: row.try_get("suggested_patch")?,
        edited_patch: row.try_get("edited_patch")?,
        validation_warnings: row.try_get("validation_warnings")?,
        debug_context: None,
        paperless_title: None,
        created_at: row.try_get("created_at")?,
    };

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "review.approved".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(record.run_id),
            job_id: record.job_id,
            paperless_document_id: Some(record.paperless_document_id),
            before: Some(record.suggested_patch.clone()),
            after: record.edited_patch.clone(),
            metadata: Some(json!({
                "review_id": record.id,
                "stage": stage_text,
                "trigger": "autopilot_drain"
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Some(record))
}

/// Mark a review_item as applied via the autopilot drain.
///
/// Mirrors [`mark_review_applied`] but with worker-actor audit (no user ID)
/// and a `trigger = "autopilot_drain"` metadata marker. Updates job and run
/// status to keep the dashboard counters consistent with the existing
/// human-approve path.
pub async fn mark_review_auto_applied(pool: &DbPool, review_id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    // Gate on `applying` (the status the drain claims into), mirroring
    // `mark_review_applied`. #253.
    let Some(row) = sqlx::query(
        r#"
        update review_items
           set status = 'applied',
               reviewed_at = coalesce(reviewed_at, now())
         where id = $1 and status = 'applying'
        returning run_id, job_id, paperless_document_id, stage
        "#,
    )
    .bind(review_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(());
    };

    let job_id: Option<Uuid> = row.try_get("job_id")?;
    if let Some(job_id) = job_id {
        sqlx::query("update jobs set status = 'succeeded', updated_at = now() where id = $1")
            .bind(job_id)
            .execute(&mut *tx)
            .await?;
    }

    let stage: Stage = row.try_get::<String, _>("stage")?.parse()?;
    let document_id: i32 = row.try_get("paperless_document_id")?;
    let run_id: Uuid = row.try_get("run_id")?;
    set_inventory_stage_status_tx(
        &mut tx,
        document_id,
        stage,
        "succeeded",
        None,
        false,
        Some(run_id),
    )
    .await?;

    if no_remaining_jobs_tx(&mut tx, run_id).await? {
        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'succeeded',
                   finished_at = now(),
                   updated_at = now()
             where id = $1
            "#,
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'succeeded',
                   complete = true,
                   updated_at = now()
             where paperless_document_id = $1
            "#,
        )
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'queued',
                   updated_at = now()
             where id = $1
            "#,
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'queued',
                   updated_at = now()
             where paperless_document_id = $1
            "#,
        )
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    }

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "review.applied".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(run_id),
            job_id,
            paperless_document_id: Some(document_id),
            before: None,
            after: Some(json!({ "review_id": review_id })),
            metadata: Some(json!({
                "stage": stage,
                "trigger": "autopilot_drain"
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Roll a review item back to `pending` after a failed autopilot drain apply.
///
/// Used when the Paperless PATCH (or any subsequent step) errors after
/// [`claim_pending_review_for_autopilot_drain`] has already flipped the row
/// to `approved`. Without rollback the row would be stuck in `approved`
/// status with no apply ever performed.
pub async fn revert_review_to_pending_after_failed_drain(
    pool: &DbPool,
    review_id: Uuid,
) -> Result<()> {
    sqlx::query(
        r#"
        update review_items
           set status = 'pending',
               reviewed_at = null
         where id = $1 and status = 'applying'
        "#,
    )
    .bind(review_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Release a human-apply claim after the Paperless PATCH failed: move the row
/// from `applying` back to the status it had before the claim so the operator
/// can retry. #253.
pub async fn revert_review_from_applying(
    pool: &DbPool,
    review_id: Uuid,
    to_status: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        update review_items
           set status = $2
         where id = $1 and status = 'applying'
        "#,
    )
    .bind(review_id)
    .bind(to_status)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct AiArtifactInput<'a> {
    pub run_id: Uuid,
    pub job_id: Uuid,
    pub stage: Stage,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_id: Option<Uuid>,
    pub input_hash: &'a str,
    pub request: Option<Value>,
    pub response: Option<Value>,
    pub normalized_output: Option<Value>,
    pub duration_ms: i32,
    pub storage_mode: AiArtifactStorageMode,
}

/// Look up a cached OCR result for a (document, page_index, page_hash)
/// triple. Returns the cached text when present. Added in v1.5.14 to
/// short-circuit expensive vision-model calls when the exact same
/// rendered page has been transcribed before.
pub async fn lookup_ocr_page_cache(
    pool: &DbPool,
    paperless_document_id: i32,
    page_index: i32,
    page_hash: &str,
) -> Result<Option<String>> {
    let row = sqlx::query(
        r#"
        select ocr_text from ocr_page_cache
         where paperless_document_id = $1
           and page_index = $2
           and page_hash = $3
         limit 1
        "#,
    )
    .bind(paperless_document_id)
    .bind(page_index)
    .bind(page_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|r| r.try_get::<String, _>("ocr_text"))
        .transpose()?)
}

/// Insert (or update) an OCR-page-cache row. Idempotent on the
/// (document, page_index, page_hash) primary key — the second call for
/// the same triple overwrites the cached text and bumps `created_at`.
pub async fn upsert_ocr_page_cache(
    pool: &DbPool,
    paperless_document_id: i32,
    page_index: i32,
    page_hash: &str,
    ocr_text: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into ocr_page_cache
            (paperless_document_id, page_index, page_hash, ocr_text, provider, model)
        values ($1, $2, $3, $4, $5, $6)
        on conflict (paperless_document_id, page_index, page_hash)
        do update set
            ocr_text = excluded.ocr_text,
            provider = excluded.provider,
            model = excluded.model,
            created_at = now()
        "#,
    )
    .bind(paperless_document_id)
    .bind(page_index)
    .bind(page_hash)
    .bind(ocr_text)
    .bind(provider)
    .bind(model)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record the OCR-content-hash on `document_inventory`. Used as the
/// key for the content-hash dedup helper below. Idempotent.
pub async fn set_document_inventory_ocr_content_hash(
    pool: &DbPool,
    paperless_document_id: i32,
    ocr_content_hash: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        update document_inventory
           set ocr_content_hash = $2,
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .bind(ocr_content_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist the (sanitized) OCR body on `document_inventory` so it can be
/// indexed for full-text retrieval in chat search (#217). The companion
/// generated `ocr_body_tsv` column is recomputed by Postgres on write.
/// Idempotent; best-effort from the worker's point of view (a failure
/// here does not fail the OCR stage).
pub async fn set_document_inventory_ocr_body(
    pool: &DbPool,
    paperless_document_id: i32,
    ocr_body: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        update document_inventory
           set ocr_body = $2,
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .bind(ocr_body)
    .execute(pool)
    .await?;
    Ok(())
}

/// Find a recent document whose OCR text hash matches and whose
/// metadata stage has already settled (succeeded). Used by the
/// metadata stage to short-circuit a re-extraction when the same
/// document content has been processed before. Returns the dedup
/// source's `paperless_document_id` and the most recent succeeded
/// metadata `ai_artifacts.normalized` payload so the caller can clone
/// the patch.
pub async fn find_metadata_dedup_source(
    pool: &DbPool,
    current_document_id: i32,
    ocr_content_hash: &str,
) -> Result<Option<(i32, Value)>> {
    let row = sqlx::query(
        r#"
        select di.paperless_document_id as source_id,
               aa.normalized_output as metadata_payload
          from document_inventory di
          join pipeline_runs pr
            on pr.paperless_document_id = di.paperless_document_id
          join ai_artifacts aa
            on aa.run_id = pr.id
           and aa.stage = 'metadata'
         where di.ocr_content_hash = $1
           and di.paperless_document_id <> $2
           and di.metadata_status = 'succeeded'
         order by aa.created_at desc
         limit 1
        "#,
    )
    .bind(ocr_content_hash)
    .bind(current_document_id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| -> Result<(i32, Value)> {
        Ok((r.try_get("source_id")?, r.try_get("metadata_payload")?))
    })
    .transpose()
}

/// Maximum number of duplicate groups returned by [`list_inventory_duplicates`].
/// Keeps the read-only dedup endpoint cheap and the payload bounded; the
/// caller logs when the result is truncated at this cap.
pub const DUPLICATE_GROUP_LIMIT: i64 = 200;

/// Group `document_inventory` rows by `ocr_content_hash`, returning every hash
/// shared by more than one document (#216 dedup view). Rows with a null hash
/// are excluded. When more than [`DUPLICATE_GROUP_LIMIT`] groups exist the
/// largest (most-duplicated) groups are kept; the returned groups themselves
/// are ordered by hash.
pub async fn list_inventory_duplicates(pool: &DbPool) -> Result<Vec<DuplicateGroup>> {
    let rows = sqlx::query(
        r#"
        select ocr_content_hash as hash,
               paperless_document_id,
               title
          from document_inventory
         where ocr_content_hash is not null
           and ocr_content_hash in (
                 select ocr_content_hash
                   from document_inventory
                  where ocr_content_hash is not null
                  group by ocr_content_hash
                 having count(*) > 1
                  order by count(*) desc
                  limit $1
               )
         order by ocr_content_hash, paperless_document_id
        "#,
    )
    .bind(DUPLICATE_GROUP_LIMIT)
    .fetch_all(pool)
    .await?;

    let mut groups: Vec<DuplicateGroup> = Vec::new();
    for row in rows {
        let hash: String = row.try_get("hash")?;
        let document = DuplicateDocument {
            paperless_document_id: row.try_get("paperless_document_id")?,
            title: row.try_get("title")?,
        };
        match groups.last_mut() {
            Some(group) if group.hash == hash => group.documents.push(document),
            _ => groups.push(DuplicateGroup {
                hash,
                documents: vec![document],
            }),
        }
    }
    Ok(groups)
}

pub async fn insert_ai_artifact(pool: &DbPool, input: AiArtifactInput<'_>) -> Result<Uuid> {
    let request = prepare_ai_artifact_value(input.request, input.storage_mode);
    let response = prepare_ai_artifact_value(input.response, input.storage_mode);

    let id = sqlx::query(
        r#"
        insert into ai_artifacts (
          run_id, job_id, stage, provider, model, prompt_id, input_hash, request, response, normalized_output, duration_ms
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        returning id
        "#,
    )
    .bind(input.run_id)
    .bind(input.job_id)
    .bind(input.stage.to_string())
    .bind(input.provider)
    .bind(input.model)
    .bind(input.prompt_id)
    .bind(input.input_hash)
    .bind(request)
    .bind(response)
    .bind(input.normalized_output)
    .bind(input.duration_ms)
    .fetch_one(pool)
    .await?
    .try_get("id")?;
    Ok(id)
}

fn prepare_ai_artifact_value(
    value: Option<Value>,
    storage_mode: AiArtifactStorageMode,
) -> Option<Value> {
    let mut value = value?;
    redact_sensitive_json(&mut value);
    match storage_mode {
        AiArtifactStorageMode::Full => Some(value),
        AiArtifactStorageMode::Redacted => {
            redact_ai_artifact_content(&mut value);
            Some(value)
        }
        AiArtifactStorageMode::MetadataOnly => Some(ai_artifact_metadata_only(&value)),
    }
}

fn redact_ai_artifact_content(value: &mut Value) {
    const CONTENT_KEYS: &[&str] = &[
        "content",
        "text",
        "prompt",
        "system_prompt",
        "user_prompt",
        "response",
        "images",
        "image",
        "bytes",
        "b64_json",
        "base64",
    ];

    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                let content_key = CONTENT_KEYS
                    .iter()
                    .any(|needle| key.to_ascii_lowercase().contains(needle));
                // `prompt_tokens` / `prompt_eval_count` match the "prompt"
                // substring but are numeric counters, not document content —
                // summarizing them away destroys token statistics.
                if content_key && !matches!(nested, Value::Number(_) | Value::Bool(_)) {
                    *nested = ai_artifact_redaction_summary(nested);
                } else {
                    redact_ai_artifact_content(nested);
                }
            }
        }
        Value::Array(values) => {
            for nested in values {
                redact_ai_artifact_content(nested);
            }
        }
        _ => {}
    }
}

fn ai_artifact_redaction_summary(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({
            "redacted": true,
            "kind": "text",
            "sha256": short_hash(text),
            "chars": text.chars().count()
        }),
        Value::Array(items) => json!({
            "redacted": true,
            "kind": "array",
            "items": items.len()
        }),
        Value::Object(map) => json!({
            "redacted": true,
            "kind": "object",
            "keys": map.len()
        }),
        Value::Null => Value::Null,
        other => json!({
            "redacted": true,
            "kind": "scalar",
            "sha256": short_hash(&other.to_string())
        }),
    }
}

fn ai_artifact_metadata_only(value: &Value) -> Value {
    let mut metadata = json!({
        "storage": "metadata_only",
        "sha256": short_hash(&value.to_string()),
        "json_bytes": value.to_string().len()
    });
    if let (Some(target), Value::Object(source)) = (metadata.as_object_mut(), value) {
        for key in [
            "model",
            "provider",
            "stage",
            "usage",
            "options",
            "done_reason",
        ] {
            if let Some(value) = source.get(key) {
                target.insert(key.to_owned(), value.clone());
            }
        }
    }
    metadata
}

/// Atomically bump a monotone metric counter, creating the row on first use.
///
/// Unlike the `audit_events`-derived gauges in [`metrics_snapshot`], these
/// counters live in their own table and are therefore unaffected by audit
/// retention pruning, keeping the `/metrics` series monotone and `rate()`-safe.
/// Increment exactly once at the source event so the counter never double-counts.
pub async fn increment_metric_counter(pool: &DbPool, name: &str, by: i64) -> Result<()> {
    sqlx::query(
        r#"
        insert into metrics_counters (name, value, updated_at)
        values ($1, $2, now())
        on conflict (name) do update
           set value = metrics_counters.value + excluded.value,
               updated_at = now()
        "#,
    )
    .bind(name)
    .bind(by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Transaction-scoped variant of [`increment_metric_counter`] so a counter bump
/// can be made atomic with the source event written in the same transaction.
async fn increment_metric_counter_tx(
    tx: &mut Transaction<'_, Postgres>,
    name: &str,
    by: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into metrics_counters (name, value, updated_at)
        values ($1, $2, now())
        on conflict (name) do update
           set value = metrics_counters.value + excluded.value,
               updated_at = now()
        "#,
    )
    .bind(name)
    .bind(by)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Read all monotone metric counters as a `name -> value` map.
pub async fn read_metric_counters(pool: &DbPool) -> Result<HashMap<String, i64>> {
    let rows = sqlx::query("select name, value from metrics_counters")
        .fetch_all(pool)
        .await?;
    let mut counters = HashMap::with_capacity(rows.len());
    for row in rows {
        let name: String = row.try_get("name")?;
        let value: i64 = row.try_get("value")?;
        counters.insert(name, value);
    }
    Ok(counters)
}

pub async fn metrics_snapshot(pool: &DbPool) -> Result<MetricsSnapshot> {
    let row = sqlx::query(
        r#"
        select
          (select count(*)::bigint from jobs where status = 'queued') as jobs_queued,
          (select count(*)::bigint from jobs where status = 'running') as jobs_running,
          (select count(*)::bigint from jobs where status = 'failed') as jobs_failed,
          (select count(*)::bigint from jobs where status = 'succeeded') as jobs_succeeded,
          (select count(*)::bigint from review_items where status = 'pending') as reviews_pending,
          (select count(*)::bigint from pipeline_runs where status in ('queued', 'running', 'waiting_review', 'applying')) as runs_active,
          -- Total event count keeps an exact count(*); switch to a pg_class
          -- reltuples estimate if this scan ever becomes a hot spot.
          -- Planner estimate instead of a full count(*): audit_events is
          -- unbounded and this query runs on every /metrics scrape.
          (select greatest(reltuples, 0)::bigint from pg_class where oid = 'audit_events'::regclass) as audit_events,
          (select count(*)::bigint from audit_events where event_type = 'workflow.selector_ran') as selector_runs_total,
          coalesce((
            select sum(coalesce((after ->> 'queued')::bigint, 0))::bigint
              from audit_events
             where event_type = 'workflow.selector_ran'
          ), 0) as selector_documents_queued_total,
          (select count(*)::bigint from audit_events where event_type = 'job.retry_scheduled') as job_retries_scheduled_total,
          -- LLM stages only. Pre-v1.4 used six per-field stages; the live
          -- Stage enum is ocr / metadata / apply, and metadata (the main LLM
          -- stage) was missing here so its errors were invisible to the
          -- metric. apply is a Paperless PATCH, not a model call. #262.
          (select count(*)::bigint
             from jobs
            where error_message is not null
              and stage in ('ocr', 'metadata')
          ) as model_errors_total,
          (select count(*)::bigint from audit_events where event_type = 'document.patch_applied' and outcome = 'success') as apply_success_total,
          (select count(*)::bigint from audit_events where event_type = 'document.patch_apply_failed' and outcome = 'failed') as apply_failure_total,
          -- Latency aggregates are scoped to a recent window so they no longer
          -- scan the entire unbounded audit_events table; the
          -- (event_type, created_at) index covers this access path.
          coalesce((
            select count(*)::bigint
              from audit_events
             where event_type in ('document.patch_applied', 'document.patch_apply_failed')
               and created_at > now() - interval '24 hours'
               and metadata ? 'duration_ms'
          ), 0) as apply_latency_ms_count,
          coalesce((
            select sum((metadata ->> 'duration_ms')::bigint)::bigint
              from audit_events
             where event_type in ('document.patch_applied', 'document.patch_apply_failed')
               and created_at > now() - interval '24 hours'
               and metadata ? 'duration_ms'
          ), 0) as apply_latency_ms_sum,
          coalesce((
            select (percentile_disc(0.95) within group (order by (metadata ->> 'duration_ms')::bigint))::bigint
              from audit_events
             where event_type in ('document.patch_applied', 'document.patch_apply_failed')
               and created_at > now() - interval '24 hours'
               and metadata ? 'duration_ms'
          ), 0) as apply_latency_ms_p95,
          -- Per-stage latency is sourced from ai_artifacts.duration_ms (the
          -- recorded job timing for each stage round-trip), scoped to the same
          -- 24h window as the apply latency aggregates above.
          coalesce((
            select count(*)::bigint
              from ai_artifacts
             where stage = 'ocr'
               and duration_ms is not null
               and created_at > now() - interval '24 hours'
          ), 0) as ocr_latency_ms_count,
          coalesce((
            select (percentile_disc(0.95) within group (order by duration_ms))::bigint
              from ai_artifacts
             where stage = 'ocr'
               and duration_ms is not null
               and created_at > now() - interval '24 hours'
          ), 0) as ocr_latency_ms_p95,
          coalesce((
            select count(*)::bigint
              from ai_artifacts
             where stage = 'metadata'
               and duration_ms is not null
               and created_at > now() - interval '24 hours'
          ), 0) as metadata_latency_ms_count,
          coalesce((
            select (percentile_disc(0.95) within group (order by duration_ms))::bigint
              from ai_artifacts
             where stage = 'metadata'
               and duration_ms is not null
               and created_at > now() - interval '24 hours'
          ), 0) as metadata_latency_ms_p95,
          -- Oldest backlog age: how long the earliest queued job has been
          -- waiting (now() - min(run_after) over status='queued'). Null when
          -- the queue is empty, coalesced to 0.
          coalesce((
            select extract(epoch from (now() - min(run_after)))::bigint
              from jobs
             where status = 'queued'
          ), 0) as oldest_queued_age_seconds
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(MetricsSnapshot {
        jobs_queued: row.try_get("jobs_queued")?,
        jobs_running: row.try_get("jobs_running")?,
        jobs_failed: row.try_get("jobs_failed")?,
        jobs_succeeded: row.try_get("jobs_succeeded")?,
        reviews_pending: row.try_get("reviews_pending")?,
        runs_active: row.try_get("runs_active")?,
        audit_events: row.try_get("audit_events")?,
        selector_runs_total: row.try_get("selector_runs_total")?,
        selector_documents_queued_total: row.try_get("selector_documents_queued_total")?,
        job_retries_scheduled_total: row.try_get("job_retries_scheduled_total")?,
        model_errors_total: row.try_get("model_errors_total")?,
        apply_success_total: row.try_get("apply_success_total")?,
        apply_failure_total: row.try_get("apply_failure_total")?,
        apply_latency_ms_count: row.try_get("apply_latency_ms_count")?,
        apply_latency_ms_sum: row.try_get("apply_latency_ms_sum")?,
        apply_latency_ms_p95: row.try_get("apply_latency_ms_p95")?,
        ocr_latency_ms_count: row.try_get("ocr_latency_ms_count")?,
        ocr_latency_ms_p95: row.try_get("ocr_latency_ms_p95")?,
        metadata_latency_ms_count: row.try_get("metadata_latency_ms_count")?,
        metadata_latency_ms_p95: row.try_get("metadata_latency_ms_p95")?,
        oldest_queued_age_seconds: row.try_get("oldest_queued_age_seconds")?,
    })
}

pub async fn recovery_candidates(
    pool: &DbPool,
    older_than_seconds: i64,
) -> Result<Vec<RecoveryCandidate>> {
    let rows = sqlx::query(
        r#"
        select j.run_id,
               j.id as job_id,
               j.paperless_document_id,
               j.stage,
               j.status,
               j.lease_owner,
               j.lease_until,
               j.updated_at,
               'stale_lease' as reason
          from jobs j
         where j.status = 'running'
           and j.lease_until < now() - make_interval(secs => $1)
        union all
        select r.id as run_id,
               null::uuid as job_id,
               r.paperless_document_id,
               null::text as stage,
               r.status,
               null::text as lease_owner,
               null::timestamptz as lease_until,
               r.updated_at,
               'stuck_run_without_active_jobs' as reason
          from pipeline_runs r
         where r.status in ('queued', 'running', 'applying')
           and r.updated_at < now() - make_interval(secs => $1)
           and not exists (
             select 1
               from jobs j
              where j.run_id = r.id
                and j.status in ('queued', 'running', 'waiting_review')
           )
         order by updated_at asc
         limit 100
        "#,
    )
    .bind(older_than_seconds as f64)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let stage: Option<String> = row.try_get("stage")?;
            Ok(RecoveryCandidate {
                run_id: row.try_get("run_id")?,
                job_id: row.try_get("job_id")?,
                paperless_document_id: row.try_get("paperless_document_id")?,
                stage: stage.map(|stage| stage.parse()).transpose()?,
                status: row.try_get("status")?,
                lease_owner: row.try_get("lease_owner")?,
                lease_until: row.try_get("lease_until")?,
                updated_at: row.try_get("updated_at")?,
                reason: row.try_get("reason")?,
            })
        })
        .collect()
}

pub async fn recover_stale_leases(
    pool: &DbPool,
    older_than_seconds: i64,
    actor_id: Uuid,
) -> Result<RecoverySummary> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        r#"
        with stale as (
          select id, run_id, paperless_document_id
            from jobs
           where status = 'running'
             and lease_until < now() - make_interval(secs => $1)
           for update
        )
        update jobs j
           set status = 'queued',
               lease_owner = null,
               lease_until = null,
               run_after = now(),
               updated_at = now()
          from stale
         where j.id = stale.id
        returning j.id, j.run_id, j.paperless_document_id
        "#,
    )
    .bind(older_than_seconds as f64)
    .fetch_all(&mut *tx)
    .await?;

    let run_ids = rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("run_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let document_ids = rows
        .iter()
        .map(|row| row.try_get::<i32, _>("paperless_document_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let job_ids = rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if !run_ids.is_empty() {
        sqlx::query(
            r#"
            update pipeline_runs
               set status = 'queued',
                   updated_at = now()
             where id = any($1)
               and status = 'running'
            "#,
        )
        .bind(&run_ids)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'queued',
                   updated_at = now()
             where paperless_document_id = any($1)
               and current_run_status = 'running'
            "#,
        )
        .bind(&document_ids)
        .execute(&mut *tx)
        .await?;
    }

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "operations.stale_leases_requeued".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "count": job_ids.len(), "job_ids": job_ids })),
            metadata: Some(json!({ "older_than_seconds": older_than_seconds })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(RecoverySummary {
        stale_leases_requeued: job_ids.len() as i64,
        stuck_runs_failed: 0,
        stuck_runs_completed: 0,
    })
}

pub async fn recover_stuck_runs(
    pool: &DbPool,
    older_than_seconds: i64,
    actor_id: Uuid,
) -> Result<RecoverySummary> {
    let mut tx = pool.begin().await?;
    let completed = sqlx::query(
        r#"
        with stuck as (
          select r.id, r.paperless_document_id
            from pipeline_runs r
           where r.status in ('queued', 'running', 'applying')
             and r.updated_at < now() - make_interval(secs => $1)
             and exists (select 1 from jobs j where j.run_id = r.id)
             and not exists (
               select 1
                 from jobs j
                where j.run_id = r.id
                  and j.status <> 'succeeded'
             )
           for update
        )
        update pipeline_runs r
           set status = 'succeeded',
               finished_at = coalesce(finished_at, now()),
               updated_at = now()
          from stuck
         where r.id = stuck.id
        returning r.id, r.paperless_document_id
        "#,
    )
    .bind(older_than_seconds as f64)
    .fetch_all(&mut *tx)
    .await?;

    let failed = sqlx::query(
        r#"
        with stuck as (
          select r.id, r.paperless_document_id
            from pipeline_runs r
           where r.status in ('queued', 'running', 'applying')
             and r.updated_at < now() - make_interval(secs => $1)
             and not exists (
               select 1
                 from jobs j
                where j.run_id = r.id
                  and j.status in ('queued', 'running', 'waiting_review')
             )
           for update
        )
        update pipeline_runs r
           set status = 'failed',
               error_message = 'Recovered stuck run with no active jobs',
               finished_at = coalesce(finished_at, now()),
               updated_at = now()
          from stuck
         where r.id = stuck.id
        returning r.id, r.paperless_document_id
        "#,
    )
    .bind(older_than_seconds as f64)
    .fetch_all(&mut *tx)
    .await?;

    let completed_document_ids = completed
        .iter()
        .map(|row| row.try_get::<i32, _>("paperless_document_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let failed_document_ids = failed
        .iter()
        .map(|row| row.try_get::<i32, _>("paperless_document_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let completed_run_ids = completed
        .iter()
        .map(|row| row.try_get::<Uuid, _>("id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let failed_run_ids = failed
        .iter()
        .map(|row| row.try_get::<Uuid, _>("id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if !completed_document_ids.is_empty() {
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'succeeded',
                   complete = true,
                   updated_at = now()
             where paperless_document_id = any($1)
            "#,
        )
        .bind(&completed_document_ids)
        .execute(&mut *tx)
        .await?;
    }
    if !failed_document_ids.is_empty() {
        sqlx::query(
            r#"
            update document_inventory
               set current_run_status = 'failed',
                   last_error = 'Recovered stuck run with no active jobs',
                   updated_at = now()
             where paperless_document_id = any($1)
            "#,
        )
        .bind(&failed_document_ids)
        .execute(&mut *tx)
        .await?;
    }

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "operations.stuck_runs_recovered".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "completed": completed_run_ids.len(),
                "failed": failed_run_ids.len(),
                "completed_run_ids": completed_run_ids,
                "failed_run_ids": failed_run_ids
            })),
            metadata: Some(json!({ "older_than_seconds": older_than_seconds })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(RecoverySummary {
        stale_leases_requeued: 0,
        stuck_runs_failed: failed_document_ids.len() as i64,
        stuck_runs_completed: completed_document_ids.len() as i64,
    })
}

pub async fn append_audit(pool: &DbPool, event: AuditEventInput) -> Result<()> {
    let mut tx = pool.begin().await?;
    append_audit_tx(&mut tx, event).await?;
    tx.commit().await?;
    Ok(())
}

async fn append_audit_tx(
    tx: &mut Transaction<'_, Postgres>,
    mut event: AuditEventInput,
) -> Result<()> {
    if let Some(value) = &mut event.before {
        redact_sensitive_json(value);
    }
    if let Some(value) = &mut event.after {
        redact_sensitive_json(value);
    }
    if let Some(value) = &mut event.metadata {
        redact_sensitive_json(value);
    }

    sqlx::query("select pg_advisory_xact_lock(hashtext('paperless_archivist_audit_events'))")
        .execute(&mut **tx)
        .await?;
    // Order by chain_position (a sequence assigned under this same advisory
    // lock), not created_at: the writing process's wall clock is unreliable
    // across pods, but chain_position monotonically follows append order. #254.
    let prev_event_hash: Option<String> = sqlx::query(
        r#"
        select event_hash
          from audit_events
         where event_hash is not null
         order by chain_position desc
         limit 1
        "#,
    )
    .fetch_optional(&mut **tx)
    .await?
    .map(|row| row.try_get("event_hash"))
    .transpose()?;
    let id = Uuid::now_v7();
    let created_at = Utc::now();
    let event_hash = audit_event_hash(id, created_at, &prev_event_hash, &event);

    sqlx::query(
        r#"
        insert into audit_events (
          id, run_id, job_id, paperless_document_id, event_type, actor_type, actor_id,
          source_ip, user_agent,
          before, after, metadata, outcome, error_message, prev_event_hash, event_hash, created_at
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
        "#,
    )
    .bind(id)
    .bind(event.run_id)
    .bind(event.job_id)
    .bind(event.paperless_document_id)
    .bind(&event.event_type)
    .bind(&event.actor_type)
    .bind(&event.actor_id)
    .bind(&event.source_ip)
    .bind(&event.user_agent)
    .bind(&event.before)
    .bind(&event.after)
    .bind(&event.metadata)
    .bind(&event.outcome)
    .bind(&event.error_message)
    .bind(&prev_event_hash)
    .bind(&event_hash)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn audit_event_hash(
    id: Uuid,
    created_at: DateTime<Utc>,
    prev_event_hash: &Option<String>,
    event: &AuditEventInput,
) -> String {
    let canonical = json!({
        "id": id,
        "created_at": created_at,
        "prev_event_hash": prev_event_hash,
        "run_id": event.run_id,
        "job_id": event.job_id,
        "paperless_document_id": event.paperless_document_id,
        "event_type": &event.event_type,
        "actor_type": &event.actor_type,
        "actor_id": &event.actor_id,
        "before": &event.before,
        "after": &event.after,
        "metadata": &event.metadata,
        "outcome": &event.outcome,
        "error_message": &event.error_message,
    });
    short_hash(&canonical.to_string())
}

pub async fn list_audit_events(pool: &DbPool, limit: i64) -> Result<Vec<AuditEventRecord>> {
    let rows = sqlx::query(
        r#"
        select id, event_type, actor_type, actor_id, paperless_document_id,
               outcome, error_message, created_at, metadata, prev_event_hash, event_hash
          from audit_events
         order by created_at desc
         limit $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(AuditEventRecord {
                id: row.try_get("id")?,
                event_type: row.try_get("event_type")?,
                actor_type: row.try_get("actor_type")?,
                actor_id: row.try_get("actor_id")?,
                paperless_document_id: row.try_get("paperless_document_id")?,
                outcome: row.try_get("outcome")?,
                error_message: row.try_get("error_message")?,
                created_at: row.try_get("created_at")?,
                metadata: row.try_get("metadata")?,
                prev_event_hash: row.try_get("prev_event_hash")?,
                event_hash: row.try_get("event_hash")?,
            })
        })
        .collect()
}

pub async fn verify_audit_integrity(pool: &DbPool) -> Result<AuditIntegrityReport> {
    use futures::TryStreamExt;

    let legacy_events: i64 =
        sqlx::query("select count(*)::bigint as count from audit_events where event_hash is null")
            .fetch_one(pool)
            .await?
            .try_get("count")?;

    // Stream the audit chain instead of loading the entire table into memory.
    // Verification is intrinsically streamable: each row's prev_event_hash
    // must match the previous row's event_hash, so we only carry one cursor
    // value forward.
    let mut stream = sqlx::query(
        r#"
        select id, run_id, job_id, paperless_document_id, event_type, actor_type, actor_id,
               before, after, metadata, outcome, error_message, created_at,
               prev_event_hash, event_hash
          from audit_events
         where event_hash is not null
         order by chain_position asc
        "#,
    )
    .fetch(pool);

    let mut checked_events = 0_i64;
    let mut latest_event_hash: Option<String> = None;
    while let Some(row) = stream.try_next().await? {
        let id: Uuid = row.try_get("id")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let prev_event_hash: Option<String> = row.try_get("prev_event_hash")?;
        let event_hash: String = row.try_get("event_hash")?;
        if let Some(expected_prev) = &latest_event_hash
            && prev_event_hash.as_ref() != Some(expected_prev)
        {
            return Ok(AuditIntegrityReport {
                ok: false,
                checked_events,
                legacy_events,
                latest_event_hash,
                broken_event_id: Some(id),
                broken_reason: Some("previous event hash does not match chain".to_owned()),
            });
        }
        let event = AuditEventInput {
            run_id: row.try_get("run_id")?,
            job_id: row.try_get("job_id")?,
            paperless_document_id: row.try_get("paperless_document_id")?,
            event_type: row.try_get("event_type")?,
            actor_type: row.try_get("actor_type")?,
            actor_id: row.try_get("actor_id")?,
            before: row.try_get("before")?,
            after: row.try_get("after")?,
            metadata: row.try_get("metadata")?,
            outcome: row.try_get("outcome")?,
            error_message: row.try_get("error_message")?,
            // source_ip / user_agent are persisted but not part of the
            // audit hash chain; leave None when reconstructing for verify.
            source_ip: None,
            user_agent: None,
        };
        let expected_hash = audit_event_hash(id, created_at, &prev_event_hash, &event);
        if expected_hash != event_hash {
            return Ok(AuditIntegrityReport {
                ok: false,
                checked_events,
                legacy_events,
                latest_event_hash,
                broken_event_id: Some(id),
                broken_reason: Some("event hash does not match event payload".to_owned()),
            });
        }
        checked_events += 1;
        latest_event_hash = Some(event_hash);
    }

    Ok(AuditIntegrityReport {
        ok: true,
        checked_events,
        legacy_events,
        latest_event_hash,
        broken_event_id: None,
        broken_reason: None,
    })
}

pub async fn apply_security_retention(
    pool: &DbPool,
    settings: &RuntimeSettings,
    actor_id: Uuid,
) -> Result<RetentionResult> {
    let security = settings.clone().normalized().security;
    let now = Utc::now();
    let artifact_cutoff = now - ChronoDuration::days(security.ai_artifact_retention_days);
    let audit_cutoff = now - ChronoDuration::days(security.audit_retention_days);

    // ocr_page_cache holds the full OCR text of every processed page and must
    // not outlive the artifact retention. Deleted in bounded batches outside
    // the audit transaction so a years-old backlog can't hold one giant lock.
    let mut ocr_page_cache_deleted: i64 = 0;
    loop {
        let deleted = sqlx::query(
            r#"
            delete from ocr_page_cache
             where ctid in (
               select ctid from ocr_page_cache
                where created_at < $1
                limit 5000
             )
            "#,
        )
        .bind(artifact_cutoff)
        .execute(pool)
        .await?
        .rows_affected() as i64;
        ocr_page_cache_deleted += deleted;
        if deleted < 5000 {
            break;
        }
    }

    // dashboard_snapshots accumulate one row every ~5 minutes forever (#273).
    // Prune to the audit retention window in bounded batches, like the OCR
    // cache above.
    let mut dashboard_snapshots_deleted: i64 = 0;
    loop {
        let deleted = sqlx::query(
            r#"
            delete from dashboard_snapshots
             where ctid in (
               select ctid from dashboard_snapshots
                where captured_at < $1
                limit 5000
             )
            "#,
        )
        .bind(audit_cutoff)
        .execute(pool)
        .await?
        .rows_affected() as i64;
        dashboard_snapshots_deleted += deleted;
        if deleted < 5000 {
            break;
        }
    }

    // Batch the artifact/audit deletes too (#275): a single unbounded DELETE
    // holds a long lock and bloats one transaction on a large backlog. The
    // audit chain tolerates a truncated prefix, so deleting the old rows
    // before appending the retention event keeps the chain consistent.
    let mut ai_artifacts_deleted: i64 = 0;
    loop {
        let deleted = sqlx::query(
            r#"
            delete from ai_artifacts
             where ctid in (
               select ctid from ai_artifacts where created_at < $1 limit 5000
             )
            "#,
        )
        .bind(artifact_cutoff)
        .execute(pool)
        .await?
        .rows_affected() as i64;
        ai_artifacts_deleted += deleted;
        if deleted < 5000 {
            break;
        }
    }
    // Delete a true chain_position PREFIX, not a created_at prefix (#285). The
    // chain verifies in chain_position order (#254), so deleting by created_at
    // under cross-pod clock skew could remove a lower-chain_position row while
    // keeping a higher one, leaving a hole that verify reports as a broken
    // chain. Compute the smallest chain_position we must keep (the oldest row
    // still inside the retention window) and delete everything strictly below
    // it; if nothing is inside the window the whole table is expired.
    let keep_boundary: i64 = sqlx::query_scalar(
        "select coalesce(min(chain_position), $2) from audit_events where created_at >= $1",
    )
    .bind(audit_cutoff)
    .bind(i64::MAX)
    .fetch_one(pool)
    .await?;
    let mut audit_events_deleted: i64 = 0;
    loop {
        let deleted = sqlx::query(
            r#"
            delete from audit_events
             where ctid in (
               select ctid from audit_events where chain_position < $1 limit 5000
             )
            "#,
        )
        .bind(keep_boundary)
        .execute(pool)
        .await?
        .rows_affected() as i64;
        audit_events_deleted += deleted;
        if deleted < 5000 {
            break;
        }
    }

    let mut tx = pool.begin().await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "audit.retention_applied".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({
                "audit_retention_days": security.audit_retention_days,
                "ai_artifact_retention_days": security.ai_artifact_retention_days,
                "audit_events_deleted": audit_events_deleted,
                "ai_artifacts_deleted": ai_artifacts_deleted,
                "ocr_page_cache_deleted": ocr_page_cache_deleted,
                "dashboard_snapshots_deleted": dashboard_snapshots_deleted
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;

    Ok(RetentionResult {
        audit_events_deleted,
        ai_artifacts_deleted,
        ocr_page_cache_deleted,
    })
}

async fn no_remaining_jobs_tx(tx: &mut Transaction<'_, Postgres>, run_id: Uuid) -> Result<bool> {
    let row = sqlx::query(
        r#"
        select not exists(
          select 1 from jobs
           where run_id = $1
             and status in ('queued', 'running', 'waiting_review')
        ) as done
        "#,
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await?;
    row.try_get("done").context("read run completion state")
}

async fn set_inventory_stage_status_tx(
    tx: &mut Transaction<'_, Postgres>,
    paperless_document_id: i32,
    stage: Stage,
    status: &str,
    error: Option<&str>,
    needs_review: bool,
    run_id: Option<Uuid>,
) -> Result<()> {
    let column = status_column_for_stage(stage)?;
    let sql = format!(
        r#"
        update document_inventory
           set {column} = $2,
               current_run_status = case when $2 = 'failed' then 'failed' else current_run_status end,
               last_error = $3,
               needs_review = $4,
               last_run_id = coalesce($5, last_run_id),
               updated_at = now()
         where paperless_document_id = $1
        "#
    );
    // SAFETY: `sql` is a static literal built above with no user-controlled
    // interpolation; only bind parameters carry caller data.
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(paperless_document_id)
        .bind(status)
        .bind(error)
        .bind(needs_review)
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Thin wrapper around [`Stage::inventory_status_column`] that surfaces a typed
/// error when a caller passes an orchestration-only stage. The returned string is
/// a static literal — callers may safely interpolate it into SQL.
fn status_column_for_stage(stage: Stage) -> Result<&'static str> {
    stage
        .inventory_status_column()
        .ok_or_else(|| anyhow!("stage does not map to inventory status: {stage}"))
}

/// SQL `ilike` patterns matching the vision-runtime-crash error-message
/// signatures (`GGML_ASSERT(...)`, "runner process no longer running", "signal
/// arrived during cgo execution"). Kept in sync with
/// `archivist_worker::is_vision_model_runtime_crash`.
pub const VISION_CRASH_SQL_PATTERNS: &[&str] = &[
    "%GGML_ASSERT%",
    "%runner process no longer running%",
    "%signal arrived during cgo execution%",
];

/// Summary of a one-shot startup requeue pass. Helpful for log lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisionCrashRequeueSummary {
    pub jobs_requeued: i64,
}

/// One-shot, idempotent helper run on worker startup that lifts `failed` OCR-stage jobs
/// whose error message matches the vision-runtime-crash signature back into the queue so
/// they get one more attempt under the new fallback machinery. We bump `max_attempts` by
/// one (rather than resetting `attempts`) so a job that has already burned through its
/// retry budget on the broken primary model still has one fresh attempt to run under the
/// fallback, but does not get an unbounded budget.
///
/// Also flips the matching `pipeline_runs` row back to `queued`, and resets the
/// inventory stage status, so the dashboard reflects the second chance.
///
/// All writes happen in a single transaction; either all matching rows are requeued or
/// none of them are. Returns the number of jobs that were lifted.
pub async fn requeue_vision_crashed_jobs(pool: &DbPool) -> Result<VisionCrashRequeueSummary> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        r#"
        with crashed as (
          select id, run_id, paperless_document_id, max_attempts
            from jobs
           where status = 'failed'
             and stage = 'ocr'
             and (
                  error_message ilike $1
               or error_message ilike $2
               or error_message ilike $3
             )
           for update
        )
        update jobs j
           set status = 'queued',
               max_attempts = j.max_attempts + 1,
               run_after = now(),
               lease_owner = null,
               lease_until = null,
               error_message = null,
               updated_at = now()
          from crashed
         where j.id = crashed.id
        returning j.id, j.run_id, j.paperless_document_id
        "#,
    )
    .bind(VISION_CRASH_SQL_PATTERNS[0])
    .bind(VISION_CRASH_SQL_PATTERNS[1])
    .bind(VISION_CRASH_SQL_PATTERNS[2])
    .fetch_all(&mut *tx)
    .await?;

    if rows.is_empty() {
        tx.commit().await?;
        return Ok(VisionCrashRequeueSummary { jobs_requeued: 0 });
    }

    let run_ids: Vec<Uuid> = rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("run_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let document_ids: Vec<i32> = rows
        .iter()
        .map(|row| row.try_get::<i32, _>("paperless_document_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let job_ids: Vec<Uuid> = rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    sqlx::query(
        r#"
        update pipeline_runs
           set status = 'queued',
               error_message = null,
               finished_at = null,
               updated_at = now()
         where id = any($1)
           and status = 'failed'
        "#,
    )
    .bind(&run_ids)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        update document_inventory
           set ocr_status = 'queued',
               current_run_status = 'queued',
               updated_at = now()
         where paperless_document_id = any($1)
           and ocr_status = 'failed'
        "#,
    )
    .bind(&document_ids)
    .execute(&mut *tx)
    .await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "worker.vision_crash_jobs_requeued".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "count": job_ids.len(),
                "job_ids": job_ids,
            })),
            metadata: Some(json!({
                "trigger": "startup_one_shot",
                "patterns": VISION_CRASH_SQL_PATTERNS,
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(VisionCrashRequeueSummary {
        jobs_requeued: job_ids.len() as i64,
    })
}

/// Summary of a one-shot metadata-stage backfill pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataStageBackfillSummary {
    pub runs_updated: i64,
    pub jobs_inserted: i64,
}

/// One-shot, idempotent helper run on worker startup that lifts the
/// historical OCR-only `pipeline_runs` (created before v1.5.4 by trigger
/// polling against documents tagged only with the OCR trigger) up to include
/// the consolidated `metadata` stage as well. Without this, those runs
/// terminate after OCR with no Title/Correspondent/Tags suggestion ever
/// being produced, so the Review queue is full of `{"content": "..."}`-only
/// review items that the operator cannot meaningfully act on.
///
/// What this does, in one transaction:
///   * Find every `pipeline_runs` whose `stages` jsonb array contains "ocr"
///     but does NOT contain "metadata", AND does not already have a
///     `metadata`-stage `jobs` row.
///   * Append "metadata" to `pipeline_runs.stages`.
///   * Insert a queued `metadata` job for the run with `stage_priority=20`
///     so it sequences AFTER the OCR job (priority 10).
///   * For runs that were already `succeeded` (OCR is done and either auto-
///     applied or never produced a review): flip status back to `queued`
///     and clear `finished_at`, so the worker re-picks the run up to claim
///     the new metadata job. For runs in `waiting_review`/`queued`/`running`,
///     status is left alone — the natural cascade in `mark_review_auto_applied`
///     / OCR completion will flip the run back to `queued` once the OCR side
///     settles, and the metadata job becomes claimable from the existing
///     run-still-has-work path.
///
/// Idempotent: re-running this finds nothing to do because of the
/// `NOT EXISTS metadata job` predicate. Safe to run on every worker startup.
pub async fn backfill_metadata_stage_for_ocr_only_runs(
    pool: &DbPool,
) -> Result<MetadataStageBackfillSummary> {
    let mut tx = pool.begin().await?;

    // Step 1: identify the target runs. Lock them for update so a parallel
    // worker doesn't race us into queueing duplicate metadata jobs.
    let target_rows = sqlx::query(
        r#"
        select pr.id as run_id,
               pr.paperless_document_id,
               pr.status as current_status
          from pipeline_runs pr
         where pr.stages @> '["ocr"]'::jsonb
           and not (pr.stages @> '["metadata"]'::jsonb)
           and not exists (
             select 1 from jobs j
              where j.run_id = pr.id and j.stage = 'metadata'
           )
         for update of pr skip locked
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;

    if target_rows.is_empty() {
        tx.commit().await?;
        return Ok(MetadataStageBackfillSummary::default());
    }

    let run_ids: Vec<Uuid> = target_rows
        .iter()
        .map(|row| row.try_get::<Uuid, _>("run_id"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // Step 2: append "metadata" to the stages array and reset succeeded
    // runs back to queued so the worker re-claims the new metadata job.
    sqlx::query(
        r#"
        update pipeline_runs
           set stages = (
                 select coalesce(jsonb_agg(s order by s_order), '[]'::jsonb)
                   from (
                     select 'ocr'      as s, 1 as s_order
                     union all
                     select 'metadata' as s, 2 as s_order
                   ) ordered
               ),
               status = case when status = 'succeeded' then 'queued' else status end,
               finished_at = case when status = 'succeeded' then null else finished_at end,
               updated_at = now()
         where id = any($1)
        "#,
    )
    .bind(&run_ids)
    .execute(&mut *tx)
    .await?;

    // Step 3: insert a queued metadata job per run, with stage_priority=20
    // so it claims AFTER the OCR job (stage_priority=10). The cross-run
    // `priority` is INHERITED from the same run's OCR job — this is the
    // v1.5.6 fix for the v1.5.4 backfill bug where metadata jobs were
    // priced with `1_000_000 - document_id` (~993K-999K) while legacy
    // trigger-polling OCR jobs sit at priority=10. Since claim_jobs orders
    // by priority ASC then stage_priority ASC, mispriced metadata never
    // claimed until every OCR job globally was done. Inheriting the
    // sibling OCR's priority keeps the cross-run ordering exactly as the
    // operator who queued the run intended, and the stage_priority=20
    // alone guarantees OCR-before-metadata ordering within the run.
    let inserted = sqlx::query(
        r#"
        insert into jobs (run_id, paperless_document_id, stage, status, payload)
        select pr.id,
               pr.paperless_document_id,
               'metadata',
               'queued',
               jsonb_build_object(
                 'priority', coalesce(
                   (ocr.payload ->> 'priority')::bigint,
                   100
                 ),
                 'stage_priority', 20,
                 'backfill', true
               )
          from pipeline_runs pr
          join jobs ocr on ocr.run_id = pr.id and ocr.stage = 'ocr'
         where pr.id = any($1)
           and not exists (
             select 1 from jobs m
              where m.run_id = pr.id and m.stage = 'metadata'
           )
        "#,
    )
    .bind(&run_ids)
    .execute(&mut *tx)
    .await?;
    let jobs_inserted = inserted.rows_affected() as i64;

    // Step 4: nudge document_inventory.current_run_status for the formerly-
    // succeeded runs that just got flipped back to queued, so the dashboard
    // status badges match the new pipeline_runs state.
    sqlx::query(
        r#"
        update document_inventory di
           set current_run_status = 'queued',
               complete = false,
               updated_at = now()
          from pipeline_runs pr
         where pr.id = any($1)
           and pr.id = di.last_run_id
           and pr.status = 'queued'
        "#,
    )
    .bind(&run_ids)
    .execute(&mut *tx)
    .await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "worker.metadata_stage_backfilled".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "runs_updated": run_ids.len(),
                "jobs_inserted": jobs_inserted,
            })),
            metadata: Some(json!({
                "trigger": "startup_one_shot",
                "reason": "ocr_only_runs_missing_metadata_stage",
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;

    Ok(MetadataStageBackfillSummary {
        runs_updated: run_ids.len() as i64,
        jobs_inserted,
    })
}

/// Summary of a one-shot bump pass for the vision `num_ctx` runtime setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisionNumCtxBumpSummary {
    pub previous: Option<i64>,
    pub current: i64,
    pub bumped: bool,
}

/// One-shot, idempotent helper that raises `ai.ollama_vision_num_ctx` from
/// any value <= 16384 to 32768. v1.5.1 fixed glm-ocr GGML_ASSERT crashes by
/// pinning num_ctx=16384 on Ollama vision calls — that floor worked for
/// single-page renders but is still too small for some multi-page or
/// high-DPI documents under realistic prod load (137 OCR jobs burned through
/// their retry budget despite num_ctx=16384). Doubling to 32768 gives the
/// vision model headroom for the page-token blow-up cases without forcing
/// operators to dig through Settings to find the dial. Operators who have
/// already set a higher value get left alone.
pub async fn bump_vision_num_ctx_if_too_small(pool: &DbPool) -> Result<VisionNumCtxBumpSummary> {
    const FLOOR: i64 = 32768;
    const PREVIOUS_FIX_CEILING: i64 = 16384;

    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        select (value #>> '{ai,ollama_vision_num_ctx}')::bigint as num_ctx
          from settings
         where key = 'runtime'
         for update
        "#,
    )
    .fetch_optional(&mut *tx)
    .await?;

    let previous: Option<i64> = row.and_then(|r| r.try_get("num_ctx").ok());

    // Only bump if the current value is <= the v1.5.1 fix ceiling. Don't
    // touch operator overrides that already raised it past 16384.
    let should_bump = match previous {
        Some(v) => v <= PREVIOUS_FIX_CEILING,
        None => true,
    };

    if !should_bump {
        tx.commit().await?;
        return Ok(VisionNumCtxBumpSummary {
            previous,
            current: previous.unwrap_or(FLOOR),
            bumped: false,
        });
    }

    sqlx::query(
        r#"
        update settings
           set value = jsonb_set(
                 value,
                 '{ai,ollama_vision_num_ctx}',
                 to_jsonb($1::bigint)
               ),
               updated_at = now()
         where key = 'runtime'
        "#,
    )
    .bind(FLOOR)
    .execute(&mut *tx)
    .await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "worker.vision_num_ctx_bumped".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: previous.map(|v| json!({ "ollama_vision_num_ctx": v })),
            after: Some(json!({ "ollama_vision_num_ctx": FLOOR })),
            metadata: Some(json!({
                "trigger": "startup_one_shot",
                "reason": "ggml_assert_recurrence_at_16384",
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(VisionNumCtxBumpSummary {
        previous,
        current: FLOOR,
        bumped: true,
    })
}

/// Summary of a one-shot bump pass for the text `num_ctx` runtime setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextNumCtxBumpSummary {
    pub previous: Option<i64>,
    pub current: i64,
    pub bumped: bool,
}

/// One-shot, idempotent helper that raises `ai.ollama_text_num_ctx` to a 16384
/// floor. The text default was 8192 with no floor (the vision path got one in
/// v1.5.1, the text path never did), so a large metadata prompt — the OCR text
/// plus the candidate correspondent/type/tag lists — could overflow the 8192
/// window and fail every metadata job permanently with
/// `exceed_context_size_error`. 16384 gives headroom for a bounded prompt;
/// operators who already raised it past the floor are left alone.
pub async fn bump_text_num_ctx_if_too_small(pool: &DbPool) -> Result<TextNumCtxBumpSummary> {
    const FLOOR: i64 = 16384;

    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        select (value #>> '{ai,ollama_text_num_ctx}')::bigint as num_ctx
          from settings
         where key = 'runtime'
         for update
        "#,
    )
    .fetch_optional(&mut *tx)
    .await?;

    let previous: Option<i64> = row.and_then(|r| r.try_get("num_ctx").ok());
    let should_bump = match previous {
        Some(v) => v < FLOOR,
        None => true,
    };

    if !should_bump {
        tx.commit().await?;
        return Ok(TextNumCtxBumpSummary {
            previous,
            current: previous.unwrap_or(FLOOR),
            bumped: false,
        });
    }

    sqlx::query(
        r#"
        update settings
           set value = jsonb_set(value, '{ai,ollama_text_num_ctx}', to_jsonb($1::bigint)),
               updated_at = now()
         where key = 'runtime'
        "#,
    )
    .bind(FLOOR)
    .execute(&mut *tx)
    .await?;

    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "worker.text_num_ctx_bumped".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: previous.map(|v| json!({ "ollama_text_num_ctx": v })),
            after: Some(json!({ "ollama_text_num_ctx": FLOOR })),
            metadata: Some(json!({
                "trigger": "startup_one_shot",
                "reason": "text_context_overflow_at_8192",
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    tx.commit().await?;
    Ok(TextNumCtxBumpSummary {
        previous,
        current: FLOOR,
        bumped: true,
    })
}

/// Summary of a one-shot pass that resets stuck `pipeline_runs.status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StuckRunStatusFixSummary {
    pub runs_reset: i64,
}

/// Recover review items stranded in `applying`: a worker or API request that
/// crashed between claiming a row (for a human apply or autopilot drain) and
/// recording the terminal status leaves the row owned-but-never-finished, and
/// neither the drain (lists `pending`) nor the operator (lists
/// `approved`/`edited`) would ever pick it up again. A row in `applying`
/// older than `older_than_seconds` is reverted to `pending` so the next drain
/// tick (or a fresh operator apply after re-approval) retries it. The Paperless
/// PATCH is idempotent on redo (it overwrites the same fields), so re-applying
/// is safe. Returns the number of rows recovered. #253.
pub async fn reset_stale_applying_reviews(pool: &DbPool, older_than_seconds: i64) -> Result<i64> {
    let reset = sqlx::query(
        r#"
        update review_items
           set status = 'pending',
               reviewed_at = null
         where status = 'applying'
           and reviewed_at < now() - make_interval(secs => $1)
        "#,
    )
    .bind(older_than_seconds as f64)
    .execute(pool)
    .await?
    .rows_affected() as i64;
    Ok(reset)
}

/// One-shot, idempotent helper that fixes `pipeline_runs.status='running'`
/// rows whose underlying jobs are all in a settled state (no `running` job
/// for that run). v1.5.4+ left these behind because `complete_job` only
/// flipped the run to `succeeded` when ALL stages were done; intermediate
/// stage successes (e.g. OCR done with metadata still queued) silently
/// stayed on `running`, surfacing as "N stuck run(s)" on the dashboard.
///
/// v1.5.7 fixes the live path in `complete_job` so new runs don't get
/// trapped, AND this helper cleans up the historical residue. Targets:
/// runs with status='running' AND no jobs.status='running' for that run —
/// flip to 'queued' if any queued job exists, else 'succeeded'.
pub async fn reset_stuck_running_pipeline_runs(pool: &DbPool) -> Result<StuckRunStatusFixSummary> {
    let mut tx = pool.begin().await?;
    // Two phases: (a) flip to 'queued' if there's at least one queued job for
    // the run that has no blocking prior-stage job; (b) otherwise (all jobs
    // succeeded/failed/cancelled), flip to 'succeeded' and set finished_at.
    let to_queued = sqlx::query(
        r#"
        update pipeline_runs pr
           set status = 'queued', updated_at = now()
         where pr.status = 'running'
           and not exists (
             select 1 from jobs j
              where j.run_id = pr.id and j.status = 'running'
           )
           and exists (
             select 1 from jobs j
              where j.run_id = pr.id and j.status in ('queued', 'waiting_review')
           )
        "#,
    )
    .execute(&mut *tx)
    .await?;
    let to_succeeded = sqlx::query(
        r#"
        update pipeline_runs pr
           set status = 'succeeded', finished_at = now(), updated_at = now()
         where pr.status = 'running'
           and not exists (
             select 1 from jobs j
              where j.run_id = pr.id
                and j.status in ('queued', 'running', 'waiting_review', 'failed')
           )
        "#,
    )
    .execute(&mut *tx)
    .await?;
    let runs_reset = (to_queued.rows_affected() + to_succeeded.rows_affected()) as i64;

    if runs_reset > 0 {
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: "worker.stuck_running_runs_reset".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: None,
                after: Some(json!({
                    "to_queued": to_queued.rows_affected(),
                    "to_succeeded": to_succeeded.rows_affected(),
                })),
                metadata: Some(json!({
                    "trigger": "startup_one_shot",
                    "reason": "complete_job_status_bug_pre_v1.5.7",
                })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }

    tx.commit().await?;
    Ok(StuckRunStatusFixSummary { runs_reset })
}

/// Summary of a one-shot rebalance pass for backfilled metadata jobs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataPriorityRebalanceSummary {
    pub jobs_repriced: i64,
}

/// One-shot, idempotent fix for the v1.5.4 metadata-stage backfill bug.
///
/// The v1.5.4 backfill priced every new metadata job with
/// `payload.priority = 1_000_000 - paperless_document_id` (~993K–999K),
/// but the legacy trigger-polling OCR jobs sit at `payload.priority = 10`.
/// Since `claim_jobs` orders by `priority ASC` first, those metadata jobs
/// could not be claimed until every OCR job globally was succeeded — even
/// for runs whose own OCR was already done — which meant the backfilled
/// 5953 metadata jobs sat queued indefinitely behind the OCR backlog.
///
/// This helper finds every still-queued metadata job that has the
/// `payload.backfill = true` marker AND whose stored `payload.priority`
/// disagrees with the sibling OCR job's `payload.priority` for the same
/// `run_id`. It rewrites the metadata job's payload to inherit the OCR's
/// priority. Single transaction, idempotent — once every backfilled
/// metadata job's priority matches its OCR sibling, subsequent startups
/// find nothing to do.
pub async fn rebalance_backfilled_metadata_priorities(
    pool: &DbPool,
) -> Result<MetadataPriorityRebalanceSummary> {
    let mut tx = pool.begin().await?;
    let result = sqlx::query(
        r#"
        update jobs m
           set payload = jsonb_set(
                 m.payload,
                 '{priority}',
                 to_jsonb(coalesce((ocr.payload ->> 'priority')::bigint, 100))
               ),
               updated_at = now()
          from jobs ocr
         where m.stage = 'metadata'
           and m.status = 'queued'
           and (m.payload ->> 'backfill')::boolean = true
           and ocr.run_id = m.run_id
           and ocr.stage = 'ocr'
           and (m.payload ->> 'priority')::bigint
             is distinct from coalesce((ocr.payload ->> 'priority')::bigint, 100)
        "#,
    )
    .execute(&mut *tx)
    .await?;
    let jobs_repriced = result.rows_affected() as i64;

    if jobs_repriced > 0 {
        append_audit_tx(
            &mut tx,
            AuditEventInput {
                event_type: "worker.metadata_priority_rebalanced".to_owned(),
                actor_type: "worker".to_owned(),
                actor_id: None,
                run_id: None,
                job_id: None,
                paperless_document_id: None,
                before: None,
                after: Some(json!({ "jobs_repriced": jobs_repriced })),
                metadata: Some(json!({
                    "trigger": "startup_one_shot",
                    "reason": "v1.5.4_backfill_priority_bug",
                })),
                outcome: "success".to_owned(),
                error_message: None,
                source_ip: None,
                user_agent: None,
            },
        )
        .await?;
    }

    tx.commit().await?;
    Ok(MetadataPriorityRebalanceSummary { jobs_repriced })
}

// ---------------------------------------------------------------------------
// Metadata-trace diagnostic helpers (v1.5.21).
//
// Power the `GET /api/inventory/{document_id}/metadata-trace` endpoint: given a
// Paperless document id, fetch the most recent metadata-stage `pipeline_runs`
// row, the LLM `ai_artifacts` payload (if any), all `review_items` for the run,
// and the apply-time `audit_events` row (if any). The API layer composes these
// into a per-field outcome view.
//
// SQL quirk to remember: `ai_artifacts` has NO `paperless_document_id` column —
// the link to a document goes through `pipeline_runs.id` via `ai_artifacts.run_id`.
// The v1.5.14 → v1.5.19 regression was exactly this kind of "non-existent column
// resolved at runtime" bug; `tests/migration_smoke.rs` calls each helper below
// against the empty fresh-migration DB so the SQL parser catches it in CI.

/// Header for the most recent metadata-stage `pipeline_runs` row of a document.
/// Mirrors the fields the diagnostic UI needs without depending on the global
/// `pipeline_runs` row struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataRunHeader {
    pub run_id: Uuid,
    pub paperless_document_id: i32,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Most recent `ai_artifacts` row for a given run + `stage = 'metadata'`.
/// Carries model/provider for the run header and `normalized_output` so the
/// frontend can render the raw LLM suggestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataArtifact {
    pub id: Uuid,
    pub model: String,
    pub provider: String,
    pub normalized_output: Option<Value>,
    pub created_at: DateTime<Utc>,
}

/// One `review_items` row scoped to a metadata-stage run. Stage stays as a
/// string (not parsed `Stage`) because legacy in-flight v1.3 runs may still be
/// drained against legacy per-field stages and the diagnostic should not crash
/// on an unrecognised value — it just won't be matched against any of the six
/// metadata fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataReviewItem {
    pub id: Uuid,
    pub run_id: Uuid,
    pub stage: String,
    pub status: String,
    pub suggested_patch: Value,
    pub edited_patch: Option<Value>,
    pub validation_warnings: Value,
    pub created_at: DateTime<Utc>,
}

/// The most recent `audit_events` row carrying `event_type = 'document.patch_applied'`
/// for the run. Its `after` payload is the patch the worker pushed to Paperless,
/// keyed by `title` / `correspondent` (id) / `document_type` (id) / `created`
/// (document_date) / `tags` (Vec<i32>) / `custom_fields` (redacted summary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataApplyAudit {
    pub id: Uuid,
    pub run_id: Uuid,
    pub after: Option<Value>,
    pub outcome: String,
    pub created_at: DateTime<Utc>,
}

/// Most recent metadata-stage `pipeline_runs` row for a document.
///
/// "Metadata-stage" means: the run has a `jobs` row with `stage = 'metadata'`
/// or, for legacy v1.3.x runs queued before the consolidation, any of the
/// per-field stages (`title`, `correspondent`, `document_type`,
/// `document_date`, `tags`, `fields`). We use the broad set so the diagnostic
/// still works against in-flight legacy runs.
pub async fn latest_metadata_run_for_document(
    pool: &DbPool,
    paperless_document_id: i32,
) -> Result<Option<MetadataRunHeader>> {
    let row = sqlx::query(
        r#"
        select pr.id as run_id,
               pr.paperless_document_id as paperless_document_id,
               pr.status as status,
               pr.created_at as created_at,
               pr.finished_at as finished_at
          from pipeline_runs pr
         where pr.paperless_document_id = $1
           and exists (
             select 1
               from jobs j
              where j.run_id = pr.id
                and j.stage in (
                  'metadata',
                  'title',
                  'correspondent',
                  'document_type',
                  'document_date',
                  'tags',
                  'fields'
                )
           )
         order by pr.created_at desc
         limit 1
        "#,
    )
    .bind(paperless_document_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| -> Result<MetadataRunHeader> {
        Ok(MetadataRunHeader {
            run_id: row.try_get("run_id")?,
            paperless_document_id: row.try_get("paperless_document_id")?,
            status: row.try_get("status")?,
            created_at: row.try_get("created_at")?,
            finished_at: row.try_get("finished_at")?,
        })
    })
    .transpose()
}

/// Most recent `ai_artifacts` row for a metadata-stage run.
///
/// Returns `None` when the LLM call has not produced an artifact yet (e.g. the
/// run is still `queued` or failed before the metadata stage executed).
pub async fn latest_metadata_artifact_for_run(
    pool: &DbPool,
    run_id: Uuid,
) -> Result<Option<MetadataArtifact>> {
    let row = sqlx::query(
        r#"
        select id,
               model,
               provider,
               normalized_output,
               created_at
          from ai_artifacts
         where run_id = $1
           and stage = 'metadata'
         order by created_at desc
         limit 1
        "#,
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| -> Result<MetadataArtifact> {
        Ok(MetadataArtifact {
            id: row.try_get("id")?,
            model: row.try_get("model")?,
            provider: row.try_get("provider")?,
            normalized_output: row.try_get("normalized_output")?,
            created_at: row.try_get("created_at")?,
        })
    })
    .transpose()
}

/// All `review_items` rows for a metadata-stage run, ordered by creation time.
///
/// Returns an empty `Vec` for runs that produced no review items (full-auto
/// happy path) or for runs that have not reached the validation stage yet.
pub async fn metadata_review_items_for_run(
    pool: &DbPool,
    run_id: Uuid,
) -> Result<Vec<MetadataReviewItem>> {
    let rows = sqlx::query(
        r#"
        select id,
               run_id,
               stage,
               status,
               suggested_patch,
               edited_patch,
               validation_warnings,
               created_at
          from review_items
         where run_id = $1
         order by created_at asc
        "#,
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| -> Result<MetadataReviewItem> {
            Ok(MetadataReviewItem {
                id: row.try_get("id")?,
                run_id: row.try_get("run_id")?,
                stage: row.try_get("stage")?,
                status: row.try_get("status")?,
                suggested_patch: row.try_get("suggested_patch")?,
                edited_patch: row.try_get("edited_patch")?,
                validation_warnings: row.try_get("validation_warnings")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

/// Most recent `audit_events.document.patch_applied` row for the run.
///
/// Used to detect that the metadata patch made it all the way to Paperless and
/// to derive `latest_run.applied_at`. The `after` payload mirrors the
/// `DocumentPatch` shape the worker pushed (title/tags/correspondent/
/// document_type/created/custom_fields).
pub async fn latest_apply_audit_for_run(
    pool: &DbPool,
    run_id: Uuid,
) -> Result<Option<MetadataApplyAudit>> {
    let row = sqlx::query(
        r#"
        select id,
               run_id,
               after,
               outcome,
               created_at
          from audit_events
         where run_id = $1
           and event_type = 'document.patch_applied'
         order by created_at desc
         limit 1
        "#,
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| -> Result<MetadataApplyAudit> {
        // `audit_events.run_id` is nullable in the schema, but the row is
        // filtered by `run_id = $1` so the bind value is the canonical id.
        // Pulling it from `row.try_get` would force `Option<Uuid>` handling
        // for no real benefit.
        let _: Option<Uuid> = row.try_get("run_id")?;
        Ok(MetadataApplyAudit {
            id: row.try_get("id")?,
            run_id,
            after: row.try_get("after")?,
            outcome: row.try_get("outcome")?,
            created_at: row.try_get("created_at")?,
        })
    })
    .transpose()
}

// ---------------------------------------------------------------------------
// AI provider cooldowns (v1.5.27 — quota-aware backoff).
//
// When a provider replies 429 with a `usage limit` / `quota` signal in the
// body, the worker writes a cooldown record so subsequent claim cycles
// requeue jobs whose stage would route to that provider rather than burn
// per-job retries. The dashboard reads `list_active_provider_cooldowns` to
// surface "provider X paused until Y" warnings.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AiProviderCooldown {
    pub provider_name: String,
    pub cooldown_until: DateTime<Utc>,
    pub reason: String,
    pub set_at: DateTime<Utc>,
}

/// Upsert a cooldown for `provider_name`. If a cooldown row already exists,
/// the longer of (existing, new) wins — operator-set or quota-derived
/// cooldowns shouldn't be silently shortened by a follow-up 429 that
/// arrived with a smaller Retry-After.
pub async fn upsert_provider_cooldown(
    pool: &DbPool,
    provider_name: &str,
    cooldown_until: DateTime<Utc>,
    reason: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into ai_provider_cooldowns (provider_name, cooldown_until, reason)
        values ($1, $2, $3)
        on conflict (provider_name) do update
          set cooldown_until = greatest(ai_provider_cooldowns.cooldown_until, excluded.cooldown_until),
              reason = excluded.reason,
              updated_at = now()
        "#,
    )
    .bind(provider_name)
    .bind(cooldown_until)
    .bind(reason)
    .execute(pool)
    .await
    .context("upsert ai_provider_cooldowns")?;
    Ok(())
}

/// Returns the cooldown end timestamp for `provider_name` if the cooldown
/// is still active (`cooldown_until > now()`). `None` means "fire away" —
/// either no cooldown was ever set or the cooldown has elapsed.
pub async fn get_active_provider_cooldown(
    pool: &DbPool,
    provider_name: &str,
) -> Result<Option<AiProviderCooldown>> {
    let row = sqlx::query(
        r#"
        select provider_name, cooldown_until, reason, set_at
          from ai_provider_cooldowns
         where provider_name = $1
           and cooldown_until > now()
        "#,
    )
    .bind(provider_name)
    .fetch_optional(pool)
    .await
    .context("query ai_provider_cooldowns")?;
    row.map(|row| {
        Ok(AiProviderCooldown {
            provider_name: row.try_get("provider_name")?,
            cooldown_until: row.try_get("cooldown_until")?,
            reason: row.try_get("reason")?,
            set_at: row.try_get("set_at")?,
        })
    })
    .transpose()
}

/// Drop a single provider's cooldown row early. Used by the operator
/// "Cooldown aufheben" action when they've just upgraded a plan / paid
/// for headroom and want the next claim cycle to retry immediately.
/// Idempotent — no error if no row matches.
pub async fn clear_provider_cooldown(pool: &DbPool, provider_name: &str) -> Result<u64> {
    let res = sqlx::query("delete from ai_provider_cooldowns where provider_name = $1")
        .bind(provider_name)
        .execute(pool)
        .await
        .context("delete ai_provider_cooldown")?;
    Ok(res.rows_affected())
}

/// Drop *all* provider cooldowns at once. The dashboard "Entsperren"
/// action calls this alongside `unblock_jobs_from_failed_predecessors`
/// so a one-click recovery clears both the dead-queue blockers and the
/// quota-cooldowns that caused them.
pub async fn clear_all_provider_cooldowns(pool: &DbPool) -> Result<u64> {
    let res = sqlx::query("delete from ai_provider_cooldowns")
        .execute(pool)
        .await
        .context("delete all ai_provider_cooldowns")?;
    Ok(res.rows_affected())
}

/// Wake jobs that a provider cooldown (or other backoff) deferred into the
/// future by resetting `run_after` to now, so the worker claims them on the
/// next poll instead of waiting out the full cooldown window. Targets only
/// `queued` jobs whose `run_after` is still in the future; in-flight and
/// already-eligible jobs are untouched, and `attempts` is preserved (this
/// reschedules, it does not reset the retry budget). Returns the number
/// released. Called by the manual "release parked jobs" operation, folded into
/// the cooldown-lift action, and triggered on an AI model/provider change so a
/// parked backlog immediately runs under the new configuration.
pub async fn release_scheduled_retries(pool: &DbPool) -> Result<u64> {
    let res = sqlx::query(
        r#"
        update jobs
           set run_after = now(),
               updated_at = now()
         where status = 'queued'
           and run_after > now()
        "#,
    )
    .execute(pool)
    .await
    .context("release scheduled job retries")?;
    Ok(res.rows_affected())
}

/// All currently-active cooldowns, ordered by remaining time (longest
/// first). Used by the dashboard to surface "provider X paused" warnings.
pub async fn list_active_provider_cooldowns(pool: &DbPool) -> Result<Vec<AiProviderCooldown>> {
    let rows = sqlx::query(
        r#"
        select provider_name, cooldown_until, reason, set_at
          from ai_provider_cooldowns
         where cooldown_until > now()
         order by cooldown_until desc
        "#,
    )
    .fetch_all(pool)
    .await
    .context("list ai_provider_cooldowns")?;
    rows.into_iter()
        .map(|row| {
            Ok(AiProviderCooldown {
                provider_name: row.try_get("provider_name")?,
                cooldown_until: row.try_get("cooldown_until")?,
                reason: row.try_get("reason")?,
                set_at: row.try_get("set_at")?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Operator unblock (v1.5.27).
//
// When a cascade of permanent failures (typically: provider quota
// exhausted, every retry burned) leaves a run with a `failed` predecessor
// stage, every subsequent queued job in that run is gated by the
// `claim_jobs` "NOT EXISTS earlier-stage in (failed, …)" filter. The
// queue silently stops draining. `unblock_jobs_from_failed_predecessors`
// finds those failed predecessors, resets them to `queued` with
// `attempts = 0`, and returns the count so the dashboard can report
// "N jobs unblocked".
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct UnblockSummary {
    /// How many `failed` predecessor jobs were reset back to queued.
    pub predecessors_requeued: i64,
    /// How many distinct runs are now eligible to make progress again.
    pub runs_unblocked: i64,
}

/// Reset every `failed` job that has at least one downstream `queued`
/// sibling (same run, higher `stage_priority`) back to `queued` with
/// `attempts = 0`. Optional `error_substring` filters to failures whose
/// `error_message ILIKE '%' || substring || '%'` — useful for unblocking
/// only the post-quota-exhaustion cohort while leaving genuine code-bug
/// failures pinned. Pass `None` to retry every blocking failure.
pub async fn unblock_jobs_from_failed_predecessors(
    pool: &DbPool,
    error_substring: Option<&str>,
) -> Result<UnblockSummary> {
    let row = sqlx::query(
        r#"
        with predecessors as (
            select j.id, j.run_id
              from jobs j
             where j.status = 'failed'
               and ($1::text is null or j.error_message ilike '%' || $1 || '%')
               and exists (
                   select 1 from jobs later
                    where later.run_id = j.run_id
                      and later.stage_priority > j.stage_priority
                      and later.status = 'queued'
               )
        ),
        updated as (
            update jobs j
               set status = 'queued',
                   attempts = 0,
                   error_message = null,
                   run_after = now(),
                   updated_at = now()
              from predecessors p
             where j.id = p.id
            returning j.run_id
        )
        select
            count(*)::bigint                  as predecessors_requeued,
            count(distinct run_id)::bigint    as runs_unblocked
          from updated
        "#,
    )
    .bind(error_substring)
    .fetch_one(pool)
    .await
    .context("unblock failed predecessors")?;
    Ok(UnblockSummary {
        predecessors_requeued: row.try_get("predecessors_requeued")?,
        runs_unblocked: row.try_get("runs_unblocked")?,
    })
}

/// Count queued jobs that the `claim_jobs` filter currently refuses to
/// hand out because an earlier-stage sibling is in (`failed`,
/// `waiting_review`). Surfaced on the dashboard so an operator sees the
/// dead-queue size and can decide to unblock.
pub async fn count_blocked_queued_jobs(pool: &DbPool) -> Result<BlockedQueuedCounts> {
    let row = sqlx::query(
        r#"
        select
            sum(case when blocker_status = 'failed'         then 1 else 0 end)::bigint as blocked_by_failed,
            sum(case when blocker_status = 'waiting_review' then 1 else 0 end)::bigint as blocked_by_review,
            count(*)::bigint as total
          from (
              select (
                  select prev.status from jobs prev
                   where prev.run_id = j.run_id
                     and prev.stage_priority < j.stage_priority
                     and prev.status in ('queued','running','waiting_review','failed')
                   limit 1
              ) as blocker_status
                from jobs j
               where j.status = 'queued'
          ) t
         where blocker_status is not null
        "#,
    )
    .fetch_one(pool)
    .await
    .context("count blocked queued jobs")?;
    Ok(BlockedQueuedCounts {
        blocked_by_failed: row
            .try_get::<Option<i64>, _>("blocked_by_failed")?
            .unwrap_or(0),
        blocked_by_review: row
            .try_get::<Option<i64>, _>("blocked_by_review")?
            .unwrap_or(0),
        total: row.try_get::<Option<i64>, _>("total")?.unwrap_or(0),
    })
}

#[derive(Debug, Clone, Default)]
pub struct BlockedQueuedCounts {
    pub blocked_by_failed: i64,
    pub blocked_by_review: i64,
    pub total: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_tokens_without_returning_raw_value() {
        assert_eq!(hash_token("secret"), hash_token("secret"));
        assert_ne!(hash_token("secret"), "secret");
    }

    #[test]
    fn status_table_names_are_static_known_tables() {
        assert_eq!(StatusTable::Jobs.name(), "jobs");
        assert_eq!(StatusTable::PipelineRuns.name(), "pipeline_runs");
        assert_eq!(StatusTable::ReviewItems.name(), "review_items");
    }

    #[test]
    fn status_column_for_stage_round_trips_every_business_stage() {
        // Every business stage must yield a static column name; orchestration-only stages
        // must surface a typed error so callers never silently fall through to format!.
        for stage in Stage::all_business_stages() {
            let column = status_column_for_stage(stage)
                .unwrap_or_else(|err| panic!("missing column for {stage}: {err}"));
            assert!(
                column.ends_with("_status"),
                "column for {stage} must end with _status, got {column}"
            );
        }
        assert!(status_column_for_stage(Stage::Apply).is_err());
    }

    fn empty_counts(total: i64, complete: i64) -> BacklogCounts {
        BacklogCounts {
            total_documents: total,
            complete,
            missing_ocr: 0,
            missing_tagging: 0,
            missing_title: 0,
            missing_correspondent: 0,
            missing_document_type: 0,
            missing_document_date: 0,
            missing_fields: 0,
            waiting_review: 0,
            failed: 0,
            running: 0,
            never_processed: 0,
        }
    }

    fn unrestricted_safety(dry_run: bool) -> WorkflowSafetyStatus {
        WorkflowSafetyStatus {
            paused: false,
            dry_run,
            hourly_document_limit: None,
            daily_document_limit: None,
            hourly_remaining: None,
            daily_remaining: None,
        }
    }

    fn live_failure(failure_kind: &str) -> DashboardLiveFailure {
        DashboardLiveFailure {
            id: Uuid::nil(),
            run_id: Uuid::nil(),
            paperless_document_id: 0,
            stage: Stage::Ocr,
            status: "failed".to_owned(),
            failure_kind: failure_kind.to_owned(),
            attempts: 1,
            error_message: String::new(),
            next_attempt_at: None,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn dashboard_comparison_subtracts_previous_window_and_uses_snapshot_when_present() {
        let counts = empty_counts(120, 80);
        let current = ActivitySummary {
            jobs_created: 50,
            jobs_succeeded: 40,
            jobs_failed: 7,
        };
        let previous = Some(ActivitySummary {
            jobs_created: 30,
            jobs_succeeded: 25,
            jobs_failed: 4,
        });
        let comparison = compute_dashboard_comparison(&counts, current, previous, Some(50));
        assert_eq!(comparison.jobs_created_delta, 20);
        assert_eq!(comparison.jobs_succeeded_delta, 15);
        assert_eq!(comparison.jobs_failed_delta, 3);
        // open_backlog = 120 - 80 = 40; previous_open_backlog = 50; delta = -10.
        assert_eq!(comparison.open_backlog_delta, -10);
    }

    #[test]
    fn dashboard_comparison_falls_back_to_zero_deltas_when_history_is_missing() {
        let counts = empty_counts(120, 80);
        let current = ActivitySummary {
            jobs_created: 5,
            jobs_succeeded: 3,
            jobs_failed: 1,
        };
        // No previous window and no snapshot -> deltas should all be zero
        // because the "previous" defaults to the current values and the
        // historical backlog defaults to the current open backlog.
        let comparison = compute_dashboard_comparison(&counts, current, None, None);
        assert_eq!(comparison.jobs_created_delta, 0);
        assert_eq!(comparison.jobs_succeeded_delta, 0);
        assert_eq!(comparison.jobs_failed_delta, 0);
        assert_eq!(comparison.open_backlog_delta, 0);
    }

    #[test]
    fn backlog_series_empty_state_synthesises_a_single_now_point() {
        let mut points: Vec<DashboardBacklogPoint> = Vec::new();
        let now = Utc::now();
        let counts = BacklogCounts {
            total_documents: 250,
            complete: 200,
            missing_ocr: 0,
            missing_tagging: 0,
            missing_title: 0,
            missing_correspondent: 0,
            missing_document_type: 0,
            missing_document_date: 0,
            missing_fields: 0,
            waiting_review: 3,
            failed: 4,
            running: 2,
            never_processed: 0,
        };
        apply_backlog_series_empty_state_fallback(
            &mut points,
            now,
            archivist_core::DashboardGranularity::Hour,
            &counts,
        );
        assert_eq!(points.len(), 1);
        let point = &points[0];
        assert_eq!(point.total_documents, 250);
        assert_eq!(point.complete, 200);
        assert_eq!(point.open_backlog, 50);
        assert_eq!(point.failed, 4);
        assert_eq!(point.waiting_review, 3);
        assert_eq!(point.running, 2);
    }

    #[test]
    fn backlog_series_empty_state_does_not_overwrite_existing_points() {
        let mut points: Vec<DashboardBacklogPoint> = vec![DashboardBacklogPoint {
            bucket: Utc::now(),
            label: "10:00".to_owned(),
            total_documents: 1,
            complete: 1,
            open_backlog: 0,
            failed: 0,
            waiting_review: 0,
            running: 0,
        }];
        apply_backlog_series_empty_state_fallback(
            &mut points,
            Utc::now(),
            archivist_core::DashboardGranularity::Hour,
            &empty_counts(99, 99),
        );
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].total_documents, 1);
    }

    #[test]
    fn needs_attention_items_emit_one_entry_per_kind() {
        let safety = WorkflowSafetyStatus {
            paused: false,
            dry_run: true,
            hourly_document_limit: Some(100),
            daily_document_limit: Some(1000),
            hourly_remaining: Some(2),  // <= ceil(100 * 0.1) = 10
            daily_remaining: Some(900), // 900 > 100 -> not below threshold
        };
        let failures = vec![
            live_failure("failed"),
            live_failure("failed"),
            live_failure("failed"),
            live_failure("retry_scheduled"),
        ];
        let items = compose_needs_attention_items(
            2,
            1,
            &safety,
            &failures,
            &BlockedQueuedCounts::default(),
            &[],
        );
        let kinds: Vec<&str> = items.iter().map(|i| i.kind.as_str()).collect();
        assert!(kinds.contains(&"stuck_runs"));
        assert!(kinds.contains(&"stale_leases"));
        assert!(kinds.contains(&"quota_low"));
        assert!(kinds.contains(&"provider_error"));
        assert!(kinds.contains(&"dry_run_active"));
    }

    #[test]
    fn needs_attention_items_sort_critical_before_warning_before_info() {
        let items = compose_needs_attention_items(
            5,
            5,
            &unrestricted_safety(true),
            &[
                live_failure("failed"),
                live_failure("failed"),
                live_failure("failed"),
            ],
            &BlockedQueuedCounts::default(),
            &[],
        );
        let severities: Vec<&str> = items.iter().map(|i| i.severity.as_str()).collect();
        // stuck_runs (critical) must come before stale_leases (warning),
        // dry_run_active (info) must come last.
        let critical_pos = severities
            .iter()
            .position(|s| *s == "critical")
            .expect("expected at least one critical item");
        let info_pos = severities
            .iter()
            .position(|s| *s == "info")
            .expect("expected at least one info item");
        assert!(
            critical_pos < info_pos,
            "critical severity ({critical_pos}) must sort before info ({info_pos}): {severities:?}"
        );
        for (index, severity) in severities.iter().enumerate().skip(1) {
            let prev = match severities[index - 1] {
                "critical" => 0,
                "warning" => 1,
                "info" => 2,
                _ => 3,
            };
            let curr = match *severity {
                "critical" => 0,
                "warning" => 1,
                "info" => 2,
                _ => 3,
            };
            assert!(prev <= curr, "ordering broken at {index}: {severities:?}");
        }
    }

    #[test]
    fn needs_attention_items_skips_provider_error_when_failures_are_below_threshold() {
        let items = compose_needs_attention_items(
            0,
            0,
            &unrestricted_safety(false),
            &[live_failure("failed"), live_failure("failed")],
            &BlockedQueuedCounts::default(),
            &[],
        );
        let has_provider_error = items.iter().any(|i| i.kind == "provider_error");
        assert!(!has_provider_error);
    }

    #[test]
    fn needs_attention_items_emit_blocked_jobs_when_present() {
        let items = compose_needs_attention_items(
            0,
            0,
            &unrestricted_safety(false),
            &[],
            &BlockedQueuedCounts {
                blocked_by_failed: 69,
                blocked_by_review: 24,
                total: 93,
            },
            &[],
        );
        let blocked = items
            .iter()
            .find(|i| i.kind == "blocked_jobs")
            .expect("expected a blocked_jobs alert when total > 0");
        assert_eq!(blocked.severity, "critical"); // any failed predecessor → critical
        assert_eq!(blocked.count, Some(93));
        assert_eq!(
            blocked.action_key.as_deref(),
            Some("dashboard.alerts.action.unblock_jobs"),
        );
    }

    #[test]
    fn needs_attention_items_emit_provider_cooldown_when_active() {
        let cooldown = AiProviderCooldown {
            provider_name: "ollama".to_owned(),
            cooldown_until: Utc::now() + chrono::Duration::hours(6),
            reason: "weekly usage limit".to_owned(),
            set_at: Utc::now(),
        };
        let items = compose_needs_attention_items(
            0,
            0,
            &unrestricted_safety(false),
            &[],
            &BlockedQueuedCounts::default(),
            &[cooldown],
        );
        let item = items
            .iter()
            .find(|i| i.kind == "provider_cooldown")
            .expect("expected provider_cooldown alert");
        assert_eq!(item.severity, "critical");
        assert!(item.description.contains("ollama"));
    }

    #[test]
    fn quota_below_threshold_uses_ten_percent_floor() {
        // Limit of 100 -> threshold = 10; remaining 10 must trip the alert,
        // remaining 11 must not.
        assert!(quota_below_threshold(Some(10), Some(100)));
        assert!(!quota_below_threshold(Some(11), Some(100)));
        // Limit of 3 -> threshold = max(ceil(0.3), 1) = 1; remaining 0 trips,
        // remaining 2 doesn't.
        assert!(quota_below_threshold(Some(0), Some(3)));
        assert!(!quota_below_threshold(Some(2), Some(3)));
        // Missing remaining or limit means no alert.
        assert!(!quota_below_threshold(None, Some(100)));
        assert!(!quota_below_threshold(Some(10), None));
        assert!(!quota_below_threshold(Some(10), Some(0)));
    }

    #[test]
    fn encrypted_secret_round_trips() {
        let key = SecretString::from("a long local encryption key for tests".to_owned());
        let ciphertext = encrypt_secret(&key, "paperless-token").unwrap();
        assert_ne!(ciphertext, "paperless-token");
        let plaintext = decrypt_secret(&key, &ciphertext).unwrap();
        assert_eq!(plaintext, "paperless-token");
    }

    #[test]
    fn ai_artifact_redaction_removes_prompts_images_and_response_text() {
        let value = json!({
            "model": "example",
            "system_prompt": "secret system prompt",
            "user_prompt": "full document text",
            "messages": [
                { "role": "user", "content": "private content", "images": ["base64-image"] }
            ],
            "usage": { "prompt_tokens": 10 }
        });
        let stored =
            prepare_ai_artifact_value(Some(value), AiArtifactStorageMode::Redacted).unwrap();
        let serialized = stored.to_string();

        assert!(!serialized.contains("secret system prompt"));
        assert!(!serialized.contains("full document text"));
        assert!(!serialized.contains("private content"));
        assert!(!serialized.contains("base64-image"));
        assert!(serialized.contains("redacted"));
        // Usage counters must survive redaction numerically, not as "[REDACTED]".
        assert_eq!(stored["usage"]["prompt_tokens"], 10);
    }

    #[test]
    fn full_storage_mode_keeps_numeric_usage_but_redacts_credentials() {
        let value = json!({
            "api_key": "sk-very-secret",
            "options": { "token": "raw-secret", "num_ctx": 4096 },
            "usage": { "prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14 },
            "prompt_eval_count": 12,
            "eval_count": 7
        });
        let stored = prepare_ai_artifact_value(Some(value), AiArtifactStorageMode::Full).unwrap();

        assert_eq!(stored["usage"]["prompt_tokens"], 10);
        assert_eq!(stored["usage"]["completion_tokens"], 4);
        assert_eq!(stored["usage"]["total_tokens"], 14);
        assert_eq!(stored["prompt_eval_count"], 12);
        assert_eq!(stored["eval_count"], 7);
        assert_eq!(stored["api_key"], "[REDACTED]");
        assert_eq!(stored["options"]["token"], "[REDACTED]");
        assert_eq!(stored["options"]["num_ctx"], 4096);
    }

    #[test]
    fn ai_artifact_metadata_only_keeps_usage_without_raw_content() {
        let value = json!({
            "model": "example",
            "response": "private model text",
            "usage": { "completion_tokens": 4 }
        });
        let stored =
            prepare_ai_artifact_value(Some(value), AiArtifactStorageMode::MetadataOnly).unwrap();
        let serialized = stored.to_string();

        assert!(!serialized.contains("private model text"));
        assert!(serialized.contains("metadata_only"));
        assert_eq!(stored["usage"]["completion_tokens"], 4);
    }

    #[test]
    fn live_llm_status_prefers_running_jobs() {
        let now = Utc::now();
        let job = DashboardLiveJob {
            id: Uuid::now_v7(),
            run_id: Uuid::now_v7(),
            trace_id: Uuid::now_v7(),
            paperless_document_id: 42,
            stage: Stage::Metadata,
            status: "running".to_owned(),
            attempts: 1,
            max_attempts: 3,
            lease_owner: Some("worker-1".to_owned()),
            lease_until: Some(now),
            updated_at: now,
            error_message: None,
        };

        let status = llm_processing_status(&[job], &[], &[]);

        assert_eq!(status.state, "running");
        assert!(status.description.contains("42"));
    }

    #[test]
    fn live_paperless_status_reports_failed_audit_event() {
        let now = Utc::now();
        let event = PaperlessAuditEvent {
            event_type: "paperless.sync".to_owned(),
            outcome: "failed".to_owned(),
            created_at: now,
            error_message: Some("Paperless timeout".to_owned()),
        };

        let status = paperless_processing_status(&[], Some(&event), &[]);

        assert_eq!(status.state, "error");
        assert_eq!(status.description, "Paperless timeout");
    }

    #[test]
    fn live_status_ignores_retry_scheduled_failures_as_hard_errors() {
        let now = Utc::now();
        let retry = DashboardLiveFailure {
            id: Uuid::now_v7(),
            run_id: Uuid::now_v7(),
            paperless_document_id: 135,
            stage: Stage::Ocr,
            status: "queued".to_owned(),
            failure_kind: "retry_scheduled".to_owned(),
            attempts: 1,
            error_message: "temporary model runner failure".to_owned(),
            next_attempt_at: Some(now),
            updated_at: now,
        };

        let status = llm_processing_status(&[], &[], &[retry]);

        assert_eq!(status.state, "idle");
        assert_eq!(status.title, "LLM idle");
    }

    #[test]
    fn selector_document_budget_uses_tightest_remaining_limit() {
        let safety = WorkflowSafetyStatus {
            paused: false,
            dry_run: false,
            hourly_document_limit: Some(10),
            daily_document_limit: Some(100),
            hourly_remaining: Some(4),
            daily_remaining: Some(25),
        };

        assert_eq!(selector_document_budget(&safety), Some(4));

        let unlimited = WorkflowSafetyStatus {
            hourly_document_limit: None,
            daily_document_limit: None,
            hourly_remaining: None,
            daily_remaining: None,
            ..safety
        };

        assert_eq!(selector_document_budget(&unlimited), None);
    }

    #[test]
    fn missing_pipeline_stages_skip_completed_documents_and_stage_tags() {
        // v1.4.0 default selector sequence is [Ocr, Metadata]; document with the OCR
        // completion tag but no metadata yet should yield Metadata only.
        let stages = missing_pipeline_stages_for_inventory(
            &Stage::all_business_stages(),
            InventoryStageState {
                ocr_status: "unknown".to_owned(),
                metadata_status: "unknown".to_owned(),
                tagging_status: "unknown".to_owned(),
                title_status: "unknown".to_owned(),
                correspondent_status: "unknown".to_owned(),
                document_type_status: "unknown".to_owned(),
                document_date_status: "unknown".to_owned(),
                fields_status: "unknown".to_owned(),
                has_ocr_completion_tag: true,
                // Documents with the tagging-completion tag are considered "metadata done"
                // because the legacy tag was applied after the per-field stages all ran.
                has_tagging_completion_tag: false,
                has_full_completion_tag: false,
            },
        );

        assert!(!stages.contains(&Stage::Ocr));
        assert!(stages.contains(&Stage::Metadata));

        let completed = missing_pipeline_stages_for_inventory(
            &Stage::all_business_stages(),
            InventoryStageState {
                ocr_status: "unknown".to_owned(),
                metadata_status: "unknown".to_owned(),
                tagging_status: "unknown".to_owned(),
                title_status: "unknown".to_owned(),
                correspondent_status: "unknown".to_owned(),
                document_type_status: "unknown".to_owned(),
                document_date_status: "unknown".to_owned(),
                fields_status: "unknown".to_owned(),
                has_ocr_completion_tag: false,
                has_tagging_completion_tag: false,
                has_full_completion_tag: true,
            },
        );

        assert!(completed.is_empty());
    }

    #[test]
    fn missing_pipeline_stages_legacy_per_field_columns_still_trigger_metadata() {
        // A v1.3 inventory row (metadata_status='unknown' default, per-field columns recorded)
        // with any per-field column needing work should still queue Metadata.
        let stages = missing_pipeline_stages_for_inventory(
            &Stage::all_business_stages(),
            InventoryStageState {
                ocr_status: "succeeded".to_owned(),
                metadata_status: "succeeded".to_owned(),
                tagging_status: "succeeded".to_owned(),
                title_status: "unknown".to_owned(),
                correspondent_status: "succeeded".to_owned(),
                document_type_status: "succeeded".to_owned(),
                document_date_status: "succeeded".to_owned(),
                fields_status: "succeeded".to_owned(),
                has_ocr_completion_tag: true,
                has_tagging_completion_tag: false,
                has_full_completion_tag: false,
            },
        );

        assert_eq!(stages, vec![Stage::Metadata]);
    }
}
