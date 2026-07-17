//! DB-required integration tests for #346: every role/enabled mutation keeps
//! at least one enabled administrator, including concurrent demotions.

use archivist_core::Role;
use archivist_db::{
    DbPool, LastEnabledAdminError, OidcUserInput, connect, create_session, create_user_with_roles,
    list_users, migrate, set_user_enabled, set_user_roles, upsert_oidc_user,
};
use chrono::{Duration, Utc};
use sqlx::{Executor, Row};
use std::sync::Arc;
use tokio::sync::{Barrier, Mutex, MutexGuard};
use tokio::time::{Duration as TokioDuration, sleep, timeout};
use uuid::Uuid;

static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute("truncate users, audit_events restart identity cascade")
        .await
        .expect("truncate users and audit events");
    Some((guard, pool))
}

async fn create_user(pool: &DbPool, username: &str, roles: &[Role]) -> Uuid {
    create_user_with_roles(pool, username, None, "test-password-hash", roles, None)
        .await
        .expect("create user")
}

async fn enabled_admin_count(pool: &DbPool) -> i64 {
    sqlx::query_scalar(
        r#"
        select count(*)
          from users u
          join user_roles ur on ur.user_id = u.id
         where u.enabled and ur.role = 'admin'
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("count enabled admins")
}

async fn wait_for_lock_waiters(pool: &DbPool, minimum: i64) {
    timeout(TokioDuration::from_secs(3), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                r#"
                select count(*)
                  from pg_stat_activity
                 where datname = current_database()
                   and pid <> pg_backend_pid()
                   and state = 'active'
                   and wait_event_type = 'Lock'
                "#,
            )
            .fetch_one(pool)
            .await
            .expect("inspect PostgreSQL lock waiters");
            if waiting >= minimum {
                return;
            }
            sleep(TokioDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("expected concurrent operations to reach their lock waits");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn sole_admin_rejects_self_and_foreign_disable_or_demotion_without_revoking_sessions() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    let admin = create_user(&pool, "sole-admin", &[Role::Admin]).await;
    let actor = create_user(&pool, "foreign-actor", &[Role::Viewer]).await;
    let session_id = create_session(
        &pool,
        admin,
        "session-hash",
        "csrf-hash",
        Utc::now() + Duration::hours(1),
    )
    .await
    .expect("create admin session");

    let self_disable = set_user_enabled(&pool, admin, false, admin)
        .await
        .expect_err("sole admin cannot self-disable");
    assert!(
        self_disable
            .downcast_ref::<LastEnabledAdminError>()
            .is_some(),
        "unexpected error: {self_disable:#}"
    );
    let foreign_disable = set_user_enabled(&pool, admin, false, actor)
        .await
        .expect_err("sole admin cannot be disabled by another actor");
    assert!(
        foreign_disable
            .downcast_ref::<LastEnabledAdminError>()
            .is_some(),
        "unexpected error: {foreign_disable:#}"
    );
    let self_demotion = set_user_roles(&pool, admin, &[Role::Viewer], admin)
        .await
        .expect_err("sole admin cannot self-demote");
    assert!(
        self_demotion
            .downcast_ref::<LastEnabledAdminError>()
            .is_some(),
        "unexpected error: {self_demotion:#}"
    );
    let foreign_demotion = set_user_roles(&pool, admin, &[Role::Viewer], actor)
        .await
        .expect_err("sole admin cannot be demoted by another actor");
    assert!(
        foreign_demotion
            .downcast_ref::<LastEnabledAdminError>()
            .is_some(),
        "unexpected error: {foreign_demotion:#}"
    );

    let user = list_users(&pool)
        .await
        .expect("list users")
        .into_iter()
        .find(|user| user.id == admin)
        .expect("admin exists");
    assert!(user.enabled);
    assert!(user.roles.contains(&Role::Admin));
    let revoked_at: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("select revoked_at from sessions where id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("session state");
    assert!(revoked_at.is_none(), "rejected disable keeps the session");

    let rows = sqlx::query(
        r#"
        select event_type, actor_id, before, after, outcome, error_message
          from audit_events
         where outcome = 'failed'
         order by chain_position
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("rejection audits");
    assert_eq!(rows.len(), 4);
    for row in rows {
        let event_type: String = row.try_get("event_type").expect("event type");
        let outcome: String = row.try_get("outcome").expect("outcome");
        let error_message: String = row.try_get("error_message").expect("safe error");
        let before: serde_json::Value = row.try_get("before").expect("before");
        let after: serde_json::Value = row.try_get("after").expect("after");
        assert!(matches!(
            event_type.as_str(),
            "user.enabled_changed" | "user.roles_changed"
        ));
        assert_eq!(outcome, "failed");
        assert_eq!(
            error_message,
            "last enabled administrator mutation rejected"
        );
        assert_eq!(before["user_id"], admin.to_string());
        assert_eq!(after["user_id"], admin.to_string());
        assert!(!before.to_string().contains("session-hash"));
        assert!(!after.to_string().contains("session-hash"));
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn two_enabled_admins_allow_one_disable_and_one_demotion() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    let first = create_user(&pool, "first-admin", &[Role::Admin]).await;
    let second = create_user(&pool, "second-admin", &[Role::Admin]).await;

    set_user_enabled(&pool, first, false, second)
        .await
        .expect("one of two admins can be disabled");
    assert_eq!(enabled_admin_count(&pool).await, 1);

    set_user_enabled(&pool, first, true, second)
        .await
        .expect("admin can be re-enabled");
    set_user_roles(&pool, second, &[Role::Viewer], first)
        .await
        .expect("one of two admins can be demoted");
    assert_eq!(enabled_admin_count(&pool).await, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn concurrent_demotions_leave_exactly_one_enabled_admin() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    let first = create_user(&pool, "concurrent-first", &[Role::Admin]).await;
    let second = create_user(&pool, "concurrent-second", &[Role::Admin]).await;
    let barrier = Arc::new(Barrier::new(3));

    let first_task = {
        let pool = pool.clone();
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            set_user_roles(&pool, first, &[Role::Viewer], second).await
        })
    };
    let second_task = {
        let pool = pool.clone();
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            set_user_roles(&pool, second, &[Role::Viewer], first).await
        })
    };
    barrier.wait().await;
    let results = [
        first_task.await.expect("first demotion task"),
        second_task.await.expect("second demotion task"),
    ];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    let rejection = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one rejected demotion");
    assert!(
        rejection.downcast_ref::<LastEnabledAdminError>().is_some(),
        "unexpected error: {rejection:#}"
    );
    assert_eq!(enabled_admin_count(&pool).await, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn concurrent_oidc_refresh_and_disable_use_one_deadlock_free_lock_order() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    let other_admin = create_user(&pool, "other-admin", &[Role::Admin]).await;
    let oidc_admin = upsert_oidc_user(
        &pool,
        OidcUserInput {
            provider: "zitadel",
            subject: "lock-order-subject",
            username: "oidc-admin",
            email: Some("oidc-admin@example.com"),
            disabled_password_hash: "disabled-hash",
            roles: &[Role::Admin],
            allow_username_link: false,
            allow_email_link: false,
            preserve_existing_roles: false,
        },
    )
    .await
    .expect("create OIDC admin");

    // Hold the target row so both real operations reach their next lock. With
    // the old OIDC row-then-advisory ordering, releasing this row formed a
    // deterministic cycle against set_user_enabled's advisory-then-row order.
    let mut row_blocker = pool.begin().await.expect("begin row blocker");
    sqlx::query("select id from users where id = $1 for update")
        .bind(oidc_admin.id)
        .fetch_one(&mut *row_blocker)
        .await
        .expect("lock OIDC admin row");

    let oidc_task = {
        let pool = pool.clone();
        tokio::spawn(async move {
            upsert_oidc_user(
                &pool,
                OidcUserInput {
                    provider: "zitadel",
                    subject: "lock-order-subject",
                    username: "oidc-admin",
                    email: Some("oidc-admin@example.com"),
                    disabled_password_hash: "disabled-hash",
                    roles: &[Role::Admin],
                    allow_username_link: false,
                    allow_email_link: false,
                    preserve_existing_roles: false,
                },
            )
            .await
        })
    };
    wait_for_lock_waiters(&pool, 1).await;

    let disable_task = {
        let pool = pool.clone();
        tokio::spawn(
            async move { set_user_enabled(&pool, oidc_admin.id, false, other_admin).await },
        )
    };
    wait_for_lock_waiters(&pool, 2).await;
    row_blocker.commit().await.expect("release target row");

    let oidc_result = timeout(TokioDuration::from_secs(5), oidc_task)
        .await
        .expect("OIDC refresh must not deadlock")
        .expect("OIDC task");
    let disable_result = timeout(TokioDuration::from_secs(5), disable_task)
        .await
        .expect("disable must not deadlock")
        .expect("disable task");
    oidc_result.expect("OIDC refresh succeeds");
    disable_result.expect("disable succeeds");
    assert_eq!(enabled_admin_count(&pool).await, 1);
}
