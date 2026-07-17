//! DB-required integration tests for #347: new audit hashes bind request
//! origin fields while mixed unhashed/v1/v2 history remains verifiable.

use archivist_core::AuditEventInput;
use archivist_db::{DbPool, append_audit, connect, migrate, verify_audit_integrity};
use chrono::{DateTime, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Executor, Row};
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
