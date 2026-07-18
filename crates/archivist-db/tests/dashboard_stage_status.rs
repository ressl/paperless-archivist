//! Regression coverage for the dashboard Stage-Matrix. The inventory snapshot
//! may temporarily contain legacy status values, but authoritative full tags
//! and terminal review rejections must never be reported as pending work.

use archivist_core::DashboardRange;
use archivist_db::{connect, get_backlog_counts, get_dashboard_stats, migrate};
use chrono::{Duration, Utc};
use sqlx::Executor;

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn dashboard_pending_excludes_full_tagged_and_rejected_documents() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL test database");
    let pool = connect(&database_url, 10)
        .await
        .expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate document_inventory, jobs, pipeline_runs, review_items,
                 ai_artifacts, audit_events restart identity cascade;
        insert into document_inventory (
          paperless_document_id, current_tags, has_full_completion_tag,
          ocr_status, metadata_status, complete
        ) values
          (1, '{ai-processed}', true,  'unknown',   'unknown',  true),
          (2, '{}',             false, 'succeeded', 'rejected', false),
          (3, '{}',             false, 'unknown',   'unknown',  false);
        "#,
    )
    .await
    .expect("seed dashboard inventory states");

    let now = Utc::now();
    let counts = get_backlog_counts(&pool).await.expect("backlog counts");
    let dashboard = get_dashboard_stats(
        &pool,
        DashboardRange::Last24Hours,
        &counts,
        now,
        now - Duration::hours(24),
    )
    .await
    .expect("dashboard stats");

    let ocr = dashboard
        .stage_status
        .iter()
        .find(|stage| stage.stage == "ocr")
        .expect("OCR stage row");
    assert_eq!(ocr.complete, 2);
    assert_eq!(ocr.pending, 1);
    assert_eq!(ocr.failed, 0);

    let metadata = dashboard
        .stage_status
        .iter()
        .find(|stage| stage.stage == "metadata")
        .expect("metadata stage row");
    assert_eq!(metadata.complete, 2);
    assert_eq!(metadata.pending, 1);
    assert_eq!(metadata.failed, 0);
}
