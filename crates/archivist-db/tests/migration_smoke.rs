use archivist_db::{connect, migrate};
use sqlx::Row;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires PostgreSQL 18; run scripts/verify/migration_smoke.sh"]
async fn migrations_apply_on_fresh_postgresql_18_database() {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must point to a PostgreSQL 18 database");
    let pool = connect(&database_url, 10)
        .await
        .expect("connect to PostgreSQL 18 test database");

    migrate(&pool).await.expect("apply all migrations");

    let version_num: i32 = sqlx::query("select current_setting('server_version_num')::int")
        .fetch_one(&pool)
        .await
        .expect("read PostgreSQL version")
        .try_get(0)
        .expect("parse PostgreSQL version");
    assert!(
        version_num >= 180000,
        "PostgreSQL 18 or newer is required, got {version_num}"
    );

    let tables: i64 = sqlx::query(
        r#"
        select count(*)::bigint
          from information_schema.tables
         where table_schema = 'public'
           and table_name in (
             'users',
             'settings',
             'document_inventory',
             'pipeline_runs',
             'jobs',
             'review_items',
             'audit_events',
             'notification_state'
           )
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("inspect migrated tables")
    .try_get(0)
    .expect("read table count");

    assert_eq!(tables, 8, "all GA tables should exist after migration");

    let stage_priority_generated: String = sqlx::query(
        r#"
        select attgenerated::text
          from pg_attribute
         where attrelid = 'jobs'::regclass
           and attname = 'stage_priority'
           and not attisdropped
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("inspect jobs.stage_priority generation mode")
    .try_get(0)
    .expect("read generated mode");

    assert_eq!(
        stage_priority_generated, "s",
        "jobs.stage_priority must be stored so its index is supported"
    );

    // Regression: v1.5.14 Bundle D shipped find_metadata_dedup_source with a
    // SQL query that referenced ai_artifacts.paperless_document_id and
    // ai_artifacts.normalized — neither column exists. The query compiled
    // (sqlx::query is not schema-checked) and only blew up at runtime in
    // prod, failing every metadata job. Calling it here against the empty
    // fresh DB exercises the SQL parser and column resolution against the
    // real schema; if a future change re-introduces a non-existent column
    // it will fail loudly in CI instead of in front of operators.
    let dedup = archivist_db::find_metadata_dedup_source(&pool, 0, "deadbeef").await;
    assert!(
        dedup.is_ok(),
        "find_metadata_dedup_source must parse against the live schema: {:?}",
        dedup.err()
    );

    // v1.5.21 metadata-trace helpers. Same regression pattern: every helper
    // referenced by the `GET /api/inventory/{id}/metadata-trace` route runs
    // against the empty fresh-migration DB so a future "non-existent column"
    // bug is caught at CI time rather than in front of operators. Throwaway
    // ids — the DB is empty so each call must return None/empty without
    // touching real rows.
    let throwaway_doc_id = 0;
    let throwaway_run_id = Uuid::nil();

    let header = archivist_db::latest_metadata_run_for_document(&pool, throwaway_doc_id).await;
    assert!(
        matches!(header, Ok(None)),
        "latest_metadata_run_for_document must parse against the live schema: {:?}",
        header.err()
    );

    let artifact = archivist_db::latest_metadata_artifact_for_run(&pool, throwaway_run_id).await;
    assert!(
        matches!(artifact, Ok(None)),
        "latest_metadata_artifact_for_run must parse against the live schema: {:?}",
        artifact.err()
    );

    let reviews = archivist_db::metadata_review_items_for_run(&pool, throwaway_run_id).await;
    assert!(
        matches!(&reviews, Ok(items) if items.is_empty()),
        "metadata_review_items_for_run must parse against the live schema: {:?}",
        reviews.err()
    );

    let audit = archivist_db::latest_apply_audit_for_run(&pool, throwaway_run_id).await;
    assert!(
        matches!(audit, Ok(None)),
        "latest_apply_audit_for_run must parse against the live schema: {:?}",
        audit.err()
    );
}
