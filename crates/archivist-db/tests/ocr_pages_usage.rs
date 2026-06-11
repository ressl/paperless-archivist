//! DB-required regression tests for token counting (#300/#310).
//!
//! Since migration 0040 the usage/statistics queries read the typed
//! `ai_artifacts.input_tokens`/`output_tokens` columns instead of re-parsing
//! the response jsonb per query. These tests prove:
//!   (a) the 0040 backfill extracts every historical wire shape — OpenAI-style
//!       `usage`, Ollama top-level counters, and the pre-#259 OCR `pages[]`
//!       nesting — without double counting flattened post-#259 rows, and is
//!       idempotent;
//!   (b) migration 0037 still flattens legacy per-page usage onto the
//!       response jsonb idempotently (#300);
//!   (c) `insert_ai_artifact` fills the typed columns for new rows.
//!
//! Marked `#[ignore]` so the default `cargo test` run does not require a live
//! PostgreSQL instance. To exercise them locally, run
//! `DATABASE_URL=postgres://... cargo test -p archivist-db --test ocr_pages_usage -- --ignored`.

use std::path::Path;

use archivist_core::{AiArtifactStorageMode, DashboardRange, Stage};
use archivist_db::{
    AiArtifactInput, DbPool, connect, insert_ai_artifact, migrate, provider_bucket_entries,
    provider_usage, statistics_usage_rows,
};
use chrono::{Duration, TimeZone, Utc};
use serde_json::{Value, json};
use sqlx::{Executor, Row};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

const FLATTEN_MIGRATION: &str = "0037_backfill_ocr_artifact_usage.sql";
const TYPED_BACKFILL_MIGRATION: &str = "0040_ai_artifacts_typed_token_columns.sql";

/// The tests in this binary truncate shared tables and then assert on their
/// global contents; run in parallel they race each other's truncate. Serialize
/// them on a shared lock (held for the whole test via the returned guard).
static DB_TABLE_LOCK: Mutex<()> = Mutex::const_new(());

async fn fresh_pool() -> Option<(MutexGuard<'static, ()>, DbPool)> {
    let guard = DB_TABLE_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"
        truncate ai_artifacts, jobs, pipeline_runs, document_inventory, audit_events restart identity cascade;
        "#,
    )
    .await
    .expect("truncate test tables");
    Some((guard, pool))
}

async fn seed_run(pool: &DbPool) -> Uuid {
    sqlx::query_scalar(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
        values (1, 'full_auto', 'ai-process', 'running', '[]'::jsonb)
        returning id
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("insert pipeline run")
}

/// Raw insert that bypasses `insert_ai_artifact`, leaving the typed token
/// columns at their 0 default — exactly the state of pre-0040 rows before
/// the backfill runs.
async fn seed_artifact(
    pool: &DbPool,
    run_id: Uuid,
    stage: &str,
    response: serde_json::Value,
) -> Uuid {
    sqlx::query_scalar(
        r#"
        insert into ai_artifacts (run_id, stage, provider, model, input_hash, response, duration_ms)
        values ($1, $2, 'ollama', 'vision-test', 'hash', $3, 1200)
        returning id
        "#,
    )
    .bind(run_id)
    .bind(stage)
    .bind(response)
    .fetch_one(pool)
    .await
    .expect("insert artifact")
}

/// Legacy (pre-#259) OCR shape: per-page responses, no top-level `usage`.
/// Page 3 carries the pre-v1.11.2 string redaction and must be skipped.
/// Recoverable totals: 1200 input / 80 output.
fn legacy_ocr_response() -> Value {
    json!({
        "pages": [
            { "usage": { "prompt_tokens": 1000, "completion_tokens": 50 } },
            { "prompt_eval_count": 200, "eval_count": 30 },
            { "usage": { "prompt_tokens": "[REDACTED]", "completion_tokens": "[REDACTED]" } },
        ]
    })
}

/// Post-#259 OCR shape: pages plus the flattened top-level `usage`. Counts
/// exactly once (10 input / 5 output) — the pages fallback must skip it.
fn flattened_ocr_response() -> Value {
    json!({
        "pages": [ { "usage": { "prompt_tokens": 10, "completion_tokens": 5 } } ],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
    })
}

/// OCR pages carrying no token counters at all: contributes 0 tokens and the
/// 0037 backfill must leave it without a `usage` block, like the worker does.
fn tokenless_ocr_response() -> Value {
    json!({ "pages": [ { "response": "text only" } ] })
}

async fn seed_all_shapes(pool: &DbPool) -> (Uuid, Uuid, Uuid) {
    let run_id = seed_run(pool).await;
    let legacy = seed_artifact(pool, run_id, "ocr", legacy_ocr_response()).await;
    let flattened = seed_artifact(pool, run_id, "ocr", flattened_ocr_response()).await;
    let tokenless = seed_artifact(pool, run_id, "ocr", tokenless_ocr_response()).await;
    // Non-OCR control row with plain top-level usage: 100 input / 40 output.
    seed_artifact(
        pool,
        run_id,
        "metadata",
        json!({ "usage": { "prompt_tokens": 100, "completion_tokens": 40 } }),
    )
    .await;
    (legacy, flattened, tokenless)
}

const EXPECTED_INPUT: i64 = 1200 + 10 + 100;
const EXPECTED_OUTPUT: i64 = 80 + 5 + 40;

async fn query_totals(pool: &DbPool) -> (i64, i64) {
    let from = Utc::now() - Duration::hours(1);
    let to = Utc::now() + Duration::hours(1);

    let rows = statistics_usage_rows(pool, from, to, "day")
        .await
        .expect("statistics_usage_rows");
    let input: i64 = rows.iter().map(|row| row.input_tokens).sum();
    let output: i64 = rows.iter().map(|row| row.output_tokens).sum();

    let buckets = provider_bucket_entries(pool, from, to, DashboardRange::Last24Hours)
        .await
        .expect("provider_bucket_entries");
    let bucket_input: i64 = buckets.iter().map(|entry| entry.input_tokens).sum();
    let bucket_output: i64 = buckets.iter().map(|entry| entry.output_tokens).sum();
    assert_eq!((input, output), (bucket_input, bucket_output));

    let usage = provider_usage(pool, from).await.expect("provider_usage");
    let usage_input: i64 = usage.iter().map(|stats| stats.input_tokens).sum();
    let usage_output: i64 = usage.iter().map(|stats| stats.output_tokens).sum();
    assert_eq!((input, output), (usage_input, usage_output));

    (input, output)
}

async fn assert_query_totals(pool: &DbPool, context: &str) {
    let (input, output) = query_totals(pool).await;
    assert_eq!(input, EXPECTED_INPUT, "input tokens {context}");
    assert_eq!(output, EXPECTED_OUTPUT, "output tokens {context}");
}

async fn fetch_response(pool: &DbPool, id: Uuid) -> Value {
    sqlx::query("select response from ai_artifacts where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("fetch artifact response")
        .try_get("response")
        .expect("read response column")
}

async fn run_migration_sql(pool: &DbPool, file_name: &str) {
    let dir = std::env::var("ARCHIVIST_MIGRATIONS_DIR").unwrap_or_else(|_| "migrations".to_owned());
    let sql = std::fs::read_to_string(Path::new(&dir).join(file_name))
        .expect("read migration; set ARCHIVIST_MIGRATIONS_DIR to the migrations dir");
    // Trusted input: the SQL is our own migration file, executed verbatim.
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .expect("apply migration SQL");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn typed_token_backfill_counts_every_wire_shape_once() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        eprintln!("DATABASE_URL not set; skipping");
        return;
    };
    seed_all_shapes(&pool).await;

    // Raw-seeded rows carry the 0 column default; the queries no longer parse
    // the response jsonb, so totals must be 0 until the backfill runs.
    assert_eq!(query_totals(&pool).await, (0, 0), "before the backfill");

    run_migration_sql(&pool, TYPED_BACKFILL_MIGRATION).await;
    assert_query_totals(&pool, "after the 0040 backfill").await;

    // Idempotent: recomputing the same values changes nothing.
    run_migration_sql(&pool, TYPED_BACKFILL_MIGRATION).await;
    assert_query_totals(&pool, "after re-running the 0040 backfill").await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn backfill_migration_flattens_legacy_ocr_usage() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        eprintln!("DATABASE_URL not set; skipping");
        return;
    };
    let (legacy, flattened, tokenless) = seed_all_shapes(&pool).await;

    run_migration_sql(&pool, FLATTEN_MIGRATION).await;

    let legacy_response = fetch_response(&pool, legacy).await;
    assert_eq!(
        legacy_response.get("usage"),
        Some(&json!({ "prompt_tokens": 1200, "completion_tokens": 80 })),
        "legacy OCR artifact must gain the aggregated top-level usage"
    );
    assert!(
        legacy_response.get("pages").is_some(),
        "backfill must merge usage in, not replace the response"
    );
    assert_eq!(
        fetch_response(&pool, flattened).await,
        flattened_ocr_response(),
        "post-#259 artifacts already carry usage and must stay untouched"
    );
    assert_eq!(
        fetch_response(&pool, tokenless).await.get("usage"),
        None,
        "artifacts without any per-page counters must not gain a usage block"
    );

    // Idempotent: a second run matches nothing (usage now present).
    run_migration_sql(&pool, FLATTEN_MIGRATION).await;
    assert_eq!(
        fetch_response(&pool, legacy).await.get("usage"),
        Some(&json!({ "prompt_tokens": 1200, "completion_tokens": 80 })),
        "re-running the backfill must not change or double the counters"
    );

    // The typed backfill on top of the flattened responses (the 0037 → 0040
    // upgrade order) reads the top-level usage and must not double count the
    // still-present pages[].
    run_migration_sql(&pool, TYPED_BACKFILL_MIGRATION).await;
    assert_query_totals(&pool, "after 0037 + 0040").await;
}

/// New rows get their typed token columns at insert time, for every wire
/// shape, without any backfill involved.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn insert_ai_artifact_fills_typed_token_columns() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        eprintln!("DATABASE_URL not set; skipping");
        return;
    };
    let run_id = seed_run(&pool).await;
    let job_id: Uuid = sqlx::query_scalar(
        r#"
        insert into jobs (run_id, paperless_document_id, stage, status, payload)
        values ($1, 1, 'ocr', 'running', '{}'::jsonb)
        returning id
        "#,
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("insert job");

    let artifact_id = insert_ai_artifact(
        &pool,
        AiArtifactInput {
            run_id,
            job_id,
            stage: Stage::Ocr,
            provider: "ollama",
            model: "vision-test",
            prompt_id: None,
            input_hash: "hash",
            request: None,
            response: Some(json!({
                "pages": [ { "prompt_eval_count": 7, "eval_count": 3 } ],
                "usage": { "prompt_tokens": 7, "completion_tokens": 3 },
            })),
            normalized_output: None,
            duration_ms: 1200,
            storage_mode: AiArtifactStorageMode::Redacted,
        },
    )
    .await
    .expect("insert_ai_artifact");

    let row = sqlx::query("select input_tokens, output_tokens from ai_artifacts where id = $1")
        .bind(artifact_id)
        .fetch_one(&pool)
        .await
        .expect("fetch typed token columns");
    let input: i64 = row.try_get("input_tokens").expect("input_tokens");
    let output: i64 = row.try_get("output_tokens").expect("output_tokens");
    assert_eq!((input, output), (7, 3), "flattened usage counted once");
}

/// #301: the Statistics default view (`to` = now) and a bare `to` date naming
/// the current day must both include rows created today.
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn statistics_default_view_includes_rows_created_today() {
    let Some((_db_lock, pool)) = fresh_pool().await else {
        eprintln!("DATABASE_URL not set; skipping");
        return;
    };
    let run_id = seed_run(&pool).await;
    seed_artifact(
        &pool,
        run_id,
        "metadata",
        json!({ "usage": { "prompt_tokens": 100, "completion_tokens": 40 } }),
    )
    .await;

    // Default view: `to` falls back to "now", `from` to now - 30 days.
    let now = Utc::now();
    let rows = statistics_usage_rows(&pool, now - Duration::days(30), now, "day")
        .await
        .expect("statistics_usage_rows over the default view");
    assert_eq!(
        rows.iter().map(|row| row.request_count).sum::<i64>(),
        1,
        "a row created today must be inside the default view"
    );

    // Bare `to` date naming today: parsed as the EXCLUSIVE end of that day
    // (next UTC midnight), so today's rows stay visible.
    let end_of_today = Utc.from_utc_datetime(
        &(now.date_naive() + Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .expect("midnight exists"),
    );
    let rows = statistics_usage_rows(&pool, now - Duration::hours(1), end_of_today, "day")
        .await
        .expect("statistics_usage_rows up to the end of today");
    assert_eq!(
        rows.iter().map(|row| row.request_count).sum::<i64>(),
        1,
        "a bare `to` date naming today must cover the whole current day"
    );
}
