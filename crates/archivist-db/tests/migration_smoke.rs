use archivist_db::{connect, migrate};
use sqlx::Row;

#[tokio::test]
#[ignore = "requires PostgreSQL 18; run scripts/verify/migration_smoke.sh"]
async fn migrations_apply_on_fresh_postgresql_18_database() {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must point to a PostgreSQL 18 database");
    let pool = connect(&database_url)
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
}
