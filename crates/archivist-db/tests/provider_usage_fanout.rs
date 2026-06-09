//! DB-required integration test: provider_usage must not inflate
//! request/token/latency stats by the number of feedback events per job (the
//! join fan-out). #260.
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_db::{DbPool, connect, migrate, provider_usage};
use chrono::{Duration, Utc};
use sqlx::Executor;
use uuid::Uuid;

async fn fresh_pool() -> Option<DbPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&url, 10).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        r#"truncate ai_artifacts, audit_events, jobs, pipeline_runs restart identity cascade;"#,
    )
    .await
    .expect("truncate test tables");
    Some(pool)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn provider_usage_does_not_fan_out_on_feedback() {
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let run_id: Uuid = sqlx::query_scalar(
        r#"
        insert into pipeline_runs (paperless_document_id, mode, trigger_tag, status, stages)
        values (1, 'full_auto', 'ai-process', 'running', '[]'::jsonb)
        returning id
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("insert run");
    let job_id: Uuid = sqlx::query_scalar(
        r#"
        insert into jobs (run_id, paperless_document_id, stage, status)
        values ($1, 1, 'metadata', 'running')
        returning id
        "#,
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("insert job");

    // One artifact with 50/20 tokens and a known duration.
    sqlx::query(
        r#"
        insert into ai_artifacts (run_id, job_id, stage, provider, model, input_hash, response, duration_ms)
        values ($1, $2, 'metadata', 'openai', 'gpt-test', 'hash',
                '{"usage": {"prompt_tokens": 50, "completion_tokens": 20}}'::jsonb, 1000)
        "#,
    )
    .bind(run_id)
    .bind(job_id)
    .execute(&pool)
    .await
    .expect("insert artifact");

    // Three feedback events for the same job (the autopilot drain can emit
    // several review.approved per job). Pre-fix these tripled the cell.
    for event_type in ["review.approved", "review.approved", "review.rejected"] {
        sqlx::query(
            r#"
            insert into audit_events (id, job_id, event_type, actor_type, outcome, created_at, chain_position)
            values ($1, $2, $3, 'worker', 'success', now(), nextval('audit_events_chain_position_seq'))
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(job_id)
        .bind(event_type)
        .execute(&pool)
        .await
        .expect("insert feedback");
    }

    let stats = provider_usage(&pool, Utc::now() - Duration::hours(1))
        .await
        .expect("provider_usage");
    assert_eq!(stats.len(), 1);
    let cell = &stats[0];
    // The artifact is counted ONCE despite three feedback events.
    assert_eq!(cell.request_count, 1, "request_count must not fan out");
    assert_eq!(cell.input_tokens, 50, "input tokens must not fan out");
    assert_eq!(cell.output_tokens, 20, "output tokens must not fan out");
    assert_eq!(cell.avg_duration_ms, 1000.0, "latency must not fan out");
    // Feedback is still counted in full.
    assert_eq!(cell.feedback_count, 3);
    assert_eq!(cell.positive_feedback, 2);
    assert_eq!(cell.negative_feedback, 1);
}
