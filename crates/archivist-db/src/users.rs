//! Users, sessions, API tokens, OIDC identities and the last-admin invariant.

use super::*;

const LAST_ENABLED_ADMIN_REJECTION: &str = "last enabled administrator mutation rejected";
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("at least one enabled administrator is required")]
pub struct LastEnabledAdminError;

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("username or email is already assigned to another account")]
pub struct UserIdentityConflictError;

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("username must not be blank")]
pub struct InvalidUserIdentityError;

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("OIDC identity matches multiple local accounts")]
pub struct AmbiguousUserIdentityLinkError;

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
    /// The creator's *current* roles. Token scopes are only effective while
    /// the creator still holds a matching permission. #392
    pub creator_roles: Vec<Role>,
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
    let username = username.trim();
    if username.is_empty() {
        return Err(InvalidUserIdentityError.into());
    }
    let email = email.map(str::trim).filter(|value| !value.is_empty());
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
    .await
    .map_err(map_user_identity_write_error)?
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
          from users_identity_namespace identity
          join users u on u.id = identity.user_id
          left join user_roles ur on ur.user_id = u.id
         where identity.normalized_identity = normalize_user_identity($1)
         group by u.id
        "#,
    )
    .bind(username_or_email)
    .fetch_optional(pool)
    .await?;

    row.map(auth_user_from_row).transpose()
}

const PAPERLESS_BRIDGE_AUTH_PROVIDER: &str = "paperless_bridge";

/// Resolve a Paperless login bridge account by its DB-backed origin mapping,
/// never by a coincidentally equal local username/email.
pub async fn find_paperless_bridge_user(
    pool: &DbPool,
    bridge_identity: &str,
) -> Result<Option<AuthUser>> {
    let row = sqlx::query(
        r#"
        select u.id, u.username, u.email, u.password_hash, u.enabled, u.failed_login_count,
               u.locked_until,
               coalesce(array_agg(ur.role order by ur.role) filter (where ur.role is not null), '{}') as roles
          from users u
          left join user_roles ur on ur.user_id = u.id
         where u.external_auth_provider = $1
           and u.external_subject = $2
         group by u.id
        "#,
    )
    .bind(PAPERLESS_BRIDGE_AUTH_PROVIDER)
    .bind(bridge_identity)
    .fetch_optional(pool)
    .await?;

    row.map(auth_user_from_row).transpose()
}

/// Create the bridge-owned viewer on first successful Paperless login, or
/// return the same bridge mapping after a concurrent create. A local account
/// with the same username deliberately remains a conflict: only the external
/// provider/subject mapping proves that a bridge login owns an Archivist user.
pub async fn find_or_create_paperless_bridge_user(
    pool: &DbPool,
    preferred_username: &str,
    external_subject: &str,
    disabled_password_hash: &str,
) -> Result<AuthUser> {
    let username: Option<String> = sqlx::query_scalar("select normalize_user_identity($1)")
        .bind(preferred_username)
        .fetch_one(pool)
        .await?;
    let base_username = username.ok_or(InvalidUserIdentityError)?;
    let external_subject = external_subject.trim();
    if external_subject.is_empty() {
        return Err(InvalidUserIdentityError.into());
    }
    if let Some(user) = find_paperless_bridge_user(pool, external_subject).await? {
        return Ok(user);
    }

    let mut tx = pool.begin().await?;
    // Serialize retries for one verified Paperless principal before taking any
    // user row lock. The namespace trigger remains the invariant for two
    // different principals whose preferred local usernames collide.
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('paperless_bridge:' || $1, 0))")
        .bind(external_subject)
        .execute(&mut *tx)
        .await?;

    let existing_id: Option<Uuid> = sqlx::query_scalar(
        r#"
        select id
          from users
         where external_auth_provider = $1
           and external_subject = $2
        "#,
    )
    .bind(PAPERLESS_BRIDGE_AUTH_PROVIDER)
    .bind(external_subject)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(user_id) = existing_id {
        tx.commit().await?;
        return find_auth_user_by_id(pool, user_id)
            .await?
            .ok_or_else(|| anyhow!("Paperless bridge user disappeared after lookup"));
    }

    let suffix = short_hash(external_subject);
    let mut inserted = None;
    for candidate_index in 0..100 {
        let username = match candidate_index {
            0 => base_username.clone(),
            1 => format!("{}-{}", base_username, &suffix[..8]),
            _ => format!("{}-{}{}", base_username, &suffix[..8], candidate_index - 1),
        };
        let mut savepoint: Transaction<'_, Postgres> = Transaction::begin(&mut *tx, None).await?;
        let result = sqlx::query(
            r#"
            insert into users (
              username, email, password_hash, external_auth_provider, external_subject
            )
            values ($1, null, $2, $3, $4)
            returning id
            "#,
        )
        .bind(&username)
        .bind(disabled_password_hash)
        .bind(PAPERLESS_BRIDGE_AUTH_PROVIDER)
        .bind(external_subject)
        .fetch_one(&mut *savepoint)
        .await;
        match result {
            Ok(row) => {
                let user_id: Uuid = row.try_get("id")?;
                savepoint.commit().await?;
                inserted = Some((user_id, username));
                break;
            }
            Err(error) if is_user_identity_write_error(&error) => {
                savepoint.rollback().await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let (user_id, username) =
        inserted.ok_or_else(|| anyhow!("could not allocate a unique Paperless bridge username"))?;
    sqlx::query("insert into user_roles (user_id, role) values ($1, $2)")
        .bind(user_id)
        .bind(Role::Viewer.to_string())
        .execute(&mut *tx)
        .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "user.paperless_bridge_created".to_owned(),
            actor_type: "system".to_owned(),
            actor_id: None,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "user_id": user_id,
                "username": username,
                "roles": [Role::Viewer]
            })),
            metadata: Some(json!({
                "auth_provider": PAPERLESS_BRIDGE_AUTH_PROVIDER,
                "external_subject_hash": short_hash(external_subject)
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;

    find_auth_user_by_id(pool, user_id)
        .await?
        .ok_or_else(|| anyhow!("Paperless bridge user disappeared after creation"))
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
    let email = input.email.map(str::trim).filter(|value| !value.is_empty());
    // Keep the lock order identical to manual role/enabled mutations: the
    // invariant lock always comes before any users/user_roles row lock.
    lock_enabled_admin_invariant_tx(&mut tx).await?;
    let mut linked_existing = false;
    let mut created = false;

    let existing_external = sqlx::query(
        r#"
        select id
          from users
         where external_auth_provider = $1
           and external_subject = $2
         for update
        "#,
    )
    .bind(input.provider)
    .bind(input.subject)
    .fetch_optional(&mut *tx)
    .await?;

    let user_id = if let Some(row) = existing_external {
        row.try_get("id")?
    } else {
        let link_candidates = sqlx::query(
            r#"
            select u.id
              from users u
              join users_identity_namespace identity on identity.user_id = u.id
             where u.external_auth_provider is null
               and u.external_subject is null
               and (
                 ($3::boolean
                   and identity.username_claim
                   and identity.normalized_identity = normalize_user_identity($1))
                 or
                 ($4::boolean
                   and $2::text is not null
                   and identity.email_claim
                   and identity.normalized_identity = normalize_user_identity($2::text))
               )
             order by u.id
             for update of u
            "#,
        )
        .bind(input.username)
        .bind(email)
        .bind(input.allow_username_link)
        .bind(input.allow_email_link)
        .fetch_all(&mut *tx)
        .await?;

        let mut candidate_ids = link_candidates
            .iter()
            .map(|row| row.try_get::<Uuid, _>("id"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        candidate_ids.dedup();
        if candidate_ids.len() > 1 {
            return Err(AmbiguousUserIdentityLinkError.into());
        }

        if let Some(id) = candidate_ids.first().copied() {
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
        }
    };

    if let Some(email) = email {
        let owner = sqlx::query(
            r#"
            select user_id
              from users_identity_namespace
             where normalized_identity = normalize_user_identity($1)
            "#,
        )
        .bind(email)
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| row.try_get::<Uuid, _>("user_id"))
        .transpose()?;
        if owner.is_none_or(|owner_id| owner_id == user_id) {
            let mut savepoint: Transaction<'_, Postgres> =
                Transaction::begin(&mut *tx, None).await?;
            let updated =
                sqlx::query("update users set email = $2, updated_at = now() where id = $1")
                    .bind(user_id)
                    .bind(email.trim())
                    .execute(&mut *savepoint)
                    .await;
            match updated {
                Ok(_) => savepoint.commit().await?,
                Err(error) if is_user_identity_write_error(&error) => {
                    savepoint.rollback().await?;
                    tracing::warn!(
                        user_id = %user_id,
                        "skipping OIDC email update because a concurrent identity writer won the claim"
                    );
                }
                Err(error) => return Err(error.into()),
            }
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
        && user_is_enabled_admin_tx(&mut tx, user_id).await?
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
    let base_username = input.username.trim();
    let suffix = short_hash(input.subject);
    let mut email = input.email.map(str::trim).filter(|value| !value.is_empty());
    if let Some(email_value) = email {
        let email_taken = sqlx::query(
            r#"
            select 1
              from users_identity_namespace
             where normalized_identity = normalize_user_identity($1)
            "#,
        )
        .bind(email_value)
        .fetch_optional(&mut **tx)
        .await?
        .is_some();
        if email_taken {
            email = None;
        }
    }

    let mut candidate_index = 0;
    for _ in 0..6 {
        let username = match candidate_index {
            0 => base_username.to_owned(),
            1 => format!("{}-{}", base_username, &suffix[..8]),
            _ => format!("{}-{}{}", base_username, &suffix[..8], candidate_index - 1),
        };
        let username_taken = sqlx::query(
            r#"
            select 1
              from users_identity_namespace
             where normalized_identity = normalize_user_identity($1)
            "#,
        )
        .bind(&username)
        .fetch_optional(&mut **tx)
        .await?
        .is_some();
        if username_taken {
            candidate_index += 1;
            continue;
        }

        if let Some(email_value) = email {
            let email_taken = sqlx::query(
                r#"
                select 1
                  from users_identity_namespace
                 where normalized_identity = normalize_user_identity($1)
                "#,
            )
            .bind(email_value)
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
            if email_taken {
                email = None;
            }
        }

        let mut savepoint: Transaction<'_, Postgres> = Transaction::begin(&mut **tx, None).await?;
        let inserted = sqlx::query(
            r#"
            insert into users (
              username, email, password_hash, external_auth_provider, external_subject
            )
            values ($1, $2, $3, $4, $5)
            returning id
            "#,
        )
        .bind(&username)
        .bind(email)
        .bind(input.disabled_password_hash)
        .bind(input.provider)
        .bind(input.subject)
        .fetch_one(&mut *savepoint)
        .await;

        match inserted {
            Ok(row) => {
                savepoint.commit().await?;
                return row.try_get("id").context("read inserted OIDC user id");
            }
            Err(error) if is_user_identity_write_error(&error) => {
                savepoint.rollback().await?;
                // A local/Paperless writer may have committed between the
                // namespace lookup and this insert. Re-read both claims and
                // either drop the now-taken optional email or advance to the
                // deterministic username suffix.
                continue;
            }
            Err(error) => return Err(error.into()),
        }
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

async fn lock_enabled_admin_invariant_tx(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query(
        "select pg_advisory_xact_lock(hashtext('paperless_archivist_enabled_admin_invariant'))",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn map_user_identity_write_error(error: sqlx::Error) -> anyhow::Error {
    if is_user_identity_write_error(&error) {
        UserIdentityConflictError.into()
    } else {
        error.into()
    }
}

fn is_user_identity_write_error(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|database_error| database_error.constraint())
        .is_some_and(|constraint| {
            matches!(
                constraint,
                "users_identity_namespace_pkey" | "users_username_key" | "users_email_key"
            )
        })
}

async fn user_is_enabled_admin_tx(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<bool> {
    let row = sqlx::query(
        r#"
        select 1
          from users u
          join user_roles ur on ur.user_id = u.id
         where u.id = $1
           and u.enabled
           and ur.role = $2
         limit 1
        "#,
    )
    .bind(user_id)
    .bind(Role::Admin.to_string())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.is_some())
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

async fn append_last_enabled_admin_rejection_tx(
    tx: &mut Transaction<'_, Postgres>,
    event_type: &str,
    actor_id: Uuid,
    user_id: Uuid,
    before: Value,
    after: Value,
) -> Result<()> {
    append_audit_tx(
        tx,
        AuditEventInput {
            event_type: event_type.to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(actor_id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: Some(before),
            after: Some(after),
            metadata: Some(json!({
                "target_user_id": user_id,
                "reason": "last_enabled_administrator"
            })),
            outcome: "failed".to_owned(),
            error_message: Some(LAST_ENABLED_ADMIN_REJECTION.to_owned()),
            source_ip: None,
            user_agent: None,
        },
    )
    .await
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

pub(crate) fn short_hash(value: &str) -> String {
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
        // #316: throttle the activity timestamp to one write per minute —
        // unthrottled, every authenticated request committed this UPDATE,
        // making it the most expensive part of the auth path. Safe: session
        // validity reads only expires_at/revoked_at/u.enabled (above), and
        // last_seen_at is display-only in the sessions list.
        sqlx::query(
            r#"
            update sessions
               set last_seen_at = now()
             where id = $1
               and (last_seen_at is null or last_seen_at < now() - interval '60 seconds')
            "#,
        )
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
    lock_enabled_admin_invariant_tx(&mut tx).await?;
    let before = sqlx::query("select enabled from users where id = $1")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(NotFoundError::User)?
        .try_get::<bool, _>("enabled")?;

    if before
        && !enabled
        && user_is_enabled_admin_tx(&mut tx, user_id).await?
        && !other_enabled_admin_exists_tx(&mut tx, user_id).await?
    {
        append_last_enabled_admin_rejection_tx(
            &mut tx,
            "user.enabled_changed",
            actor_id,
            user_id,
            json!({ "user_id": user_id, "enabled": before }),
            json!({ "user_id": user_id, "enabled": enabled }),
        )
        .await?;
        tx.commit().await?;
        return Err(LastEnabledAdminError.into());
    }

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
    lock_enabled_admin_invariant_tx(&mut tx).await?;
    // Unknown users used to surface as a foreign-key 500. #441
    let exists: bool = sqlx::query_scalar("select exists(select 1 from users where id = $1)")
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Err(NotFoundError::User.into());
    }
    let before = load_user_roles_tx(&mut tx, user_id).await?;

    if before.contains(&Role::Admin)
        && !roles.contains(&Role::Admin)
        && user_is_enabled_admin_tx(&mut tx, user_id).await?
        && !other_enabled_admin_exists_tx(&mut tx, user_id).await?
    {
        append_last_enabled_admin_rejection_tx(
            &mut tx,
            "user.roles_changed",
            actor_id,
            user_id,
            json!({ "user_id": user_id, "roles": before }),
            json!({ "user_id": user_id, "roles": roles }),
        )
        .await?;
        tx.commit().await?;
        return Err(LastEnabledAdminError.into());
    }

    replace_user_roles_tx(&mut tx, user_id, roles).await?;
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
    // A password change/reset is the credential-compromise response, so it
    // also revokes the API tokens this user created (they act with the
    // user's rights). Decided in #392; operators re-issue tokens afterwards.
    let api_tokens_revoked = sqlx::query(
        "update api_tokens set revoked_at = now() where created_by = $1 and revoked_at is null",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
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
            after: Some(json!({
                "user_id": user_id,
                "sessions_revoked": true,
                "api_tokens_revoked": api_tokens_revoked
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

pub async fn find_api_token(pool: &DbPool, token_hash: &str) -> Result<Option<ApiTokenPrincipal>> {
    let row = sqlx::query(
        r#"
        select t.id, t.name, t.scopes, t.created_by,
               coalesce(
                 array_agg(ur.role order by ur.role) filter (where ur.role is not null),
                 '{}'
               ) as creator_roles
          from api_tokens t
          join users u on u.id = t.created_by
          left join user_roles ur on ur.user_id = u.id
         where t.token_hash = $1
           and t.revoked_at is null
           and (t.expires_at is null or t.expires_at > now())
           -- Neutralize tokens whose creator has been disabled: set_user_enabled
           -- revokes sessions but not API tokens, so without this a disabled
           -- operator's token kept full access. #271
           and u.enabled = true
         group by t.id
        "#,
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    let token_id: Uuid = row.try_get("id")?;
    // Throttle the activity timestamp to one write per minute, like
    // sessions (#316); it is display-only. #392
    sqlx::query(
        r#"
        update api_tokens
           set last_used_at = now()
         where id = $1
           and (last_used_at is null or last_used_at < now() - interval '60 seconds')
        "#,
    )
    .bind(token_id)
    .execute(pool)
    .await?;
    let creator_roles: Vec<String> = row.try_get("creator_roles")?;
    Ok(Some(ApiTokenPrincipal {
        token_id,
        name: row.try_get("name")?,
        scopes: row.try_get("scopes")?,
        user_id: row.try_get("created_by")?,
        creator_roles: creator_roles
            .iter()
            .map(|role| role.parse())
            .collect::<std::result::Result<Vec<_>, _>>()?,
    }))
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
    .ok_or(NotFoundError::ApiToken)?;
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

/// Resolve an audit actor filter given as a username to the `actor_id` the
/// audit trail stores for users (their UUID). Case-insensitive; None when no
/// such user exists (#448).
pub async fn find_user_id_by_username(pool: &DbPool, username: &str) -> Result<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("select id from users where lower(username) = lower($1) limit 1")
            .bind(username)
            .fetch_optional(pool)
            .await?,
    )
}
