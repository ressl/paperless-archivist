//! DB-required integration tests for the sync no-op guards (#302) and the
//! sessions.last_seen_at throttle (#316): an upsert whose payload matches the
//! stored row must not physically rewrite the tuple, the inventory status
//! ratchet must still fire on drift, and find_session must only refresh the
//! activity timestamp once per minute.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::Role;
use archivist_db::{
    DbPool, InventoryUpsert, connect, create_session, create_user_with_roles, find_session,
    migrate, upsert_inventory_item, upsert_paperless_custom_field, upsert_paperless_named_entity,
    upsert_paperless_tag,
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
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
    pool.execute(
        r#"truncate document_inventory, paperless_tags, paperless_correspondents,
                    paperless_document_types, paperless_custom_fields restart identity cascade;"#,
    )
    .await
    .expect("truncate sync tables");
    Some((guard, pool))
}

/// The row version (`xmin`) plus `last_seen_at`: a no-op upsert must leave
/// both untouched, a real change must move both.
async fn row_version(
    pool: &DbPool,
    table: &str,
    id_column: &str,
    id: i32,
) -> (String, DateTime<Utc>) {
    let sql =
        format!("select xmin::text as version, last_seen_at from {table} where {id_column} = $1");
    // SAFETY: table/id_column come from string literals in this test file.
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("fetch row version");
    (
        row.try_get("version").expect("xmin"),
        row.try_get("last_seen_at").expect("last_seen_at"),
    )
}

fn inventory_item() -> InventoryUpsert {
    InventoryUpsert {
        paperless_document_id: 42,
        title: Some("Quarterly invoice".to_owned()),
        original_file_name: Some("invoice.pdf".to_owned()),
        current_tags: vec!["inbox".to_owned(), "invoice".to_owned()],
        current_tag_ids: vec![1, 2],
        correspondent_id: Some(7),
        document_type_id: Some(3),
        document_date: NaiveDate::from_ymd_opt(2026, 6, 1),
        paperless_modified_at: None,
        has_ocr_completion_tag: false,
        has_tagging_completion_tag: false,
        has_full_completion_tag: false,
    }
}

async fn apply_tag(pool: &DbPool, color: Option<&str>) {
    let mut tx = pool.begin().await.expect("begin");
    upsert_paperless_tag(&mut tx, 1, "inbox", Some("inbox"), color, false)
        .await
        .expect("upsert tag");
    tx.commit().await.expect("commit");
}

async fn apply_inventory(pool: &DbPool, item: &InventoryUpsert) {
    let mut tx = pool.begin().await.expect("begin");
    upsert_inventory_item(&mut tx, item)
        .await
        .expect("upsert inventory item");
    tx.commit().await.expect("commit");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn sync_upserts_skip_physical_writes_when_payload_unchanged() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    // Tags: identical payload leaves the tuple (and last_seen_at) untouched.
    apply_tag(&pool, Some("#ff0000")).await;
    let before = row_version(&pool, "paperless_tags", "id", 1).await;
    apply_tag(&pool, Some("#ff0000")).await;
    assert_eq!(
        row_version(&pool, "paperless_tags", "id", 1).await,
        before,
        "identical tag upsert must be a no-op"
    );
    apply_tag(&pool, Some("#00ff00")).await;
    let after = row_version(&pool, "paperless_tags", "id", 1).await;
    assert_ne!(after.0, before.0, "changed tag must rewrite the row");
    assert!(after.1 > before.1, "changed tag must bump last_seen_at");

    // Named entities (correspondents/document types share the statement).
    let mut tx = pool.begin().await.expect("begin");
    upsert_paperless_named_entity(&mut tx, "paperless_correspondents", 5, "ACME")
        .await
        .expect("upsert correspondent");
    tx.commit().await.expect("commit");
    let before = row_version(&pool, "paperless_correspondents", "id", 5).await;
    let mut tx = pool.begin().await.expect("begin");
    upsert_paperless_named_entity(&mut tx, "paperless_correspondents", 5, "ACME")
        .await
        .expect("re-upsert correspondent");
    tx.commit().await.expect("commit");
    assert_eq!(
        row_version(&pool, "paperless_correspondents", "id", 5).await,
        before,
        "identical correspondent upsert must be a no-op"
    );
    let mut tx = pool.begin().await.expect("begin");
    upsert_paperless_named_entity(&mut tx, "paperless_correspondents", 5, "ACME Corp")
        .await
        .expect("rename correspondent");
    tx.commit().await.expect("commit");
    let after = row_version(&pool, "paperless_correspondents", "id", 5).await;
    assert_ne!(after.0, before.0, "renamed correspondent must rewrite");

    // Custom fields: NULL data_type must compare as equal, not as a change.
    let mut tx = pool.begin().await.expect("begin");
    upsert_paperless_custom_field(&mut tx, 9, "Amount", None)
        .await
        .expect("upsert custom field");
    tx.commit().await.expect("commit");
    let before = row_version(&pool, "paperless_custom_fields", "id", 9).await;
    let mut tx = pool.begin().await.expect("begin");
    upsert_paperless_custom_field(&mut tx, 9, "Amount", None)
        .await
        .expect("re-upsert custom field");
    tx.commit().await.expect("commit");
    assert_eq!(
        row_version(&pool, "paperless_custom_fields", "id", 9).await,
        before,
        "identical custom field upsert must be a no-op"
    );
    let mut tx = pool.begin().await.expect("begin");
    upsert_paperless_custom_field(&mut tx, 9, "Amount", Some("monetary"))
        .await
        .expect("change custom field");
    tx.commit().await.expect("commit");
    assert_ne!(
        row_version(&pool, "paperless_custom_fields", "id", 9)
            .await
            .0,
        before.0,
        "changed data_type must rewrite"
    );

    // Inventory: identical document payload is a no-op, a title change writes.
    let item = inventory_item();
    apply_inventory(&pool, &item).await;
    let before = row_version(&pool, "document_inventory", "paperless_document_id", 42).await;
    apply_inventory(&pool, &item).await;
    assert_eq!(
        row_version(&pool, "document_inventory", "paperless_document_id", 42).await,
        before,
        "identical inventory upsert must be a no-op"
    );
    let mut renamed = inventory_item();
    renamed.title = Some("Quarterly invoice (signed)".to_owned());
    apply_inventory(&pool, &renamed).await;
    let after = row_version(&pool, "document_inventory", "paperless_document_id", 42).await;
    assert_ne!(after.0, before.0, "changed inventory row must rewrite");
    assert!(
        after.1 > before.1,
        "changed inventory row must bump last_seen_at"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn inventory_upsert_guard_preserves_status_ratchet_semantics() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    let item = inventory_item();
    apply_inventory(&pool, &item).await;

    // Another writer moved ocr_status (e.g. the failed-retry path). With the
    // completion tag still absent the upsert keeps it — and must stay a no-op.
    pool.execute(
        r#"update document_inventory set ocr_status = 'queued' where paperless_document_id = 42"#,
    )
    .await
    .expect("simulate retry writer");
    let before = row_version(&pool, "document_inventory", "paperless_document_id", 42).await;
    apply_inventory(&pool, &item).await;
    assert_eq!(
        row_version(&pool, "document_inventory", "paperless_document_id", 42).await,
        before,
        "without the completion tag the upsert must not touch ocr_status"
    );

    // The completion tag appears: the guard must let the ratchet through.
    let mut tagged = inventory_item();
    tagged.has_ocr_completion_tag = true;
    tagged.current_tags.push("ocr-done".to_owned());
    tagged.current_tag_ids.push(11);
    apply_inventory(&pool, &tagged).await;
    let row = sqlx::query(
        r#"select ocr_status from document_inventory where paperless_document_id = 42"#,
    )
    .fetch_one(&pool)
    .await
    .expect("fetch ocr_status");
    assert_eq!(
        row.try_get::<String, _>("ocr_status").expect("ocr_status"),
        "succeeded"
    );

    // Drift: ocr_status downgraded while the completion tag stays set. The
    // payload is now identical to the stored flags, but the computed ratchet
    // differs — the guard must fire and restore 'succeeded' (pre-#302
    // behavior, just without the unconditional rewrite on every round).
    pool.execute(
        r#"update document_inventory set ocr_status = 'failed' where paperless_document_id = 42"#,
    )
    .await
    .expect("simulate status drift");
    apply_inventory(&pool, &tagged).await;
    let row = sqlx::query(
        r#"select ocr_status from document_inventory where paperless_document_id = 42"#,
    )
    .fetch_one(&pool)
    .await
    .expect("fetch ocr_status");
    assert_eq!(
        row.try_get::<String, _>("ocr_status").expect("ocr_status"),
        "succeeded",
        "the ratchet must re-fire on drifted ocr_status"
    );

    // complete is an overwrite, not a ratchet: a stage marked the document
    // complete, but Paperless does not carry the processed tag — the sync
    // must still win and flip it back, exactly like the unguarded version.
    pool.execute(
        r#"update document_inventory set complete = true where paperless_document_id = 42"#,
    )
    .await
    .expect("simulate stage completion");
    apply_inventory(&pool, &tagged).await;
    let row =
        sqlx::query(r#"select complete from document_inventory where paperless_document_id = 42"#)
            .fetch_one(&pool)
            .await
            .expect("fetch complete");
    assert!(
        !row.try_get::<bool, _>("complete").expect("complete"),
        "sync without the processed tag must overwrite complete back to false"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn find_session_throttles_last_seen_at_updates() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };

    let user_id = create_user_with_roles(
        &pool,
        "session-user",
        None,
        "local-password-hash",
        &[Role::Viewer],
        None,
    )
    .await
    .expect("create user");
    let session_id = create_session(
        &pool,
        user_id,
        "throttle-session-hash",
        "csrf-secret-hash",
        Utc::now() + Duration::hours(1),
    )
    .await
    .expect("create session");

    async fn last_seen(pool: &DbPool, session_id: uuid::Uuid) -> Option<DateTime<Utc>> {
        sqlx::query(r#"select last_seen_at from sessions where id = $1"#)
            .bind(session_id)
            .fetch_one(pool)
            .await
            .expect("fetch session")
            .try_get("last_seen_at")
            .expect("last_seen_at")
    }

    // First authenticated request: NULL → now().
    let principal = find_session(&pool, "throttle-session-hash")
        .await
        .expect("find session")
        .expect("session resolves");
    assert_eq!(principal.user_id, user_id);
    let first = last_seen(&pool, session_id)
        .await
        .expect("first lookup sets last_seen_at");

    // Immediate follow-up requests are throttled: no write, same timestamp.
    find_session(&pool, "throttle-session-hash")
        .await
        .expect("find session again")
        .expect("session still resolves");
    assert_eq!(
        last_seen(&pool, session_id).await,
        Some(first),
        "a lookup within 60s must not rewrite last_seen_at"
    );

    // Once the stored timestamp is older than the 60s window, it refreshes.
    pool.execute(
        r#"update sessions set last_seen_at = now() - interval '2 minutes' where session_hash = 'throttle-session-hash'"#,
    )
    .await
    .expect("backdate last_seen_at");
    let backdated = last_seen(&pool, session_id)
        .await
        .expect("backdated value present");
    find_session(&pool, "throttle-session-hash")
        .await
        .expect("find session after backdate")
        .expect("session resolves after backdate");
    let refreshed = last_seen(&pool, session_id)
        .await
        .expect("refreshed value present");
    assert!(
        refreshed > backdated,
        "a lookup after the 60s window must refresh last_seen_at"
    );
}
