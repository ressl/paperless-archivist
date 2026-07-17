//! PostgreSQL-required identity namespace coverage for issue #350.

use std::path::Path;
use std::sync::Arc;

use archivist_core::Role;
use archivist_db::{
    DbPool, OidcUserInput, connect, create_user_with_roles, find_user_for_login, migrate,
    upsert_oidc_user,
};
use sqlx::{Executor, Row};
use tokio::sync::{Barrier, Mutex, MutexGuard};
use uuid::Uuid;

static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute("truncate users, audit_events restart identity cascade")
        .await
        .expect("truncate identity fixtures");
    Some((guard, pool))
}

async fn create_test_user(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
) -> anyhow::Result<Uuid> {
    create_user_with_roles(
        pool,
        username,
        email,
        "test-password-hash",
        &[Role::Viewer],
        None,
    )
    .await
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn case_and_cross_column_collisions_have_one_database_owner() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };

    create_test_user(&pool, "Alice", Some("owner@example.com"))
        .await
        .expect("create identity owner");

    for (username, email) in [
        (" alice ", None),
        ("other-user", Some(" OWNER@example.com ")),
        ("OWNER@example.com", None),
        ("third-user", Some(" ALICE ")),
    ] {
        let result = create_test_user(&pool, username, email).await;
        assert!(
            result.is_err(),
            "normalized identity collision must fail for username={username:?} email={email:?}"
        );
    }

    let update_target = create_test_user(&pool, "update-target", None)
        .await
        .expect("create update target");
    let update = sqlx::query("update users set email = ' ALICE ' where id = $1")
        .bind(update_target)
        .execute(&pool)
        .await;
    assert!(
        update.is_err(),
        "direct updates must pass through the same cross-column invariant"
    );

    let owner = find_user_for_login(&pool, "  ALICE  ")
        .await
        .expect("lookup normalized username")
        .expect("identity owner");
    assert_eq!(owner.username, "Alice");

    let same_value = create_test_user(&pool, "self@example.com", Some(" SELF@example.com "))
        .await
        .expect("one user may own the same normalized username and email");
    let claims: i64 = sqlx::query_scalar(
        "select count(*)::bigint from users_identity_namespace where user_id = $1",
    )
    .bind(same_value)
    .fetch_one(&pool)
    .await
    .expect("count coalesced claims");
    assert_eq!(claims, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn parallel_case_variants_cannot_bypass_the_identity_namespace() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").expect("database url");
    let pool_a = connect(&url, 2).await.expect("connect writer A");
    let pool_b = connect(&url, 2).await.expect("connect writer B");
    let barrier = Arc::new(Barrier::new(3));

    let writer_a = {
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            create_test_user(&pool_a, "RaceUser", None).await
        })
    };
    let writer_b = {
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            create_test_user(&pool_b, " raceuser ", None).await
        })
    };
    barrier.wait().await;

    let results = [writer_a.await.unwrap(), writer_b.await.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);

    let owners: i64 = sqlx::query_scalar(
        r#"
        select count(*)::bigint
          from users_identity_namespace
         where normalized_identity = normalize_user_identity('RACEUSER')
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("count normalized owners");
    assert_eq!(owners, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn oidc_linking_is_kind_aware_and_never_selects_between_two_accounts() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };

    let username_owner = create_test_user(&pool, "local-owner", Some("one@example.com"))
        .await
        .expect("create username owner");
    create_test_user(&pool, "second-owner", Some("two@example.com"))
        .await
        .expect("create email owner");

    let ambiguous = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-ambiguous-link",
            username: "local-owner",
            email: Some("TWO@example.com"),
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Viewer],
            allow_username_link: true,
            allow_email_link: true,
            preserve_existing_roles: false,
        },
    )
    .await;
    assert!(
        ambiguous.is_err(),
        "two-account OIDC matches must fail instead of choosing by row order"
    );

    let linked = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-trimmed-link",
            username: " LOCAL-OWNER ",
            email: None,
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Viewer],
            allow_username_link: true,
            allow_email_link: false,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("trimmed case variant links to username owner");
    assert_eq!(linked.id, username_owner);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn oidc_provisioning_suffixes_a_username_that_is_owned_as_an_email() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };

    create_test_user(&pool, "local-user", Some("claimed@example.com"))
        .await
        .expect("create email claim");
    let provisioned = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "subject-cross-column",
            username: "CLAIMED@example.com",
            email: None,
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Viewer],
            allow_username_link: false,
            allow_email_link: false,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("provision collision-safe OIDC username");
    assert_ne!(
        provisioned.username.trim().to_lowercase(),
        "claimed@example.com"
    );
    assert!(provisioned.username.starts_with("CLAIMED@example.com-"));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn oidc_and_local_parallel_provisioning_keep_one_owner_per_identity() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").expect("database url");
    let local_pool = connect(&url, 2).await.expect("connect local writer");
    let oidc_pool = connect(&url, 2).await.expect("connect OIDC writer");
    let barrier = Arc::new(Barrier::new(3));

    let local_writer = {
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            create_test_user(&local_pool, "shared-identity", None).await
        })
    };
    let oidc_writer = {
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            upsert_oidc_user(
                &oidc_pool,
                OidcUserInput {
                    provider: "zitadel",
                    subject: "parallel-subject",
                    username: " SHARED-IDENTITY ",
                    email: None,
                    disabled_password_hash: "disabled-hash",
                    roles: &[Role::Viewer],
                    allow_username_link: false,
                    allow_email_link: false,
                    preserve_existing_roles: false,
                },
            )
            .await
        })
    };
    barrier.wait().await;

    let local = local_writer.await.unwrap();
    let oidc = oidc_writer
        .await
        .unwrap()
        .expect("OIDC provisioning must allocate a collision-safe username");
    let owners: i64 = sqlx::query_scalar(
        r#"
        select count(*)::bigint
          from users_identity_namespace
         where normalized_identity = normalize_user_identity('shared-identity')
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("count base identity owners");
    assert_eq!(owners, 1);
    if local.is_ok() {
        assert_ne!(oidc.username.trim().to_lowercase(), "shared-identity");
    } else {
        assert_eq!(oidc.username.trim().to_lowercase(), "shared-identity");
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn migration_reports_legacy_collisions_without_merging_accounts() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    let schema = format!("identity_preflight_{}", Uuid::new_v4().simple());
    let create_schema = format!(r#"create schema "{schema}""#);
    sqlx::query(sqlx::AssertSqlSafe(create_schema))
        .execute(&pool)
        .await
        .expect("create isolated legacy schema");

    let mut connection = pool.acquire().await.expect("acquire migration connection");
    let set_search_path = format!(r#"set search_path to "{schema}", public"#);
    sqlx::query(sqlx::AssertSqlSafe(set_search_path))
        .execute(&mut *connection)
        .await
        .expect("select isolated schema");
    let migrations_dir =
        std::env::var("ARCHIVIST_MIGRATIONS_DIR").unwrap_or_else(|_| "migrations".to_owned());
    let migrator = sqlx::migrate::Migrator::new(Path::new(&migrations_dir))
        .await
        .expect("load migrations");
    migrator
        .run_to(48, &mut *connection)
        .await
        .expect("apply legacy schema through migration 48");

    let alice: Uuid = sqlx::query_scalar(
        "insert into users (username, password_hash) values ('Alice', 'hash') returning id",
    )
    .fetch_one(&mut *connection)
    .await
    .expect("insert first legacy account");
    let bob: Uuid = sqlx::query_scalar(
        "insert into users (username, email, password_hash) values ('Bob', ' alice ', 'hash') returning id",
    )
    .fetch_one(&mut *connection)
    .await
    .expect("insert colliding legacy account");

    let error = migrator
        .run(&mut *connection)
        .await
        .expect_err("identity migration must reject legacy collisions");
    let message = format!("{error:?}").to_lowercase();
    assert!(message.contains("user identity normalization collision"));
    assert!(
        message.contains("rename"),
        "actionable hint missing: {message}"
    );

    let rows = sqlx::query("select id, username, email from users order by username")
        .fetch_all(&mut *connection)
        .await
        .expect("legacy rows remain readable");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].try_get::<Uuid, _>("id").unwrap(), alice);
    assert_eq!(rows[0].try_get::<String, _>("username").unwrap(), "Alice");
    assert_eq!(rows[1].try_get::<Uuid, _>("id").unwrap(), bob);
    assert_eq!(
        rows[1].try_get::<Option<String>, _>("email").unwrap(),
        Some(" alice ".to_owned())
    );

    drop(connection);
    let drop_schema = format!(r#"drop schema "{schema}" cascade"#);
    sqlx::query(sqlx::AssertSqlSafe(drop_schema))
        .execute(&pool)
        .await
        .expect("drop isolated legacy schema");
}
