//! DB-required regression tests for the usage-statistics queries.
//!
//! Releases up to v1.11.2 redacted numeric `usage.*` token counts in stored
//! AI artifacts to the string "[REDACTED]"; the aggregate queries then failed
//! with `22P02 invalid input syntax for type bigint` as soon as one such row
//! was in range. These tests prove the queries tolerate legacy rows and still
//! count numeric usage from healthy ones.
//!
//! Marked `#[ignore]` so the default `cargo test` run does not require a live
//! PostgreSQL instance. To exercise them locally, run
//! `DATABASE_URL=postgres://... cargo test -p archivist-db --test usage_stats_redaction -- --ignored`.

use archivist_core::DashboardRange;
use archivist_db::{DbPool, connect, migrate, provider_bucket_entries, statistics_usage_rows};
use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::Executor;
use uuid::Uuid;

async fn fresh_pool() -> Option<DbPool> {
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
    Some(pool)
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

async fn seed_artifact(pool: &DbPool, run_id: Uuid, response: serde_json::Value) {
    sqlx::query(
        r#"
        insert into ai_artifacts (run_id, stage, provider, model, input_hash, response, duration_ms)
        values ($1, 'metadata', 'openai', 'gpt-test', 'hash', $2, 1200)
        "#,
    )
    .bind(run_id)
    .bind(response)
    .execute(pool)
    .await
    .expect("insert artifact");
}

#[tokio::test]
#[ignore]
async fn usage_queries_tolerate_legacy_redacted_strings() {
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL not set; skipping");
        return;
    };
    let run_id = seed_run(&pool).await;

    // Legacy row: numeric counts destroyed by the pre-fix redaction.
    seed_artifact(
        &pool,
        run_id,
        json!({ "usage": { "prompt_tokens": "[REDACTED]", "completion_tokens": "[REDACTED]" } }),
    )
    .await;
    // Healthy OpenAI-shaped row.
    seed_artifact(
        &pool,
        run_id,
        json!({ "usage": { "prompt_tokens": 100, "completion_tokens": 40 } }),
    )
    .await;
    // Healthy Ollama-shaped row (top-level counters).
    seed_artifact(
        &pool,
        run_id,
        json!({ "prompt_eval_count": 7, "eval_count": 3 }),
    )
    .await;

    let from = Utc::now() - Duration::hours(1);
    let to = Utc::now() + Duration::hours(1);

    let rows = statistics_usage_rows(&pool, from, to, "day")
        .await
        .expect("statistics_usage_rows must tolerate legacy redacted rows");
    let requests: i64 = rows.iter().map(|row| row.request_count).sum();
    let input: i64 = rows.iter().map(|row| row.input_tokens).sum();
    let output: i64 = rows.iter().map(|row| row.output_tokens).sum();
    assert_eq!(requests, 3);
    assert_eq!(input, 107);
    assert_eq!(output, 43);

    let buckets = provider_bucket_entries(&pool, from, to, DashboardRange::Last24Hours)
        .await
        .expect("provider_bucket_entries must tolerate legacy redacted rows");
    let bucket_input: i64 = buckets.iter().map(|entry| entry.input_tokens).sum();
    let bucket_output: i64 = buckets.iter().map(|entry| entry.output_tokens).sum();
    assert_eq!(bucket_input, 107);
    assert_eq!(bucket_output, 43);
}
