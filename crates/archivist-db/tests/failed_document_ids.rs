//! DB-required integration test: `failed_document_ids` returns exactly the
//! documents the dashboard counts as failed (a failed `ocr` or `metadata`
//! stage) that are NOT currently being reprocessed, so the "re-run all failed"
//! maintenance action targets the right set and never shadows an active run.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_db::{DbPool, connect, failed_document_ids, migrate};
use sqlx::{Executor, Row};

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 5).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute("truncate document_inventory restart identity cascade;")
        .await
        .expect("truncate inventory");
    Some(pool)
}

async fn seed(pool: &DbPool, doc: i32, ocr: &str, meta: &str, run: Option<&str>) {
    sqlx::query(
        "insert into document_inventory (paperless_document_id, current_tags, ocr_status, metadata_status, current_run_status) values ($1, '{}', $2, $3, $4)",
    )
    .bind(doc)
    .bind(ocr)
    .bind(meta)
    .bind(run)
    .execute(pool)
    .await
    .expect("seed inventory row");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn failed_document_ids_selects_failed_and_excludes_active() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    seed(&pool, 1, "succeeded", "failed", Some("failed")).await; // metadata failed → included
    seed(&pool, 2, "failed", "queued", Some("failed")).await; // ocr failed → included
    seed(&pool, 3, "succeeded", "failed", Some("running")).await; // failed stage but ACTIVE → excluded
    seed(&pool, 4, "succeeded", "succeeded", Some("succeeded")).await; // clean → excluded
    seed(&pool, 5, "succeeded", "failed", None).await; // failed, no run status → included
    seed(&pool, 6, "succeeded", "failed", Some("queued")).await; // failed stage but re-queued → excluded

    let ids = failed_document_ids(&pool).await.expect("query failed ids");
    assert_eq!(
        ids,
        vec![1, 2, 5],
        "only failed-stage documents with no active run, sorted"
    );

    // Sanity: the count matches what a dashboard 'failed' filter excluding active runs would show.
    let dashboard_failed: i64 = sqlx::query(
        "select count(*)::bigint as n from document_inventory where (ocr_status='failed' or metadata_status='failed') and coalesce(current_run_status,'') not in ('queued','running','applying','waiting_review')",
    )
    .fetch_one(&pool)
    .await
    .expect("count")
    .try_get("n")
    .expect("n");
    assert_eq!(dashboard_failed, 3);
}
