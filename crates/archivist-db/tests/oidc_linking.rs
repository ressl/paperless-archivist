//! DB-required integration test: OIDC email-match account linking requires
//! the explicit `allow_email_link` opt-in — without it a matching email must
//! create a fresh user instead of inheriting the local account (and its
//! roles).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::Role;
use archivist_db::{
    DbPool, OidcUserInput, connect, create_user_with_roles, migrate, upsert_oidc_user,
};
use sqlx::Executor;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(r#"truncate users restart identity cascade;"#)
        .await
        .expect("truncate users");
    Some(pool)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn oidc_email_linking_requires_explicit_opt_in() {
    let Some(pool) = fresh_pool().await else {
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
    let Some(pool) = fresh_pool().await else {
        return;
    };

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
}
