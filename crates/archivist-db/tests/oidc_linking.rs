//! DB-required integration tests for OIDC upserts: email-match account
//! linking requires the explicit `allow_email_link` opt-in, returning logins
//! replace roles (with a `user.roles_replaced` audit trail, #307), degraded
//! ID-token claims preserve existing roles, and the last remaining enabled
//! admin can never be demoted (#299).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::Role;
use archivist_db::{
    DbPool, OidcUserInput, connect, create_user_with_roles, migrate, upsert_oidc_user,
};
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard};

/// The tests in this binary truncate shared tables and then assert on their
/// global contents; run in parallel they race each other's truncate. Serialize
/// them on a shared lock (held for the whole test via the returned guard).
static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(r#"truncate users restart identity cascade;"#)
        .await
        .expect("truncate users");
    pool.execute(r#"truncate audit_events restart identity cascade;"#)
        .await
        .expect("truncate audit_events");
    Some((guard, pool))
}

async fn roles_replaced_events(pool: &DbPool) -> Vec<(serde_json::Value, serde_json::Value)> {
    sqlx::query(
        r#"
        select before, after
          from audit_events
         where event_type = 'user.roles_replaced'
         order by chain_position
        "#,
    )
    .fetch_all(pool)
    .await
    .expect("query roles_replaced events")
    .into_iter()
    .map(|row| {
        (
            row.try_get("before").expect("before json"),
            row.try_get("after").expect("after json"),
        )
    })
    .collect()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn oidc_email_linking_requires_explicit_opt_in() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    let local_admin = create_user_with_roles(
        &pool,
        "local-admin",
        Some("admin@example.com"),
        "local-password-hash",
        &[Role::Admin],
        None,
    )
    .await
    .expect("create local admin");

    // Email matches the local admin, but linking is not opted in: the upsert
    // must create a brand-new user with only the provided roles.
    let user = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-no-link",
            username: "oidc-viewer",
            email: Some("admin@example.com"),
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Viewer],
            allow_username_link: false,
            allow_email_link: false,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("upsert without link");
    assert_ne!(user.id, local_admin, "must not inherit the local account");
    assert!(!user.roles.contains(&Role::Admin));

    // With the opt-in, a new subject with the matching email links onto the
    // existing local account and inherits it.
    let linked = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-linked",
            username: "oidc-linked",
            email: Some("admin@example.com"),
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Viewer],
            allow_username_link: false,
            allow_email_link: true,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("upsert with link");
    assert_eq!(linked.id, local_admin);
    assert_eq!(linked.username, "local-admin");
    assert!(linked.roles.contains(&Role::Admin));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn oidc_login_replaces_roles_so_allowlist_removal_demotes() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    // A second enabled admin must exist, otherwise the last-admin lockout
    // protection (#299) would legitimately refuse the demotion below.
    create_user_with_roles(
        &pool,
        "other-admin",
        None,
        "local-password-hash",
        &[Role::Admin],
        None,
    )
    .await
    .expect("create second admin");

    // First login as an admin (e.g. matched the admin allowlist).
    let admin = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-demote",
            username: "oidc-admin",
            email: None,
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Admin, Role::Operator, Role::Reviewer, Role::Auditor],
            allow_username_link: false,
            allow_email_link: false,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("first login");
    assert!(admin.roles.contains(&Role::Admin));

    // Next login after the operator removed them from the allowlist: the
    // computed roles are now just the default. Roles must be REPLACED, not
    // merged, so the stale Admin grant is gone.
    let demoted = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-demote",
            username: "oidc-admin",
            email: None,
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Viewer],
            allow_username_link: false,
            allow_email_link: false,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("second login");
    assert_eq!(demoted.id, admin.id);
    assert_eq!(
        demoted.roles,
        vec![Role::Viewer],
        "allowlist removal must demote"
    );

    // #307: the returning-login role rewrite must leave an audit trail.
    let events = roles_replaced_events(&pool).await;
    assert_eq!(events.len(), 1, "exactly one user.roles_replaced event");
    let (before, after) = &events[0];
    assert_eq!(
        before["roles"],
        serde_json::json!(["admin", "auditor", "operator", "reviewer"])
    );
    assert_eq!(after["roles"], serde_json::json!(["viewer"]));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn oidc_degraded_claims_keep_existing_roles_for_returning_user() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    // A second admin exists, so role preservation below cannot be a side
    // effect of the last-admin protection.
    create_user_with_roles(
        &pool,
        "other-admin",
        None,
        "local-password-hash",
        &[Role::Admin],
        None,
    )
    .await
    .expect("create second admin");

    let admin = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-degraded",
            username: "oidc-admin",
            email: None,
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Admin, Role::Operator, Role::Reviewer, Role::Auditor],
            allow_username_link: false,
            allow_email_link: false,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("first login");
    assert!(admin.roles.contains(&Role::Admin));

    // Returning login with DEGRADED claims (#299): the IdP sent neither
    // preferred_username nor a verified email, so the computed roles fell
    // back to the default. Existing roles must be preserved, not replaced.
    let preserved = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-degraded",
            username: "subject-degraded",
            email: None,
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Viewer],
            allow_username_link: false,
            allow_email_link: false,
            preserve_existing_roles: true,
        },
    )
    .await
    .expect("degraded returning login");
    assert_eq!(preserved.id, admin.id);
    assert!(
        preserved.roles.contains(&Role::Admin),
        "degraded claims must not demote a returning user"
    );
    assert_eq!(preserved.roles, admin.roles);

    // No role change happened, so no user.roles_replaced event either.
    assert!(roles_replaced_events(&pool).await.is_empty());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn oidc_last_remaining_admin_is_not_demoted() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    // The OIDC user is the ONLY admin in the system.
    let admin = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-last-admin",
            username: "oidc-admin",
            email: None,
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Admin, Role::Operator, Role::Reviewer, Role::Auditor],
            allow_username_link: false,
            allow_email_link: false,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("first login");
    assert!(admin.roles.contains(&Role::Admin));

    // Healthy claims, but the allowlist no longer matches: the replace would
    // leave the system without any enabled admin, so Admin must be kept.
    let kept = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-last-admin",
            username: "oidc-admin",
            email: None,
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Viewer],
            allow_username_link: false,
            allow_email_link: false,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("second login");
    assert_eq!(kept.id, admin.id);
    assert!(
        kept.roles.contains(&Role::Admin),
        "the last remaining admin must not be demoted"
    );
    assert!(kept.roles.contains(&Role::Viewer));

    // The (partial) role change is still audited with the protection noted.
    let events = roles_replaced_events(&pool).await;
    assert_eq!(events.len(), 1);
    let (before, after) = &events[0];
    assert_eq!(
        before["roles"],
        serde_json::json!(["admin", "auditor", "operator", "reviewer"])
    );
    assert_eq!(after["roles"], serde_json::json!(["viewer", "admin"]));
}
