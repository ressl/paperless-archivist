use std::path::Path;
use std::time::Duration;

use aes_gcm::aead::{Aead, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{Context, Result, anyhow};
use archivist_core::{
    AiArtifactStorageMode, AuditEventInput, BacklogCounts, DashboardBacklogPoint,
    DashboardComparison, DashboardLiveFailure, DashboardLiveJob, DashboardLiveLlmEvent,
    DashboardLiveRun, DashboardLiveStatus, DashboardRange, DashboardStageStatus, DashboardStats,
    DashboardStatusCount, DashboardTimeBucket, DocumentChatSource, DocumentInventoryItem,
    LanguageDetection, ProcessingMode, ProviderUsageStats, QualityStats, Role, RuntimeSettings,
    ServiceProcessingStatus, Stage, WorkflowRules, WorkflowSafetyStatus, redact_sensitive_json,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

pub type DbPool = PgPool;

pub async fn connect(database_url: &str) -> Result<DbPool> {
    PgPoolOptions::new()
        .max_connections(10)
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
    pub email: Option<&'a str>,
    pub disabled_password_hash: &'a str,
    pub roles: &'a [Role],
    pub allow_username_link: bool,
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

pub async fn record_login_success(pool: &DbPool, user_id: Uuid) -> Result<()> {
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
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_login_failure(
    pool: &DbPool,
    user_id: Option<Uuid>,
    username: &str,
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
             or ($2::text is not null and lower(coalesce(email, '')) = lower($2::text))
           )
         order by created_at
         limit 1
        "#,
    )
    .bind(input.username)
    .bind(input.email)
    .bind(input.allow_username_link)
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

    let mut roles = load_user_roles_tx(&mut tx, user_id).await?;
    for role in input.roles {
        if !roles.contains(role) {
            roles.push(role.clone());
        }
    }
    if roles.is_empty() {
        roles.push(Role::Viewer);
    }
    replace_user_roles_tx(&mut tx, user_id, &roles).await?;

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

pub async fn revoke_session(pool: &DbPool, session_id: Uuid, actor_id: Uuid) -> Result<()> {
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
         where stage = $1 and active = true
         order by created_at desc
         limit 1
        "#,
    )
    .bind(stage.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(prompt_from_row).transpose()
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
            std::fs::read_to_string(resolved)?.trim().to_owned()
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
    sqlx::query(&sql)
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
    let rows = sqlx::query(&format!("select name from {table} order by name"))
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
) -> Result<Vec<(String, i32)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "select name, id from paperless_custom_fields where lower(name) = any($1) order by name",
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
    let row = sqlx::query(&format!(
        "select id from {table} where lower(name) = lower($1)"
    ))
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
) -> Result<DashboardStats> {
    record_dashboard_snapshot(pool, counts).await?;
    let now = Utc::now();
    let start = dashboard_range_start(pool, range, now).await?;
    let activity = activity_summary(pool, start, now).await?;
    let previous = if let Some(duration) = range.duration() {
        Some(activity_summary(pool, start - duration, start).await?)
    } else {
        None
    };
    let comparison = dashboard_comparison(pool, start, counts, activity, previous).await?;
    let job_status = status_counts(pool, "jobs").await?;
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
        },
        comparison,
        stage_status: stage_status(pool).await?,
        throughput_series: throughput_series(pool, start, now, range).await?,
        backlog_series: backlog_series(pool, start, now, range, counts).await?,
        job_status,
        run_status: status_counts(pool, "pipeline_runs").await?,
        review_status: status_counts(pool, "review_items").await?,
        provider_usage: provider_usage(pool, start).await?,
        quality: quality_stats(pool, start).await?,
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
    })
}

pub async fn get_workflow_safety_status(
    pool: &DbPool,
    settings: &RuntimeSettings,
) -> Result<WorkflowSafetyStatus> {
    let hourly_used = auto_selector_runs_since(pool, "1 hour").await?;
    let daily_used = auto_selector_runs_since(pool, "1 day").await?;
    Ok(WorkflowSafetyStatus {
        paused: settings.workflow.paused,
        dry_run: settings.workflow.dry_run,
        hourly_document_limit: settings.workflow.hourly_document_limit,
        daily_document_limit: settings.workflow.daily_document_limit,
        hourly_remaining: remaining_budget(settings.workflow.hourly_document_limit, hourly_used),
        daily_remaining: remaining_budget(settings.workflow.daily_document_limit, daily_used),
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

async fn provider_usage(pool: &DbPool, start: DateTime<Utc>) -> Result<Vec<ProviderUsageStats>> {
    let rows = sqlx::query(
        r#"
        select provider,
               model,
               stage,
               count(*)::bigint as request_count,
               coalesce(avg(duration_ms), 0)::double precision as avg_duration_ms,
               coalesce(percentile_cont(0.95) within group (order by duration_ms), 0)::bigint as p95_duration_ms,
               coalesce(sum(
                 coalesce(nullif(response #>> '{usage,prompt_tokens}', '')::bigint, 0) +
                 coalesce(nullif(response #>> '{usage,input_tokens}', '')::bigint, 0)
               ), 0)::bigint as input_tokens,
               coalesce(sum(
                 coalesce(nullif(response #>> '{usage,completion_tokens}', '')::bigint, 0) +
                 coalesce(nullif(response #>> '{usage,output_tokens}', '')::bigint, 0)
               ), 0)::bigint as output_tokens,
               count(distinct feedback.id)::bigint as feedback_count,
               count(distinct feedback.id) filter (
                 where feedback.event_type in ('review.approved', 'review.edited')
               )::bigint as positive_feedback,
               count(distinct feedback.id) filter (
                 where feedback.event_type = 'review.rejected'
               )::bigint as negative_feedback
          from ai_artifacts ai
          left join audit_events feedback
            on feedback.job_id = ai.job_id
           and feedback.event_type in ('review.approved', 'review.edited', 'review.rejected')
         where ai.created_at >= $1
         group by provider, model, stage
         order by request_count desc, provider, model, stage
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
            })
        })
        .collect()
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

async fn dashboard_range_start(
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
    let previous_open_backlog = sqlx::query(
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
    .transpose()?
    .unwrap_or(counts.total_documents - counts.complete);
    let previous = previous.unwrap_or(ActivitySummary {
        jobs_created: current.jobs_created,
        jobs_succeeded: current.jobs_succeeded,
        jobs_failed: current.jobs_failed,
    });
    Ok(DashboardComparison {
        jobs_created_delta: current.jobs_created - previous.jobs_created,
        jobs_succeeded_delta: current.jobs_succeeded - previous.jobs_succeeded,
        jobs_failed_delta: current.jobs_failed - previous.jobs_failed,
        open_backlog_delta: counts.total_documents - counts.complete - previous_open_backlog,
    })
}

async fn stage_status(pool: &DbPool) -> Result<Vec<DashboardStageStatus>> {
    let rows = sqlx::query(
        r#"
        with stage_rows as (
          select 'ocr' as stage, ocr_status as status, current_run_status from document_inventory
          union all select 'title', title_status, current_run_status from document_inventory
          union all select 'document_type', document_type_status, current_run_status from document_inventory
          union all select 'correspondent', correspondent_status, current_run_status from document_inventory
          union all select 'document_date', document_date_status, current_run_status from document_inventory
          union all select 'tags', tagging_status, current_run_status from document_inventory
          union all select 'fields', fields_status, current_run_status from document_inventory
        ),
        counted as (
          select
            stage,
            count(*)::bigint as total,
            count(*) filter (where status in ('succeeded', 'skipped', 'not_needed'))::bigint as complete,
            count(*) filter (where status = 'failed')::bigint as failed,
            count(*) filter (where status = 'waiting_review' or current_run_status = 'waiting_review')::bigint as waiting_review,
            count(*) filter (where current_run_status in ('queued', 'running', 'applying') and status not in ('succeeded', 'skipped', 'not_needed', 'failed'))::bigint as running
          from stage_rows
          group by stage
        )
        select stage, complete, failed, waiting_review, running,
               greatest(total - complete - failed - waiting_review - running, 0)::bigint as pending
          from counted
         order by case stage
           when 'ocr' then 1
           when 'title' then 2
           when 'document_type' then 3
           when 'correspondent' then 4
           when 'tags' then 5
           else 6
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

    Ok(points)
}

async fn status_counts(pool: &DbPool, table: &str) -> Result<Vec<DashboardStatusCount>> {
    let table = match table {
        "jobs" => "jobs",
        "pipeline_runs" => "pipeline_runs",
        "review_items" => "review_items",
        _ => return Err(anyhow!("unsupported status table: {table}")),
    };
    let rows = sqlx::query(&format!(
        r#"
        select status, count(*)::bigint as count
          from {table}
         group by status
         order by count desc, status
        "#
    ))
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

pub async fn list_inventory(
    pool: &DbPool,
    limit: i64,
    offset: i64,
) -> Result<Vec<DocumentInventoryItem>> {
    let rows = sqlx::query(
        r#"
        select paperless_document_id, title, original_file_name, current_tags, ocr_status,
               tagging_status, title_status, correspondent_status, document_type_status,
               document_date_status, fields_status, current_run_status, last_run_id, last_error,
               next_required_stage, needs_review, complete, document_date, detected_language,
               detected_language_confidence, detected_language_source, last_seen_at
          from document_inventory
         order by paperless_document_id desc
         limit $1 offset $2
        "#,
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(DocumentInventoryItem {
                paperless_document_id: row.try_get("paperless_document_id")?,
                title: row.try_get("title")?,
                original_file_name: row.try_get("original_file_name")?,
                current_tags: row.try_get("current_tags")?,
                ocr_status: row.try_get("ocr_status")?,
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
               greatest(
                 similarity(coalesce(title, ''), $1),
                 similarity(coalesce(original_file_name, ''), $1),
                 similarity(array_to_string(current_tags, ' '), $1)
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

pub async fn create_run_with_jobs(
    pool: &DbPool,
    paperless_document_id: i32,
    stages: &[Stage],
    mode: ProcessingMode,
    trigger_tag: &str,
    actor: &str,
) -> Result<Uuid> {
    if stages.is_empty() {
        return Err(anyhow!("cannot create a run without stages"));
    }

    let mut tx = pool.begin().await?;
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
    .fetch_optional(&mut *tx)
    .await?
    {
        return Ok(row.try_get("id")?);
    }

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
    .fetch_one(&mut *tx)
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
        .bind(json!({ "priority": ((index as i32) + 1) * 10 }))
        .execute(&mut *tx)
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
    .execute(&mut *tx)
    .await?;

    append_audit_tx(
        &mut tx,
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
        },
    )
    .await?;

    tx.commit().await?;
    Ok(run_id)
}

pub async fn queue_missing_stage(
    pool: &DbPool,
    stage: Stage,
    mode: ProcessingMode,
    actor: &str,
    rules: &WorkflowRules,
) -> Result<i64> {
    let column = status_column_for_stage(stage)?;
    let include_tags = WorkflowRules::normalized_tags(&rules.include_tags);
    let exclude_tags = WorkflowRules::normalized_tags(&rules.exclude_tags);
    let rows = sqlx::query(&format!(
        r#"
        select paperless_document_id
          from document_inventory
         where {column} not in ('succeeded', 'skipped', 'not_needed')
           and coalesce(current_run_status, '') not in ('queued', 'running', 'waiting_review', 'applying')
           and ($1::text[] = '{{}}' or current_tags && $1::text[])
           and not (current_tags && $2::text[])
         order by paperless_document_id
        "#
    ))
    .bind(&include_tags)
    .bind(&exclude_tags)
    .fetch_all(pool)
    .await?;

    let mut created = 0;
    for row in rows {
        let document_id: i32 = row.try_get("paperless_document_id")?;
        create_run_with_jobs(pool, document_id, &[stage], mode, "manual-batch", actor).await?;
        created += 1;
    }
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
    let rows = sqlx::query(
        r#"
        select paperless_document_id,
               ocr_status,
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
           and ($1::text[] = '{}' or current_tags && $1::text[])
           and not (current_tags && $2::text[])
         order by paperless_document_id
        "#,
    )
    .bind(&include_tags)
    .bind(&exclude_tags)
    .fetch_all(pool)
    .await?;

    let mut created = 0;
    for row in rows {
        if max_documents.is_some_and(|limit| created >= limit) {
            break;
        }
        let stages = missing_pipeline_stages_for_inventory(
            enabled_stages,
            InventoryStageState {
                ocr_status: row.try_get("ocr_status")?,
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

        let document_id: i32 = row.try_get("paperless_document_id")?;
        create_run_with_jobs(pool, document_id, &stages, mode, trigger_tag, actor).await?;
        created += 1;
    }
    Ok(created)
}

struct InventoryStageState {
    ocr_status: String,
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
            Stage::Tags => {
                !state.has_tagging_completion_tag && stage_needs_work(&state.tagging_status)
            }
            Stage::Title => stage_needs_work(&state.title_status),
            Stage::Correspondent => stage_needs_work(&state.correspondent_status),
            Stage::DocumentType => stage_needs_work(&state.document_type_status),
            Stage::DocumentDate => stage_needs_work(&state.document_date_status),
            Stage::Fields => stage_needs_work(&state.fields_status),
            Stage::OcrFix | Stage::Apply => false,
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
    let rows = sqlx::query(
        r#"
        with claimed as (
          select id
            from jobs
           where ((status = 'queued' and run_after <= now())
              or (status = 'running' and lease_until < now()))
             and not exists (
               select 1
                 from jobs prev
                where prev.run_id = jobs.run_id
                  and prev.priority < jobs.priority
                  and prev.status in ('queued', 'running', 'waiting_review', 'failed')
             )
           order by case when error_message is not null and attempts > 0 then 0 else 1 end,
                    run_after,
                    priority,
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
                    j.attempts, j.max_attempts, j.payload
        )
        select u.id, u.run_id, u.paperless_document_id, u.stage, r.mode, u.status,
               u.attempts, u.max_attempts, u.payload
          from updated u
          join pipeline_runs r on r.id = u.run_id
        "#,
    )
    .bind(limit)
    .bind(lease_owner)
    .bind(lease_seconds as f64)
    .fetch_all(pool)
    .await?;

    let mut jobs = Vec::new();
    for row in rows {
        let stage: String = row.try_get("stage")?;
        let mode: String = row.try_get("mode")?;
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
        mark_run_running(pool, job.run_id, job.paperless_document_id).await?;
        jobs.push(job);
    }
    Ok(jobs)
}

async fn mark_run_running(pool: &DbPool, run_id: Uuid, document_id: i32) -> Result<()> {
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

pub async fn complete_job(pool: &DbPool, job: &JobRecord, result: Value) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        update jobs
           set status = 'succeeded',
               result = $2,
               lease_owner = null,
               lease_until = null,
               error_message = null,
               updated_at = now()
         where id = $1
        "#,
    )
    .bind(job.id)
    .bind(&result)
    .execute(&mut *tx)
    .await?;

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
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
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

pub async fn fail_job(pool: &DbPool, job: &JobRecord, error: &str, retryable: bool) -> Result<()> {
    let retry = retryable && job.attempts < job.max_attempts;
    let status = if retry { "queued" } else { "failed" };
    let delay_seconds = (2_i64.pow(job.attempts.clamp(0, 6) as u32)) * 30;
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        update jobs
           set status = $2,
               error_message = $3,
               lease_owner = null,
               lease_until = null,
               run_after = case when $2 = 'queued' then now() + make_interval(secs => $4) else run_after end,
               updated_at = now()
         where id = $1
        "#,
    )
    .bind(job.id)
    .bind(status)
    .bind(error)
    .bind(delay_seconds as f64)
    .execute(&mut *tx)
    .await?;

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
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn create_review_item(
    pool: &DbPool,
    job: &JobRecord,
    suggested_patch: Value,
    validation_warnings: Value,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
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

    sqlx::query("update jobs set status = 'waiting_review', updated_at = now() where id = $1")
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
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
        },
    )
    .await?;
    tx.commit().await?;
    Ok(id)
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
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
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
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn pending_review_for_apply(
    pool: &DbPool,
    review_id: Uuid,
) -> Result<Option<ReviewItemRecord>> {
    let row = sqlx::query(
        r#"
        select id, run_id, job_id, paperless_document_id, stage, status,
               suggested_patch, edited_patch, validation_warnings, created_at
          from review_items
         where id = $1 and status in ('approved', 'edited')
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
            created_at: row.try_get("created_at")?,
        })
    })
    .transpose()
}

pub async fn mark_review_applied(pool: &DbPool, review_id: Uuid, actor_id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        r#"
        update review_items
           set status = 'applied',
               reviewed_by = coalesce(reviewed_by, $2),
               reviewed_at = coalesce(reviewed_at, now())
         where id = $1
        returning run_id, job_id, paperless_document_id, stage
        "#,
    )
    .bind(review_id)
    .bind(actor_id)
    .fetch_one(&mut *tx)
    .await?;

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
        },
    )
    .await?;
    tx.commit().await?;
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
                if CONTENT_KEYS
                    .iter()
                    .any(|needle| key.to_ascii_lowercase().contains(needle))
                {
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
          (select count(*)::bigint from audit_events) as audit_events,
          (select count(*)::bigint from audit_events where event_type = 'workflow.selector_ran') as selector_runs_total,
          coalesce((
            select sum(coalesce((after ->> 'queued')::bigint, 0))::bigint
              from audit_events
             where event_type = 'workflow.selector_ran'
          ), 0) as selector_documents_queued_total,
          (select count(*)::bigint from audit_events where event_type = 'job.retry_scheduled') as job_retries_scheduled_total,
          (select count(*)::bigint
             from jobs
            where error_message is not null
              and stage in ('ocr', 'tags', 'title', 'correspondent', 'document_type', 'fields')
          ) as model_errors_total,
          (select count(*)::bigint from audit_events where event_type = 'document.patch_applied' and outcome = 'success') as apply_success_total,
          (select count(*)::bigint from audit_events where event_type = 'document.patch_apply_failed' and outcome = 'failed') as apply_failure_total,
          coalesce((
            select count(*)::bigint
              from audit_events
             where event_type in ('document.patch_applied', 'document.patch_apply_failed')
               and metadata ? 'duration_ms'
          ), 0) as apply_latency_ms_count,
          coalesce((
            select sum((metadata ->> 'duration_ms')::bigint)::bigint
              from audit_events
             where event_type in ('document.patch_applied', 'document.patch_apply_failed')
               and metadata ? 'duration_ms'
          ), 0) as apply_latency_ms_sum,
          coalesce((
            select (percentile_disc(0.95) within group (order by (metadata ->> 'duration_ms')::bigint))::bigint
              from audit_events
             where event_type in ('document.patch_applied', 'document.patch_apply_failed')
               and metadata ? 'duration_ms'
          ), 0) as apply_latency_ms_p95
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
    let prev_event_hash: Option<String> = sqlx::query(
        r#"
        select event_hash
          from audit_events
         where event_hash is not null
         order by created_at desc, id desc
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
          before, after, metadata, outcome, error_message, prev_event_hash, event_hash, created_at
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        "#,
    )
    .bind(id)
    .bind(event.run_id)
    .bind(event.job_id)
    .bind(event.paperless_document_id)
    .bind(&event.event_type)
    .bind(&event.actor_type)
    .bind(&event.actor_id)
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
    let legacy_events: i64 =
        sqlx::query("select count(*)::bigint as count from audit_events where event_hash is null")
            .fetch_one(pool)
            .await?
            .try_get("count")?;
    let rows = sqlx::query(
        r#"
        select id, run_id, job_id, paperless_document_id, event_type, actor_type, actor_id,
               before, after, metadata, outcome, error_message, created_at,
               prev_event_hash, event_hash
          from audit_events
         where event_hash is not null
         order by created_at asc, id asc
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut checked_events = 0_i64;
    let mut latest_event_hash = None;
    for row in rows {
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
    let mut tx = pool.begin().await?;
    let ai_artifacts_deleted = sqlx::query("delete from ai_artifacts where created_at < $1")
        .bind(artifact_cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected() as i64;
    let audit_events_deleted = sqlx::query("delete from audit_events where created_at < $1")
        .bind(audit_cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected() as i64;
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
                "ai_artifacts_deleted": ai_artifacts_deleted
            })),
            outcome: "success".to_owned(),
            error_message: None,
        },
    )
    .await?;
    tx.commit().await?;

    Ok(RetentionResult {
        audit_events_deleted,
        ai_artifacts_deleted,
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
    sqlx::query(&sql)
        .bind(paperless_document_id)
        .bind(status)
        .bind(error)
        .bind(needs_review)
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn status_column_for_stage(stage: Stage) -> Result<&'static str> {
    match stage {
        Stage::Ocr => Ok("ocr_status"),
        Stage::Tags => Ok("tagging_status"),
        Stage::Title => Ok("title_status"),
        Stage::Correspondent => Ok("correspondent_status"),
        Stage::DocumentType => Ok("document_type_status"),
        Stage::DocumentDate => Ok("document_date_status"),
        Stage::Fields => Ok("fields_status"),
        Stage::OcrFix | Stage::Apply => Err(anyhow!("stage does not map to inventory status")),
    }
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
        assert!(serialized.contains("prompt_tokens"));
        assert!(serialized.contains("redacted"));
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
        assert!(serialized.contains("completion_tokens"));
    }

    #[test]
    fn live_llm_status_prefers_running_jobs() {
        let now = Utc::now();
        let job = DashboardLiveJob {
            id: Uuid::now_v7(),
            run_id: Uuid::now_v7(),
            trace_id: Uuid::now_v7(),
            paperless_document_id: 42,
            stage: Stage::Tags,
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
        let stages = missing_pipeline_stages_for_inventory(
            &Stage::all_business_stages(),
            InventoryStageState {
                ocr_status: "unknown".to_owned(),
                tagging_status: "unknown".to_owned(),
                title_status: "unknown".to_owned(),
                correspondent_status: "unknown".to_owned(),
                document_type_status: "unknown".to_owned(),
                document_date_status: "unknown".to_owned(),
                fields_status: "unknown".to_owned(),
                has_ocr_completion_tag: true,
                has_tagging_completion_tag: true,
                has_full_completion_tag: false,
            },
        );

        assert!(!stages.contains(&Stage::Ocr));
        assert!(!stages.contains(&Stage::Tags));
        assert!(stages.contains(&Stage::Title));

        let completed = missing_pipeline_stages_for_inventory(
            &Stage::all_business_stages(),
            InventoryStageState {
                ocr_status: "unknown".to_owned(),
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
}
