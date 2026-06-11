//! DB-required integration test: `upsert_provider_cooldown` distinguishes a
//! fresh insert, an extension, and a no-op against an existing longer window
//! via PostgreSQL 18's `RETURNING old/new` (#317).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_db::{
    CooldownUpsertOutcome, DbPool, connect, get_active_provider_cooldown, migrate,
    upsert_provider_cooldown,
};
use chrono::{Duration, DurationRound, Utc};
use sqlx::Executor;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(r#"truncate ai_provider_cooldowns;"#)
        .await
        .expect("truncate ai_provider_cooldowns");
    Some(pool)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn upsert_provider_cooldown_reports_fresh_extended_and_noop() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Truncate to whole microseconds: timestamptz stores microsecond
    // precision, so a nanosecond-precision chrono value would not round-trip
    // equal through RETURNING.
    let base = (Utc::now() + Duration::hours(1))
        .duration_trunc(Duration::microseconds(1))
        .expect("truncate to microseconds");

    // 1) No row yet -> fresh insert.
    let fresh = upsert_provider_cooldown(&pool, "ollama-cloud", base, "weekly quota")
        .await
        .expect("insert cooldown");
    assert_eq!(fresh.outcome, CooldownUpsertOutcome::Inserted);
    assert_eq!(fresh.effective_until, base);
    assert_eq!(fresh.previous_until, None);

    // 2) Later expiry -> the window is extended to the requested end.
    let later = base + Duration::hours(23);
    let extended = upsert_provider_cooldown(&pool, "ollama-cloud", later, "weekly quota again")
        .await
        .expect("extend cooldown");
    assert_eq!(extended.outcome, CooldownUpsertOutcome::Extended);
    assert_eq!(extended.effective_until, later);
    assert_eq!(extended.previous_until, Some(base));

    // 3) Shorter Retry-After -> the existing longer window wins (greatest()),
    //    and the caller learns its requested value was overruled.
    let shorter = base + Duration::minutes(5);
    let unchanged = upsert_provider_cooldown(&pool, "ollama-cloud", shorter, "short throttle")
        .await
        .expect("no-op cooldown");
    assert_eq!(unchanged.outcome, CooldownUpsertOutcome::Unchanged);
    assert_eq!(unchanged.effective_until, later);
    assert_eq!(unchanged.previous_until, Some(later));

    // The persisted row matches the reported effective window and carries the
    // refreshed reason (reason/updated_at update even on the no-op arm).
    let row = get_active_provider_cooldown(&pool, "ollama-cloud")
        .await
        .expect("read cooldown")
        .expect("cooldown row exists");
    assert_eq!(row.cooldown_until, later);
    assert_eq!(row.reason, "short throttle");

    // A different provider is independent and inserts fresh.
    let other = upsert_provider_cooldown(&pool, "openai", base, "tier quota")
        .await
        .expect("insert other provider");
    assert_eq!(other.outcome, CooldownUpsertOutcome::Inserted);
}
