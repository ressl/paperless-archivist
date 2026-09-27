//! AI artifacts, OCR page cache and metadata dedup lookups.

use super::*;

pub struct AiArtifactInput<'a> {
    pub run_id: Uuid,
    pub job_id: Uuid,
    pub stage: Stage,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_id: Option<Uuid>,
    pub input_hash: &'a str,
    pub request: Option<Value>,
    pub response: Option<Value>,
    pub normalized_output: Option<Value>,
    pub duration_ms: i32,
    pub storage_mode: AiArtifactStorageMode,
}

/// Look up a cached OCR result for a (document, page_index, page_hash)
/// triple. Returns the cached text when present. Added in v1.5.14 to
/// short-circuit expensive vision-model calls when the exact same
/// rendered page has been transcribed before.
pub async fn lookup_ocr_page_cache(
    pool: &DbPool,
    paperless_document_id: i32,
    page_index: i32,
    page_hash: &str,
) -> Result<Option<String>> {
    let row = sqlx::query(
        r#"
        select ocr_text from ocr_page_cache
         where paperless_document_id = $1
           and page_index = $2
           and page_hash = $3
         limit 1
        "#,
    )
    .bind(paperless_document_id)
    .bind(page_index)
    .bind(page_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|r| r.try_get::<String, _>("ocr_text"))
        .transpose()?)
}

/// Insert (or update) an OCR-page-cache row. Idempotent on the
/// (document, page_index, page_hash) primary key — the second call for
/// the same triple overwrites the cached text and bumps `created_at`.
pub async fn upsert_ocr_page_cache(
    pool: &DbPool,
    paperless_document_id: i32,
    page_index: i32,
    page_hash: &str,
    ocr_text: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into ocr_page_cache
            (paperless_document_id, page_index, page_hash, ocr_text, provider, model)
        values ($1, $2, $3, $4, $5, $6)
        on conflict (paperless_document_id, page_index, page_hash)
        do update set
            ocr_text = excluded.ocr_text,
            provider = excluded.provider,
            model = excluded.model,
            created_at = now()
        "#,
    )
    .bind(paperless_document_id)
    .bind(page_index)
    .bind(page_hash)
    .bind(ocr_text)
    .bind(provider)
    .bind(model)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record the OCR-content-hash on `document_inventory`. Used as the
/// key for the content-hash dedup helper below. Idempotent.
pub async fn set_document_inventory_ocr_content_hash(
    pool: &DbPool,
    paperless_document_id: i32,
    ocr_content_hash: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        update document_inventory
           set ocr_content_hash = $2,
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .bind(ocr_content_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist the (sanitized) OCR body on `document_inventory` so it can be
/// indexed for full-text retrieval in chat search (#217). The companion
/// generated `ocr_body_tsv` column is recomputed by Postgres on write.
/// Idempotent; best-effort from the worker's point of view (a failure
/// here does not fail the OCR stage).
pub async fn set_document_inventory_ocr_body(
    pool: &DbPool,
    paperless_document_id: i32,
    ocr_body: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        update document_inventory
           set ocr_body = $2,
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .bind(ocr_body)
    .execute(pool)
    .await?;
    Ok(())
}

/// Find a recent document whose OCR text hash matches and whose
/// metadata stage has already settled (succeeded). Used by the
/// metadata stage to short-circuit a re-extraction when the same
/// document content has been processed before. Returns the dedup
/// source's `paperless_document_id` and the most recent succeeded
/// metadata `ai_artifacts.normalized` payload so the caller can clone
/// the patch.
pub async fn find_metadata_dedup_source(
    pool: &DbPool,
    current_document_id: i32,
    ocr_content_hash: &str,
) -> Result<Option<(i32, Value)>> {
    let row = sqlx::query(
        r#"
        select di.paperless_document_id as source_id,
               aa.normalized_output as metadata_payload
          from document_inventory di
          join pipeline_runs pr
            on pr.paperless_document_id = di.paperless_document_id
          join ai_artifacts aa
            on aa.run_id = pr.id
           and aa.stage = 'metadata'
         where di.ocr_content_hash = $1
           and di.paperless_document_id <> $2
           and di.metadata_status = 'succeeded'
         order by aa.created_at desc
         limit 1
        "#,
    )
    .bind(ocr_content_hash)
    .bind(current_document_id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| -> Result<(i32, Value)> {
        Ok((r.try_get("source_id")?, r.try_get("metadata_payload")?))
    })
    .transpose()
}

/// Extract the typed token counters persisted on `ai_artifacts.input_tokens`
/// / `output_tokens` (migration 0040) from a raw provider response.
///
/// Mirrors the SQL extraction the usage queries performed before 0040 (and
/// that the 0040 backfill still uses): OpenAI/Anthropic-style
/// `usage.prompt_tokens`/`input_tokens` + `usage.completion_tokens`/
/// `output_tokens`, Ollama top-level `prompt_eval_count`/`eval_count`, and
/// the per-page `pages[]` fallback for OCR responses without a flattened
/// top-level `usage` block. Only plain non-negative integers count — redacted
/// strings and summary objects contribute 0, like the SQL regexp guard.
///
/// Runs on the response BEFORE storage-mode preparation, so the counters stay
/// correct even when `metadata_only` storage strips the Ollama top-level
/// counters from the persisted jsonb.
pub(crate) fn ai_response_token_usage(response: Option<&Value>) -> (i64, i64) {
    fn counter(value: &Value, path: &[&str]) -> i64 {
        let mut current = value;
        for key in path {
            match current.get(key) {
                Some(next) => current = next,
                None => return 0,
            }
        }
        current.as_i64().filter(|count| *count >= 0).unwrap_or(0)
    }
    fn tokens(value: &Value) -> (i64, i64) {
        (
            counter(value, &["usage", "prompt_tokens"])
                + counter(value, &["usage", "input_tokens"])
                + counter(value, &["prompt_eval_count"]),
            counter(value, &["usage", "completion_tokens"])
                + counter(value, &["usage", "output_tokens"])
                + counter(value, &["eval_count"]),
        )
    }

    let Some(response) = response else {
        return (0, 0);
    };
    let (mut input, mut output) = tokens(response);
    // Pages fallback exactly when no top-level `usage` KEY exists (a JSON
    // null `usage` suppresses it, matching the SQL `-> 'usage' is null`
    // semantics of the 0040 backfill), so flattened post-#259 OCR responses
    // are never double counted.
    if response.get("usage").is_none()
        && let Some(pages) = response.get("pages").and_then(Value::as_array)
    {
        for page in pages {
            let (page_input, page_output) = tokens(page);
            input += page_input;
            output += page_output;
        }
    }
    (input, output)
}

pub async fn insert_ai_artifact(pool: &DbPool, input: AiArtifactInput<'_>) -> Result<Uuid> {
    // Token counters are read off the raw response before the storage-mode
    // redaction/summarization so they reflect what the provider reported.
    let (input_tokens, output_tokens) = ai_response_token_usage(input.response.as_ref());
    let request = prepare_ai_artifact_value(input.request, input.storage_mode);
    let response = prepare_ai_artifact_value(input.response, input.storage_mode);
    let normalized_output = input.normalized_output.map(|mut value| {
        replace_nul_in_json(&mut value);
        value
    });

    let id = sqlx::query(
        r#"
        insert into ai_artifacts (
          run_id, job_id, stage, provider, model, prompt_id, input_hash, request, response, normalized_output, duration_ms,
          input_tokens, output_tokens
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        returning id
        "#,
    )
    .bind(input.run_id)
    .bind(input.job_id)
    .bind(input.stage.to_string())
    .bind(input.provider)
    .bind(input.model)
    .bind(input.prompt_id)
    .bind(input.input_hash)
    .bind(request)
    .bind(response)
    .bind(normalized_output)
    .bind(input.duration_ms)
    .bind(input_tokens)
    .bind(output_tokens)
    .fetch_one(pool)
    .await?
    .try_get("id")?;
    Ok(id)
}

/// Replace every U+0000 in JSON strings and object keys with U+FFFD, which
/// PostgreSQL `jsonb`/`text` can store. #415
pub fn replace_nul_in_json(value: &mut Value) {
    const NUL: char = '\u{0}';
    const REPLACEMENT: &str = "\u{FFFD}";
    match value {
        Value::String(text) if text.contains(NUL) => {
            *text = text.replace(NUL, REPLACEMENT);
        }
        Value::Array(items) => items.iter_mut().for_each(replace_nul_in_json),
        Value::Object(map) => {
            if map.keys().any(|key| key.contains(NUL)) {
                let entries = std::mem::take(map);
                *map = entries
                    .into_iter()
                    .map(|(key, value)| (key.replace(NUL, REPLACEMENT), value))
                    .collect();
            }
            map.values_mut().for_each(replace_nul_in_json);
        }
        _ => {}
    }
}

pub(crate) fn prepare_ai_artifact_value(
    value: Option<Value>,
    storage_mode: AiArtifactStorageMode,
) -> Option<Value> {
    let mut value = value?;
    redact_sensitive_json(&mut value);
    // #415: PostgreSQL rejects U+0000 in jsonb; model output occasionally
    // contains it, which failed the insert in `Full` mode.
    replace_nul_in_json(&mut value);
    match storage_mode {
        AiArtifactStorageMode::Full => Some(value),
        AiArtifactStorageMode::Redacted => {
            redact_ai_artifact_content(&mut value);
            Some(value)
        }
        AiArtifactStorageMode::MetadataOnly => Some(ai_artifact_metadata_only(&value)),
    }
}

fn redact_ai_artifact_content(value: &mut Value) {
    const CONTENT_KEYS: &[&str] = &[
        "content",
        "text",
        "prompt",
        "system_prompt",
        "user_prompt",
        "response",
        "images",
        "image",
        "bytes",
        "b64_json",
        "base64",
    ];

    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                let content_key = CONTENT_KEYS
                    .iter()
                    .any(|needle| key.to_ascii_lowercase().contains(needle));
                // `prompt_tokens` / `prompt_eval_count` match the "prompt"
                // substring but are numeric counters, not document content —
                // summarizing them away destroys token statistics.
                if content_key && !matches!(nested, Value::Number(_) | Value::Bool(_)) {
                    *nested = ai_artifact_redaction_summary(nested);
                } else {
                    redact_ai_artifact_content(nested);
                }
            }
        }
        Value::Array(values) => {
            for nested in values {
                redact_ai_artifact_content(nested);
            }
        }
        _ => {}
    }
}

fn ai_artifact_redaction_summary(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({
            "redacted": true,
            "kind": "text",
            "sha256": short_hash(text),
            "chars": text.chars().count()
        }),
        Value::Array(items) => json!({
            "redacted": true,
            "kind": "array",
            "items": items.len()
        }),
        Value::Object(map) => json!({
            "redacted": true,
            "kind": "object",
            "keys": map.len()
        }),
        Value::Null => Value::Null,
        other => json!({
            "redacted": true,
            "kind": "scalar",
            "sha256": short_hash(&other.to_string())
        }),
    }
}

fn ai_artifact_metadata_only(value: &Value) -> Value {
    let mut metadata = json!({
        "storage": "metadata_only",
        "sha256": short_hash(&value.to_string()),
        "json_bytes": value.to_string().len()
    });
    if let (Some(target), Value::Object(source)) = (metadata.as_object_mut(), value) {
        for key in [
            "model",
            "provider",
            "stage",
            "usage",
            "options",
            "done_reason",
        ] {
            if let Some(value) = source.get(key) {
                target.insert(key.to_owned(), value.clone());
            }
        }
    }
    metadata
}
