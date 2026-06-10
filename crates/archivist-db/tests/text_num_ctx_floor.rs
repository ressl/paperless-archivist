//! DB-required integration test: `bump_text_num_ctx_if_too_small` raises a
//! too-small text num_ctx (the old 8192 default that overflowed metadata
//! prompts) to the 16384 floor, is idempotent, and leaves operator overrides
//! above the floor untouched.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_db::{DbPool, bump_text_num_ctx_if_too_small, connect, migrate};
use sqlx::{Executor, Row};

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute("truncate settings, audit_events restart identity cascade;")
        .await
        .expect("truncate test tables");
    Some(pool)
}

async fn set_runtime_text_num_ctx(pool: &DbPool, value: i64) {
    sqlx::query(
        r#"
        insert into settings (key, value)
        values ('runtime', jsonb_build_object('ai', jsonb_build_object('ollama_text_num_ctx', $1::bigint)))
        on conflict (key) do update
           set value = jsonb_set(settings.value, '{ai,ollama_text_num_ctx}', to_jsonb($1::bigint))
        "#,
    )
    .bind(value)
    .execute(pool)
    .await
    .expect("seed runtime text num_ctx");
}

async fn read_text_num_ctx(pool: &DbPool) -> i64 {
    sqlx::query("select (value #>> '{ai,ollama_text_num_ctx}')::bigint as v from settings where key = 'runtime'")
        .fetch_one(pool)
        .await
        .expect("read num_ctx")
        .try_get::<i64, _>("v")
        .expect("num_ctx value")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn bump_text_num_ctx_raises_low_values_and_leaves_high_ones() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // The old 8192 default is below the floor → bumped to 16384 and persisted.
    set_runtime_text_num_ctx(&pool, 8192).await;
    let summary = bump_text_num_ctx_if_too_small(&pool)
        .await
        .expect("bump from 8192");
    assert!(summary.bumped, "8192 is below the floor and must be bumped");
    assert_eq!(summary.previous, Some(8192));
    assert_eq!(summary.current, 16384);
    assert_eq!(
        read_text_num_ctx(&pool).await,
        16384,
        "persisted to the floor"
    );

    // Idempotent: a second pass is a no-op now that it sits at the floor.
    let again = bump_text_num_ctx_if_too_small(&pool)
        .await
        .expect("idempotent pass");
    assert!(!again.bumped, "already at the floor; no bump");
    assert_eq!(again.current, 16384);

    // An operator override above the floor is left untouched.
    set_runtime_text_num_ctx(&pool, 65536).await;
    let override_pass = bump_text_num_ctx_if_too_small(&pool)
        .await
        .expect("override pass");
    assert!(!override_pass.bumped, "must not lower an operator override");
    assert_eq!(read_text_num_ctx(&pool).await, 65536);
}
