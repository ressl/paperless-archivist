//! DB-required integration test: the audit hash chain verifies correctly even
//! when events were written with out-of-order wall-clock timestamps (cross-pod
//! clock skew). Ordering is by the monotonic chain_position sequence, not
//! created_at. #254.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{AuditEventInput, RuntimeSettings};
use archivist_db::{
    DbPool, append_audit, apply_security_retention, connect, migrate, verify_audit_integrity,
};
use sqlx::Executor;
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

/// Both tests in this file truncate audit_events and then assert on its exact
/// global contents; run in parallel they race each other's truncate. Serialize
/// them on a shared lock (held for the whole test via the returned guard).
static AUDIT_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = AUDIT_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(r#"truncate audit_events restart identity cascade;"#)
        .await
        .expect("truncate audit_events");
    Some((guard, pool))
}

fn event(event_type: &str) -> AuditEventInput {
    AuditEventInput {
        event_type: event_type.to_owned(),
        actor_type: "test".to_owned(),
        actor_id: None,
        run_id: None,
        job_id: None,
        paperless_document_id: None,
        before: None,
        after: None,
        metadata: None,
        outcome: "success".to_owned(),
        error_message: None,
        source_ip: None,
        user_agent: None,
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn audit_chain_links_and_verifies_by_append_order_not_clock() {
    let Some((_audit_lock, pool)) = fresh_pool().await else {
        return;
    };

    for i in 0..5 {
        append_audit(&pool, event(&format!("test.event_{i}")))
            .await
            .expect("append audit event");
    }

    // chain_position is the append-order key (assigned by a sequence under the
    // advisory lock). The prev_event_hash linkage must follow it: each event's
    // prev_event_hash equals the event_hash of the row at the next-lower
    // chain_position — regardless of created_at. This is exactly the property
    // that breaks under cross-pod clock skew if ordering used created_at.
    let linked: i64 = sqlx::query_scalar(
        r#"
        select count(*)
          from audit_events ae
          join audit_events prev
            on prev.chain_position = ae.chain_position - 1
         where ae.prev_event_hash is distinct from prev.event_hash
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("count mismatched links");
    assert_eq!(
        linked, 0,
        "every event must link to the prior chain_position"
    );

    let positions: Vec<i64> =
        sqlx::query_scalar("select chain_position from audit_events order by chain_position")
            .fetch_all(&pool)
            .await
            .expect("positions");
    assert_eq!(positions, vec![1, 2, 3, 4, 5]);

    // Regression guard: the intact chain verifies under the chain_position
    // replay order (verify_audit_integrity now orders by chain_position, not
    // created_at). created_at stays part of each event's own hash, so it is
    // not rewritten here — the append-order property is asserted by the SQL
    // linkage check above, which is independent of created_at.
    let report = verify_audit_integrity(&pool)
        .await
        .expect("verify integrity");
    assert!(
        report.ok,
        "intact chain must verify; broken_reason={:?} at {:?}",
        report.broken_reason, report.broken_event_id
    );
    assert_eq!(report.checked_events, 5);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn retention_deletes_a_chain_prefix_not_a_created_at_prefix() {
    let Some((_audit_lock, pool)) = fresh_pool().await else {
        return;
    };

    for i in 0..6 {
        append_audit(&pool, event(&format!("test.event_{i}")))
            .await
            .expect("append audit event");
    }

    // Scramble created_at so it is non-monotonic vs chain_position, with an
    // inversion straddling the retention cutoff: chain_position 4 is OLDER than
    // chain_position 3. Cutoff is 50 days. A created_at-based delete would
    // remove chain_position 1,2,4 (older than 50d) but keep 3,5,6 — leaving a
    // hole, since 5's prev_event_hash points at the deleted 4. The
    // chain_position-prefix delete must instead drop only 1,2 (everything below
    // the oldest row still in the window, chain_position 3).
    let ages_days = [100_i64, 90, 10, 80, 5, 1]; // indexed by chain_position-1
    for (idx, age) in ages_days.iter().enumerate() {
        sqlx::query(
            "update audit_events set created_at = now() - make_interval(days => $1) where chain_position = $2",
        )
        .bind(*age as i32)
        .bind((idx + 1) as i64)
        .execute(&pool)
        .await
        .expect("rewrite created_at");
    }

    let mut settings = RuntimeSettings::default();
    settings.security.audit_retention_days = 50;
    settings.security.ai_artifact_retention_days = 365;

    let result = apply_security_retention(&pool, &settings, Uuid::now_v7())
        .await
        .expect("apply retention");
    assert_eq!(
        result.audit_events_deleted, 2,
        "must delete only the chain prefix 1,2"
    );

    // The survivors must be a contiguous chain_position prefix with no hole:
    // chain_position 1,2 deleted; 3,4,5,6 (and the new retention event 7) kept.
    // A created_at-based delete would have removed 4 (old created_at) while
    // keeping 3,5,6 — leaving a hole. We assert the survivor set directly
    // rather than via verify_audit_integrity, because rewriting created_at
    // above invalidates each event's own hash (created_at is part of the hash)
    // — that is a test artifact, not the production scenario where each pod
    // writes its own consistent created_at.
    let survivors: Vec<i64> = sqlx::query_scalar(
        "select chain_position from audit_events where event_type like 'test.event_%' order by chain_position",
    )
    .fetch_all(&pool)
    .await
    .expect("surviving chain positions");
    assert_eq!(
        survivors,
        vec![3, 4, 5, 6],
        "retention must drop the chain prefix 1,2 and keep a contiguous 3..6 (no hole at 4)"
    );
}
