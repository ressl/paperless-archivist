//! DB-required integration test: security retention also prunes
//! ocr_page_cache (full OCR page text must not outlive the artifact
//! retention).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::RuntimeSettings;
use archivist_db::{DbPool, apply_security_retention, connect, migrate};
use sqlx::Executor;
use uuid::Uuid;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"truncate ocr_page_cache, ai_artifacts, audit_events restart identity cascade;"#,
    )
    .await
    .expect("truncate test tables");
    Some(pool)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn retention_prunes_expired_ocr_page_cache_rows() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    sqlx::query(
        r#"
        insert into ocr_page_cache (paperless_document_id, page_index, page_hash, ocr_text, created_at)
        values (1, 0, 'hash-old', 'old page text', now() - interval '60 days'),
               (1, 1, 'hash-new', 'new page text', now())
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed cache rows");

    let mut settings = RuntimeSettings::default();
    settings.security.ai_artifact_retention_days = 30;
    settings.security.audit_retention_days = 30;

    let result = apply_security_retention(&pool, &settings, Uuid::now_v7())
        .await
        .expect("apply retention");
    assert_eq!(result.ocr_page_cache_deleted, 1);

    let remaining: i64 = sqlx::query_scalar("select count(*) from ocr_page_cache")
        .fetch_one(&pool)
        .await
        .expect("count cache rows");
    assert_eq!(remaining, 1);
}
