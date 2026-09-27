//! DB-required regression test for #415: model output containing U+0000 must
//! be storable in the `Full` artifact storage mode (PostgreSQL rejects NUL in
//! jsonb).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::{AiArtifactStorageMode, Stage};
use archivist_db::{AiArtifactInput, connect, insert_ai_artifact, migrate};
use serde_json::json;
use sqlx::{Executor, Row};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn full_mode_artifact_with_nul_characters_is_stored() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&url, 5).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");
    pool.execute(
        "truncate ai_artifacts, jobs, pipeline_runs, audit_events restart identity cascade;",
    )
    .await
    .expect("truncate test tables");
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
        "insert into jobs (run_id, paperless_document_id, stage, status) values ($1, 1, 'ocr', 'running') returning id",
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
            request: Some(json!({ "prompt": "transcribe\u{0}" })),
            response: Some(json!({ "text": "Rechnung\u{0}4711", "key\u{0}": ["a\u{0}b"] })),
            normalized_output: Some(json!({ "text": "Rechnung\u{0}4711" })),
            duration_ms: 10,
            storage_mode: AiArtifactStorageMode::Full,
        },
    )
    .await
    .expect("artifact with NUL characters is stored");

    let row = sqlx::query("select response, normalized_output from ai_artifacts where id = $1")
        .bind(artifact_id)
        .fetch_one(&pool)
        .await
        .expect("stored artifact");
    let response: serde_json::Value = row.get("response");
    assert_eq!(response["text"], "Rechnung\u{FFFD}4711");
    assert_eq!(response["key\u{FFFD}"], json!(["a\u{FFFD}b"]));
    let normalized: serde_json::Value = row.get("normalized_output");
    assert_eq!(normalized["text"], "Rechnung\u{FFFD}4711");
}
