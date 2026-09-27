//! DB-required integration tests for queue_missing_stage / queue_missing_pipeline.
//!
//! These tests are marked `#[ignore]` so the default `cargo test` run does not require a live
//! PostgreSQL instance. To exercise them locally, run
//! `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{ProcessingMode, Stage, WorkflowRules};
use archivist_db::{
    DbPool, connect, custom_field_ids_for_names, migrate, queue_missing_pipeline,
    queue_missing_stage, tag_id_pairs_for_names,
};
use sqlx::Executor;
use tokio::sync::{Mutex, MutexGuard};

/// The tests in this binary truncate shared tables and then assert on their
/// global contents; run in parallel they race each other's truncate. Serialize
/// them on a shared lock (held for the whole test via the returned guard).
static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database (DB integration tests share one database: run them serially with `-- --ignored --test-threads=1`, see scripts/verify/migration_smoke.sh)");
    migrate(&pool).await.expect("apply migrations");
    // Clear the few tables we touch so the test is hermetic across reruns.
    pool.execute(
        r#"
        truncate document_inventory, jobs, pipeline_runs, audit_events,
                 paperless_tags, paperless_custom_fields restart identity cascade;
        "#,
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
}

async fn seed_inventory(pool: &DbPool, count: i32, ocr_status: &str) {
    for id in 1..=count {
        sqlx::query(
            r#"
            insert into document_inventory (
              paperless_document_id, current_tags, ocr_status,
              has_ocr_completion_tag, has_tagging_completion_tag, has_full_completion_tag,
              current_run_status
            )
            values ($1, '{}', $2, false, false, false, null)
            "#,
        )
        .bind(id)
        .bind(ocr_status)
        .execute(pool)
        .await
        .expect("seed inventory row");
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn queue_missing_stage_respects_sql_limit() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 10, "unknown").await;

    let rules = WorkflowRules::default();
    let created = queue_missing_stage(
        &pool,
        Stage::Ocr,
        ProcessingMode::ManualReview,
        "test",
        &rules,
        Some(3),
    )
    .await
    .expect("queue_missing_stage");
    assert_eq!(created, 3, "exactly three runs should be created");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn queue_missing_pipeline_respects_budget() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 10, "unknown").await;

    let rules = WorkflowRules::default();
    let stages = Stage::all_business_stages();
    let created = queue_missing_pipeline(
        &pool,
        &stages,
        ProcessingMode::ManualReview,
        "test",
        "test",
        &rules,
        Some(3),
    )
    .await
    .expect("queue_missing_pipeline");
    assert_eq!(created, 3, "exactly three runs should be created");
}

/// Regression test for v1.5.2 Bug 1: the API endpoint `/api/batches/full` used to call
/// `queue_missing_stage` once per enabled stage, producing two SEPARATE single-stage runs
/// (one with `stages = ["ocr"]`, one with `stages = ["metadata"]`) per document. After the
/// fix the handler delegates to `queue_missing_pipeline`, which emits ONE run per document
/// carrying the full enabled-stages array so the pipeline drains in a single run.
///
/// This test seeds 5 unknown-OCR documents and asserts that, with enabled_stages
/// `[Ocr, Metadata]`, each resulting `pipeline_runs` row contains both stages.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn queue_missing_pipeline_emits_combined_stage_runs() {
    use sqlx::Row;
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 5, "unknown").await;

    let rules = WorkflowRules::default();
    let enabled = vec![Stage::Ocr, Stage::Metadata];
    let created = queue_missing_pipeline(
        &pool,
        &enabled,
        ProcessingMode::ManualReview,
        "manual-batch",
        "operator",
        &rules,
        None,
    )
    .await
    .expect("queue_missing_pipeline");
    assert_eq!(created, 5, "one run per eligible document");

    // Every run row should carry both stages, NOT a single-stage array.
    let rows = sqlx::query("select stages from pipeline_runs order by created_at")
        .fetch_all(&pool)
        .await
        .expect("fetch pipeline_runs");
    assert_eq!(rows.len(), 5, "exactly one run per document, not per-stage");
    for row in rows {
        let stages: serde_json::Value = row.try_get("stages").expect("stages column");
        let arr = stages.as_array().expect("stages is jsonb array");
        let names: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            names.contains(&"ocr") && names.contains(&"metadata"),
            "expected combined ocr+metadata stages, got {names:?}"
        );
    }
}

/// Regression test for v1.5.2 Bug 2: review_items created during the consolidated metadata
/// stage's validation-fallback branch used to contain raw LLM tag NAMES like
/// `["Hardware", "Rechnung"]` where the apply path expects `Vec<i32>`. The worker now uses
/// `tag_id_pairs_for_names` to resolve names → ids BEFORE building the review_item; this
/// test pins the SQL contract (case-insensitive match, name+id pair returned, unknown
/// names omitted) so the worker behavior stays correct.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn tag_id_pairs_for_names_is_case_insensitive_and_skips_unknown() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    sqlx::query(
        r#"
        insert into paperless_tags (id, name) values
            (7, 'Hardware'),
            (12, 'Rechnung')
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed paperless_tags");

    let requested = vec![
        "hardware".to_owned(),  // different case
        "RECHNUNG".to_owned(),  // different case
        "NoSuchTag".to_owned(), // unknown
    ];
    let pairs = tag_id_pairs_for_names(&pool, &requested)
        .await
        .expect("tag_id_pairs_for_names");
    let ids: Vec<i32> = pairs.iter().map(|(_, id)| *id).collect();
    assert!(
        ids.contains(&7),
        "Hardware id should match case-insensitively"
    );
    assert!(
        ids.contains(&12),
        "Rechnung id should match case-insensitively"
    );
    assert_eq!(pairs.len(), 2, "unknown tags are NOT returned");
}

/// Companion to the tag pairs test: same contract for custom_field_ids_for_names, since the
/// worker's resolve_custom_field_values_to_ids uses it for the same name-to-id shape fix.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn custom_field_ids_for_names_is_case_insensitive_and_skips_unknown() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    sqlx::query(
        r#"
        insert into paperless_custom_fields (id, name, data_type) values
            (1, 'Invoice Number', 'string'),
            (2, 'Total', 'monetary')
        "#,
    )
    .execute(&pool)
    .await
    .expect("seed paperless_custom_fields");

    let requested = vec!["INVOICE NUMBER".to_owned(), "ghost_field".to_owned()];
    let pairs = custom_field_ids_for_names(&pool, &requested)
        .await
        .expect("custom_field_ids_for_names");
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].1, 1);
}

/// Non-ASCII capitals must fold like ASCII ones, independent of the database
/// locale: "Ärzte" was never matched by "ärzte" because the lookup lowercased
/// the requested names with Rust's ASCII-only folding. #409
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn catalog_lookups_fold_non_ascii_capitals() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    sqlx::query("insert into paperless_tags (id, name) values (21, 'Ärzte'), (22, 'Übersetzung')")
        .execute(&pool)
        .await
        .expect("seed paperless_tags");
    sqlx::query(
        "insert into paperless_custom_fields (id, name, data_type) values (5, 'Größe', 'string')",
    )
    .execute(&pool)
    .await
    .expect("seed paperless_custom_fields");

    let requested = vec!["ärzte".to_owned(), "ÜBERSETZUNG".to_owned()];
    let mut pairs = tag_id_pairs_for_names(&pool, &requested)
        .await
        .expect("tag_id_pairs_for_names");
    pairs.sort_by_key(|(_, id)| *id);
    assert_eq!(
        pairs,
        vec![("Ärzte".to_owned(), 21), ("Übersetzung".to_owned(), 22)]
    );
    let mut ids = archivist_db::tag_ids_for_names(&pool, &requested)
        .await
        .expect("tag_ids_for_names");
    ids.sort_unstable();
    assert_eq!(ids, vec![21, 22]);

    let fields = custom_field_ids_for_names(&pool, &["GRÖSSE".to_owned(), "größe".to_owned()])
        .await
        .expect("custom_field_ids_for_names");
    assert_eq!(
        fields.len(),
        1,
        "only the lowercase spelling matches 'Größe'"
    );
    assert_eq!(fields[0].1, 5);
}

async fn seed_failed_runs(pool: &DbPool, document_id: i32, count: i32, finished_hours_ago: f64) {
    for _ in 0..count {
        sqlx::query(
            r#"
            insert into pipeline_runs (
              paperless_document_id, mode, trigger_tag, status, stages,
              finished_at, created_at, updated_at
            )
            values ($1, 'full_auto', 'auto-selector', 'failed', '["ocr"]'::jsonb,
                    now() - make_interval(secs => $2 * 3600),
                    now() - make_interval(secs => $2 * 3600 + 60),
                    now() - make_interval(secs => $2 * 3600))
            "#,
        )
        .bind(document_id)
        .bind(finished_hours_ago)
        .execute(pool)
        .await
        .expect("seed failed run");
    }
    sqlx::query(
        "update document_inventory set ocr_status = 'failed', current_run_status = 'failed' where paperless_document_id = $1",
    )
    .bind(document_id)
    .execute(pool)
    .await
    .expect("mark inventory failed");
}

async fn queued_document_ids(pool: &DbPool) -> Vec<i32> {
    sqlx::query_scalar(
        "select paperless_document_id from pipeline_runs where status = 'queued' order by paperless_document_id",
    )
    .fetch_all(pool)
    .await
    .expect("queued runs")
}

/// #401: a recently failed document must not be re-selected by the
/// auto-selector every tick; the budget goes to fresh documents instead, while
/// operator batches still pick the failed document up.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn auto_selector_skips_failed_documents_within_cooloff() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        return;
    };
    seed_inventory(&pool, 4, "unknown").await;
    // Doc 1: failed a minute ago (inside the 1h cool-off).
    seed_failed_runs(&pool, 1, 1, 0.02).await;
    // Doc 2: failed once, 2h ago (outside the 1h cool-off) -> eligible again.
    seed_failed_runs(&pool, 2, 1, 2.0).await;
    // Doc 3: hit the consecutive-failure cap long ago -> never auto-selected.
    seed_failed_runs(&pool, 3, 5, 1000.0).await;
    let rules = WorkflowRules::default();

    let created = queue_missing_pipeline(
        &pool,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "auto-selector",
        "worker",
        &rules,
        Some(10),
    )
    .await
    .expect("auto-selector");
    assert_eq!(created, 2);
    assert_eq!(queued_document_ids(&pool).await, vec![2, 4]);

    // With a budget of one, the fresh document must win over the cooled-off
    // doc 1 even though doc 1 has the lower id.
    sqlx::query("update pipeline_runs set status = 'cancelled' where status = 'queued'")
        .execute(&pool)
        .await
        .expect("cancel queued runs");
    sqlx::query(
        "update document_inventory set current_run_status = null where paperless_document_id in (2, 4)",
    )
    .execute(&pool)
    .await
    .expect("reset inventory");
    sqlx::query("delete from pipeline_runs where paperless_document_id = 2")
        .execute(&pool)
        .await
        .expect("drop doc 2 runs");
    sqlx::query(
        "update document_inventory set ocr_status = 'unknown' where paperless_document_id = 2",
    )
    .execute(&pool)
    .await
    .expect("reset doc 2");
    seed_failed_runs(&pool, 2, 1, 0.01).await;
    let created = queue_missing_pipeline(
        &pool,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "auto-selector",
        "worker",
        &rules,
        Some(1),
    )
    .await
    .expect("auto-selector budget 1");
    assert_eq!(created, 1);
    assert_eq!(queued_document_ids(&pool).await, vec![4]);

    // A manual batch is not throttled: docs 1, 2 and 3 get queued too.
    let created = queue_missing_pipeline(
        &pool,
        &[Stage::Ocr],
        ProcessingMode::ManualReview,
        "manual-batch",
        "operator",
        &rules,
        None,
    )
    .await
    .expect("manual batch");
    assert_eq!(created, 3);
    assert_eq!(queued_document_ids(&pool).await, vec![1, 2, 3, 4]);
}
