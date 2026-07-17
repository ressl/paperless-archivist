//! DB-required integration tests for #347: new audit hashes bind request
//! origin fields while mixed unhashed/v1/v2 history remains verifiable.

use archivist_core::AuditEventInput;
use archivist_db::{DbPool, append_audit, connect, migrate, verify_audit_integrity};
use chrono::{DateTime, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Connection, Executor, Row};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static AUDIT_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = AUDIT_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute("truncate audit_events restart identity cascade")
        .await
        .expect("truncate audit events");
    Some((guard, pool))
}

fn event(source_ip: Option<&str>, user_agent: Option<&str>) -> AuditEventInput {
    AuditEventInput {
        event_type: "test.origin".to_owned(),
        actor_type: "test".to_owned(),
        actor_id: Some("fixture".to_owned()),
        run_id: None,
        job_id: None,
        paperless_document_id: Some(4904),
        before: None,
        after: Some(json!({ "ok": true })),
        metadata: None,
        outcome: "success".to_owned(),
        error_message: None,
        source_ip: source_ip.map(str::to_owned),
        user_agent: user_agent.map(str::to_owned),
    }
}

fn v1_hash(
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
    hex::encode(Sha256::digest(canonical.to_string().as_bytes()))
}

fn v2_hash(
    id: Uuid,
    created_at: DateTime<Utc>,
    prev_event_hash: &Option<String>,
    event: &AuditEventInput,
) -> String {
    let canonical = json!({
        "hash_version": 2,
        "id": id,
        "created_at": created_at,
        "prev_event_hash": prev_event_hash,
        "run_id": event.run_id,
        "job_id": event.job_id,
        "paperless_document_id": event.paperless_document_id,
        "event_type": &event.event_type,
        "actor_type": &event.actor_type,
        "actor_id": &event.actor_id,
        "source_ip": &event.source_ip,
        "user_agent": &event.user_agent,
        "before": &event.before,
        "after": &event.after,
        "metadata": &event.metadata,
        "outcome": &event.outcome,
        "error_message": &event.error_message,
    });
    hex::encode(Sha256::digest(canonical.to_string().as_bytes()))
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn new_events_are_explicit_v2_and_origin_tampering_breaks_verification() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    append_audit(&pool, event(Some("203.0.113.17"), Some("Archivist-Test/2")))
        .await
        .expect("append v2 event");

    let row = sqlx::query(
        "select id, hash_version, source_ip, user_agent from audit_events order by chain_position",
    )
    .fetch_one(&pool)
    .await
    .expect("load v2 event");
    let id: Uuid = row.try_get("id").expect("event id");
    assert_eq!(row.try_get::<i16, _>("hash_version").unwrap(), 2);
    assert_eq!(
        row.try_get::<Option<String>, _>("source_ip")
            .unwrap()
            .as_deref(),
        Some("203.0.113.17")
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("user_agent")
            .unwrap()
            .as_deref(),
        Some("Archivist-Test/2")
    );

    let intact = verify_audit_integrity(&pool)
        .await
        .expect("verify v2 event");
    assert!(intact.ok);
    assert_eq!(intact.legacy_events, 0);
    assert_eq!(intact.v1_events, 0);
    assert_eq!(intact.v2_events, 1);

    sqlx::query("update audit_events set source_ip = '203.0.113.99' where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("tamper source IP");
    let source_tampered = verify_audit_integrity(&pool)
        .await
        .expect("verify source tamper");
    assert!(!source_tampered.ok);
    assert_eq!(source_tampered.broken_event_id, Some(id));

    sqlx::query(
        "update audit_events set source_ip = '203.0.113.17', user_agent = null where id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .expect("restore IP and tamper user agent to null");
    let agent_tampered = verify_audit_integrity(&pool)
        .await
        .expect("verify user-agent tamper");
    assert!(!agent_tampered.ok);
    assert_eq!(agent_tampered.broken_event_id, Some(id));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn mixed_unhashed_v1_v2_history_verifies_and_reports_coverage() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };

    sqlx::query(
        r#"
        insert into audit_events (
          event_type, actor_type, outcome, hash_version, event_hash, prev_event_hash
        ) values ('test.unhashed', 'test', 'success', null, null, null)
        "#,
    )
    .execute(&pool)
    .await
    .expect("insert unhashed legacy event");

    let id = Uuid::parse_str("018f0000-0000-7000-8000-000000000010").unwrap();
    let created_at = "2026-07-17T08:30:00Z".parse::<DateTime<Utc>>().unwrap();
    let legacy = event(Some("198.51.100.10"), Some("Legacy-Agent/1"));
    let hash = v1_hash(id, created_at, &None, &legacy);
    sqlx::query(
        r#"
        insert into audit_events (
          id, paperless_document_id, event_type, actor_type, actor_id,
          source_ip, user_agent, before, after, metadata, outcome, error_message,
          created_at, prev_event_hash, event_hash
        ) values (
          $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, null, $14
        )
        "#,
    )
    .bind(id)
    .bind(legacy.paperless_document_id)
    .bind(&legacy.event_type)
    .bind(&legacy.actor_type)
    .bind(&legacy.actor_id)
    .bind(&legacy.source_ip)
    .bind(&legacy.user_agent)
    .bind(&legacy.before)
    .bind(&legacy.after)
    .bind(&legacy.metadata)
    .bind(&legacy.outcome)
    .bind(&legacy.error_message)
    .bind(created_at)
    .bind(&hash)
    .execute(&pool)
    .await
    .expect("old writer inserts v1 event without naming hash_version");
    let stored_version: i16 =
        sqlx::query_scalar("select hash_version from audit_events where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("migration default labels old-writer hash as v1");
    assert_eq!(stored_version, 1);

    append_audit(&pool, event(None, None))
        .await
        .expect("append v2 event with explicit null origins");

    let report = verify_audit_integrity(&pool)
        .await
        .expect("verify mixed chain");
    assert!(report.ok, "mixed chain: {:?}", report.broken_reason);
    assert_eq!(report.checked_events, 2);
    assert_eq!(report.legacy_events, 1);
    assert_eq!(report.v1_events, 1);
    assert_eq!(report.v2_events, 1);

    // v1 never claimed to cover origin metadata. Changing those fields must
    // remain verifiable so upgrading does not retroactively reinterpret v1.
    sqlx::query(
        "update audit_events set source_ip = '198.51.100.99', user_agent = null where id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .expect("alter v1-only origin fields");
    assert!(
        verify_audit_integrity(&pool)
            .await
            .expect("verify v1 compatibility")
            .ok
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn pre_fix_nanosecond_hashes_remain_verifiable_after_upgrade() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };

    let v1_id = Uuid::parse_str("018f0000-0000-7000-8000-000000000020").unwrap();
    let v1_created_at = "2026-07-17T08:30:00.123456789Z"
        .parse::<DateTime<Utc>>()
        .unwrap();
    let v1_event = event(Some("198.51.100.20"), Some("Legacy-Agent/1"));
    let v1_event_hash = v1_hash(v1_id, v1_created_at, &None, &v1_event);

    sqlx::query(
        r#"
        insert into audit_events (
          id, paperless_document_id, event_type, actor_type, actor_id,
          source_ip, user_agent, before, after, metadata, outcome, error_message,
          created_at, prev_event_hash, event_hash, hash_version
        ) values (
          $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, null, $14, 1
        )
        "#,
    )
    .bind(v1_id)
    .bind(v1_event.paperless_document_id)
    .bind(&v1_event.event_type)
    .bind(&v1_event.actor_type)
    .bind(&v1_event.actor_id)
    .bind(&v1_event.source_ip)
    .bind(&v1_event.user_agent)
    .bind(&v1_event.before)
    .bind(&v1_event.after)
    .bind(&v1_event.metadata)
    .bind(&v1_event.outcome)
    .bind(&v1_event.error_message)
    .bind(v1_created_at)
    .bind(&v1_event_hash)
    .execute(&pool)
    .await
    .expect("insert v1 event exactly as the pre-fix writer did");

    let v2_id = Uuid::parse_str("018f0000-0000-7000-8000-000000000021").unwrap();
    let v2_created_at = "2026-07-17T08:30:01.987654321Z"
        .parse::<DateTime<Utc>>()
        .unwrap();
    let v2_event = event(Some("198.51.100.21"), Some("Legacy-Agent/2"));
    let previous = Some(v1_event_hash.clone());
    let v2_event_hash = v2_hash(v2_id, v2_created_at, &previous, &v2_event);

    sqlx::query(
        r#"
        insert into audit_events (
          id, paperless_document_id, event_type, actor_type, actor_id,
          source_ip, user_agent, before, after, metadata, outcome, error_message,
          created_at, prev_event_hash, event_hash, hash_version
        ) values (
          $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 2
        )
        "#,
    )
    .bind(v2_id)
    .bind(v2_event.paperless_document_id)
    .bind(&v2_event.event_type)
    .bind(&v2_event.actor_type)
    .bind(&v2_event.actor_id)
    .bind(&v2_event.source_ip)
    .bind(&v2_event.user_agent)
    .bind(&v2_event.before)
    .bind(&v2_event.after)
    .bind(&v2_event.metadata)
    .bind(&v2_event.outcome)
    .bind(&v2_event.error_message)
    .bind(v2_created_at)
    .bind(&previous)
    .bind(&v2_event_hash)
    .execute(&pool)
    .await
    .expect("insert v2 event exactly as the pre-fix writer did");

    append_audit(&pool, event(None, None))
        .await
        .expect("append post-fix v2 event");

    let stored_timestamps: Vec<DateTime<Utc>> =
        sqlx::query_scalar("select created_at from audit_events order by chain_position limit 2")
            .fetch_all(&pool)
            .await
            .expect("load PostgreSQL-rounded timestamps");
    assert_ne!(stored_timestamps[0], v1_created_at);
    assert_ne!(stored_timestamps[1], v2_created_at);

    let report = verify_audit_integrity(&pool)
        .await
        .expect("verify upgraded mixed-precision chain");
    assert!(
        report.ok,
        "pre-fix hashes must verify without rewriting history: {:?}",
        report.broken_reason
    );
    assert_eq!(report.checked_events, 3);
    assert_eq!(report.legacy_precision_events, 2);

    let persisted_suffixes: Vec<Option<i16>> = sqlx::query_scalar(
        "select hash_created_at_ns_suffix from audit_events order by chain_position",
    )
    .fetch_all(&pool)
    .await
    .expect("load validated timestamp suffix hints");
    assert_eq!(
        persisted_suffixes,
        vec![Some(789), Some(321), Some(0)],
        "verification must persist the validated suffix so later scans hash each event only once"
    );

    let repeated = verify_audit_integrity(&pool)
        .await
        .expect("repeat verification using persisted hints");
    assert!(repeated.ok);
    assert_eq!(repeated.legacy_precision_events, 2);

    sqlx::query("update audit_events set event_type = 'test.tampered' where id = $1")
        .bind(v2_id)
        .execute(&pool)
        .await
        .expect("tamper legacy event payload");
    let tampered = verify_audit_integrity(&pool)
        .await
        .expect("verify tampered mixed-precision chain");
    assert!(
        !tampered.ok,
        "compatibility must not hide payload tampering"
    );
    assert_eq!(tampered.broken_event_id, Some(v2_id));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn concurrent_legacy_volume_verification_is_single_flight_and_persists_hints() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };

    let base = "2026-07-17T09:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let mut previous = None;
    for index in 0..128_i64 {
        let id = Uuid::now_v7();
        let suffix = index * 37 % 999 + 1;
        let created_at =
            base + chrono::Duration::milliseconds(index) + chrono::Duration::nanoseconds(suffix);
        let mut legacy = event(Some("198.51.100.30"), Some("Legacy-Volume/2"));
        legacy.after = Some(json!({
            "index": index,
            "representative_payload": "x".repeat(2_048)
        }));
        let event_hash = v2_hash(id, created_at, &previous, &legacy);

        sqlx::query(
            r#"
            insert into audit_events (
              id, paperless_document_id, event_type, actor_type, actor_id,
              source_ip, user_agent, before, after, metadata, outcome, error_message,
              created_at, prev_event_hash, event_hash, hash_version
            ) values (
              $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 2
            )
            "#,
        )
        .bind(id)
        .bind(legacy.paperless_document_id)
        .bind(&legacy.event_type)
        .bind(&legacy.actor_type)
        .bind(&legacy.actor_id)
        .bind(&legacy.source_ip)
        .bind(&legacy.user_agent)
        .bind(&legacy.before)
        .bind(&legacy.after)
        .bind(&legacy.metadata)
        .bind(&legacy.outcome)
        .bind(&legacy.error_message)
        .bind(created_at)
        .bind(&previous)
        .bind(&event_hash)
        .execute(&pool)
        .await
        .expect("insert representative pre-fix event");
        previous = Some(event_hash);
    }

    let mut verifiers = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let pool = pool.clone();
        verifiers.spawn(async move { verify_audit_integrity(&pool).await });
    }
    while let Some(result) = verifiers.join_next().await {
        let report = result
            .expect("verification task must not panic")
            .expect("concurrent verification must succeed");
        assert!(report.ok, "concurrent report: {:?}", report.broken_reason);
        assert_eq!(report.checked_events, 128);
        assert_eq!(report.legacy_precision_events, 128);
    }

    let missing_hints: i64 = sqlx::query_scalar(
        "select count(*) from audit_events where hash_created_at_ns_suffix is null",
    )
    .fetch_one(&pool)
    .await
    .expect("count missing timestamp hints");
    assert_eq!(missing_hints, 0);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn database_single_flight_lock_precedes_repeatable_read_snapshot() {
    let Some((_guard, pool)) = fresh_pool().await else {
        return;
    };
    append_audit(&pool, event(None, None))
        .await
        .expect("append canonical event");
    let id: Uuid = sqlx::query_scalar("select id from audit_events limit 1")
        .fetch_one(&pool)
        .await
        .expect("load event id");
    sqlx::query("update audit_events set hash_created_at_ns_suffix = null where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("simulate a not-yet-backfilled row");

    let mut first = pool.acquire().await.expect("acquire first connection");
    first.close_on_drop();
    let mut second = pool.acquire().await.expect("acquire second connection");
    second.close_on_drop();
    sqlx::query("select pg_advisory_lock(hashtext('paperless_archivist_audit_integrity_verify'))")
        .execute(&mut *first)
        .await
        .expect("first connection acquires session lock");

    let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn(async move {
        waiting_tx.send(()).expect("announce lock attempt");
        sqlx::query(
            "select pg_advisory_lock(hashtext('paperless_archivist_audit_integrity_verify'))",
        )
        .execute(&mut *second)
        .await
        .expect("second connection acquires released lock");

        let mut tx = (*second)
            .begin()
            .await
            .expect("begin post-lock transaction");
        sqlx::query("set transaction isolation level repeatable read")
            .execute(&mut *tx)
            .await
            .expect("set post-lock snapshot isolation");
        let suffix: Option<i16> =
            sqlx::query_scalar("select hash_created_at_ns_suffix from audit_events where id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await
                .expect("read suffix from post-lock snapshot");
        tx.commit().await.expect("commit reader transaction");
        let unlocked: bool = sqlx::query_scalar(
            "select pg_advisory_unlock(hashtext('paperless_archivist_audit_integrity_verify'))",
        )
        .fetch_one(&mut *second)
        .await
        .expect("unlock second connection");
        assert!(unlocked);
        suffix
    });

    waiting_rx.await.expect("second verifier starts waiting");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !waiting.is_finished(),
        "the second connection must wait for the cluster-wide verifier lock"
    );

    sqlx::query("update audit_events set hash_created_at_ns_suffix = 0 where id = $1")
        .bind(id)
        .execute(&mut *first)
        .await
        .expect("commit validated hint before releasing lock");
    let unlocked: bool = sqlx::query_scalar(
        "select pg_advisory_unlock(hashtext('paperless_archivist_audit_integrity_verify'))",
    )
    .fetch_one(&mut *first)
    .await
    .expect("unlock first connection");
    assert!(unlocked);

    assert_eq!(
        waiting.await.expect("join second verifier"),
        Some(0),
        "the repeatable-read snapshot created after lock acquisition must see the committed hint"
    );
}
