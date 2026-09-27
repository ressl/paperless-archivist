//! OCR stage: renders document pages and runs the vision model over them.

use anyhow::{Result, anyhow};
use archivist_ai::{DEFAULT_OCR_SYSTEM_PROMPT, ImageInput, VisionRequest};
use archivist_config::AppConfig;
use archivist_core::{DocumentPatch, RuntimeSettings, Stage, detect_document_language};
use archivist_db::{
    AiArtifactInput, DbPool, JobRecord, get_active_prompt, insert_ai_artifact,
    record_document_language,
};
use archivist_ocr::{normalize_and_validate_ocr_pages, render_document_pages, strip_code_fences};
use archivist_paperless::PaperlessClient;
use serde_json::json;
use tracing::{info, warn};

use crate::apply::handle_patch_result;
use crate::hash_bytes;
use crate::job_supervisor::with_lease_keepalive;
use crate::lease::{job_lease_seconds, lease_keepalive_interval};
use crate::providers::{
    build_vision_client, ollama_vision_num_ctx_for_provider, provider_for_stage,
    run_vision_with_fallback,
};

/// Sum per-page vision token usage across raw provider responses into
/// `(input_tokens, output_tokens)`. Handles both wire shapes: OpenAI/Anthropic
/// (`usage.prompt_tokens`/`input_tokens`, `usage.completion_tokens`/
/// `output_tokens`) and Ollama (top-level `prompt_eval_count`/`eval_count`).
/// Returns `None` when no page reported any tokens. #259.
fn sum_vision_usage(raw_responses: &[serde_json::Value]) -> Option<(i64, i64)> {
    fn field(value: &serde_json::Value, path: &[&str]) -> i64 {
        let mut node = value;
        for key in path {
            match node.get(key) {
                Some(next) => node = next,
                None => return 0,
            }
        }
        node.as_i64()
            .or_else(|| node.as_str().and_then(|s| s.parse::<i64>().ok()))
            .unwrap_or(0)
    }
    let mut input = 0_i64;
    let mut output = 0_i64;
    for page in raw_responses {
        input += field(page, &["usage", "prompt_tokens"])
            + field(page, &["usage", "input_tokens"])
            + field(page, &["prompt_eval_count"]);
        output += field(page, &["usage", "completion_tokens"])
            + field(page, &["usage", "output_tokens"])
            + field(page, &["eval_count"]);
    }
    (input > 0 || output > 0).then_some((input, output))
}

pub(crate) async fn process_ocr(
    pool: &DbPool,
    config: &AppConfig,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
    job: &JobRecord,
    lease_owner: &str,
) -> Result<()> {
    // #413: download (up to 10x the Paperless timeout) plus pdfinfo/pdftoppm
    // rendering (30s + 10s/page) used to run before the first lease renewal
    // and could outlive the 300s default lease, letting another replica
    // reclaim the job mid-render. Keep the lease alive every third of a lease
    // window for the whole pre-page setup.
    let lease_seconds = job_lease_seconds(settings);
    let setup = with_lease_keepalive(
        async {
            // Independent GETs — fetch the document bytes and the document
            // detail concurrently instead of serially.
            let (original, document) = tokio::try_join!(
                paperless.download_original(job.paperless_document_id),
                paperless.get_document(job.paperless_document_id),
            )?;
            let pages = render_document_pages(
                &original,
                document.original_file_name.as_deref(),
                settings
                    .effective_tuning_for_stage(Stage::Ocr)
                    .ocr_page_limit,
            )
            .await?;
            anyhow::Ok((original, pages))
        },
        || archivist_db::bump_job_lease(pool, job.id, lease_owner, lease_seconds),
        lease_keepalive_interval(lease_seconds),
    )
    .await?;
    let Some(setup) = setup else {
        warn!(
            job_id = %job.id,
            document_id = job.paperless_document_id,
            "OCR lease lost during download/render; stopping so a replica isn't double-applied"
        );
        return Ok(());
    };
    let (original, pages) = setup?;
    // The original download bytes (up to the download cap) are only needed for
    // rendering and the artifact input hash. Compute the hash now and drop the
    // bytes so they aren't held in memory for the whole per-page vision loop —
    // that loop already holds the rendered pages plus per-page base64 copies.
    // #283
    let input_hash = hash_bytes(&original);
    drop(original);
    if pages.is_empty() {
        return Err(anyhow!("document rendered zero OCR pages"));
    }
    let page_bytes: usize = pages.iter().map(|page| page.bytes.len()).sum();
    info!(
        job_id = %job.id,
        document_id = job.paperless_document_id,
        pages = pages.len(),
        page_bytes,
        "rendered OCR input pages"
    );

    let provider = provider_for_stage(settings, Stage::Ocr, true)?;
    let prompt = get_active_prompt(pool, Stage::Ocr).await?;
    // Build the vision client once for the whole document: resolves+decrypts
    // the provider secret a single time and keeps one keep-alive/TLS-warm
    // connection pool across every page and the crash fallback.
    let vision_client = build_vision_client(pool, config, &provider).await?;
    let mut texts = Vec::new();
    let mut raw_responses = Vec::new();
    let mut models_used: Vec<String> = Vec::new();
    let mut any_fallback_used = false;
    let mut pages_from_cache: u32 = 0;
    let started = std::time::Instant::now();
    for (index, page) in pages.iter().enumerate() {
        // v1.5.14 (#116): try the OCR page cache before re-running the
        // vision model. Hit key is (document_id, page_index,
        // sha256-of-rendered-bytes). The hash captures both the
        // rendering config and the document content, so re-renders with
        // identical config produce identical hashes and cached text is
        // safe to reuse. Re-renders with different config (e.g. higher
        // DPI) get a new hash and the LLM runs as before.
        let page_hash = hash_bytes(&page.bytes);
        if let Some(cached_text) = archivist_db::lookup_ocr_page_cache(
            pool,
            job.paperless_document_id,
            index as i32,
            &page_hash,
        )
        .await?
        {
            pages_from_cache += 1;
            info!(
                document_id = job.paperless_document_id,
                page_index = index,
                "served OCR page from cache, skipped vision call"
            );
            models_used.push("(cache)".to_owned());
            texts.push(cached_text);
            raw_responses.push(json!({"cached": true}));
            continue;
        }

        let page_prompt = prompt
            .as_ref()
            .map(|prompt| {
                format!(
                    "{}\n\nPage {}: transcribe exactly and return only OCR text.",
                    prompt.content,
                    index + 1
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "{}\n\nPage {}: transcribe exactly and return only OCR text.",
                    DEFAULT_OCR_SYSTEM_PROMPT,
                    index + 1
                )
            });
        // Wire the runtime-configured Ollama context window into the vision
        // payload. This is the GGML_ASSERT crash fix (ollama/ollama#14401):
        // glm-ocr and similar vision models expand a single page into more
        // tokens than Ollama's 4096-token default holds, which kills the
        // runner with `GGML_ASSERT(a->ne[2] * 4 == b->ne[0])`. The default of
        // 16384 covers realistic single-page renders; operators can raise it
        // for huge multi-page documents or lower it on small Ollama hosts.
        // Remote providers (OpenAI / Anthropic / OpenAI-compatible) ignore
        // this field — see `build_ollama_vision_payload`.
        let request = VisionRequest {
            model: provider.model.clone(),
            temperature: 0.0,
            num_ctx: ollama_vision_num_ctx_for_provider(
                &provider,
                settings
                    .effective_tuning_for_stage(Stage::Ocr)
                    .vision_num_ctx,
            ),
            reasoning_effort: Some(provider.reasoning_effort),
            max_output_tokens: provider.max_output_tokens,
            prompt: page_prompt,
            images: vec![ImageInput {
                mime_type: page.mime_type.clone(),
                bytes: page.bytes.clone(),
            }],
        };
        let page_started = std::time::Instant::now();
        let Some((response, model_used, fallback_used)) = run_vision_with_fallback(
            pool,
            config,
            &vision_client,
            &provider,
            settings,
            job,
            lease_owner,
            index,
            request,
        )
        .await?
        else {
            return Ok(());
        };
        // Progress breadcrumb for the slow per-page vision calls — without this
        // the worker went silent for the whole OCR duration (only cache hits
        // logged), so a document stuck mid-OCR was invisible.
        info!(
            document_id = job.paperless_document_id,
            page_index = index,
            model = %model_used,
            fallback_used,
            duration_ms = page_started.elapsed().as_millis() as u64,
            "ocr page complete"
        );
        any_fallback_used |= fallback_used;

        // Strip fences before caching, but intentionally keep provider layout
        // markup raw. Document-level normalization runs after page assembly so
        // parser fixes also apply to cached pages and entities are never decoded twice.
        let page_text = strip_code_fences(&response.text);

        // Cache the successful page-level OCR so a future retry / re-trigger
        // doesn't pay for the vision call again. Cache write is best-effort:
        // a failure here is logged but does not fail the OCR job.
        if let Err(cache_error) = archivist_db::upsert_ocr_page_cache(
            pool,
            job.paperless_document_id,
            index as i32,
            &page_hash,
            &page_text,
            Some(&provider.name),
            Some(&model_used),
        )
        .await
        {
            warn!(
                document_id = job.paperless_document_id,
                page_index = index,
                error = %cache_error,
                "OCR page-cache write failed; not blocking the job"
            );
        }

        models_used.push(model_used);
        texts.push(page_text);
        raw_responses.push(response.raw_response);

        // Heartbeat the lease after each page. Multi-page vision OCR can run
        // far longer than the lease window `claim_jobs` grants, so without
        // this a second replica would reclaim the "stale" lease and re-OCR
        // the same document concurrently. Push `lease_until` forward by the
        // same window; if the bump finds no matching row our lease was lost
        // (another replica took over), so stop instead of double-applying.
        if !archivist_db::bump_job_lease(pool, job.id, lease_owner, job_lease_seconds(settings))
            .await?
        {
            warn!(
                job_id = %job.id,
                document_id = job.paperless_document_id,
                page_index = index,
                "OCR lease lost mid-document; stopping so a replica isn't double-applied"
            );
            return Ok(());
        }
    }
    let text = normalize_and_validate_ocr_pages(&texts, settings.ocr.min_chars)?;
    let language_detection = detect_document_language(&text);
    record_document_language(
        pool,
        job.paperless_document_id,
        &language_detection,
        Some(job.run_id),
        Some(job.id),
        "worker",
    )
    .await?;

    insert_ai_artifact(
        pool,
        AiArtifactInput {
            run_id: job.run_id,
            job_id: job.id,
            stage: Stage::Ocr,
            provider: &provider.name,
            model: &provider.model,
            prompt_id: prompt.as_ref().map(|prompt| prompt.id),
            input_hash: &input_hash,
            request: None,
            response: Some({
                let mut response = json!({ "pages": raw_responses });
                // Flatten per-page token usage to a top-level `usage` block so
                // the OCR/vision stage — usually the most token-heavy — is
                // counted by provider_usage / statistics / cost queries, which
                // only read top-level token fields. Top-level also survives
                // metadata-only storage (which keeps `usage`). #259.
                if let Some((input, output)) = sum_vision_usage(&raw_responses)
                    && let Some(object) = response.as_object_mut()
                {
                    object.insert(
                        "usage".to_owned(),
                        json!({ "prompt_tokens": input, "completion_tokens": output }),
                    );
                }
                response
            }),
            normalized_output: Some(json!({
                "content_chars": text.chars().count(),
                "language": language_detection.language,
                "language_confidence": language_detection.confidence,
                "language_source": language_detection.source,
                "models_used_per_page": models_used,
                "vision_model_fallback_used": any_fallback_used,
                "pages_from_cache": pages_from_cache,
            })),
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
            storage_mode: settings.security.ai_artifact_storage,
        },
    )
    .await?;

    // v1.5.14 (#117): record sha256(ocr_text) on the inventory row so the
    // metadata stage can dedup against earlier documents with identical
    // content. Best-effort write — a failure here doesn't fail OCR.
    let content_hash = hash_bytes(text.as_bytes());
    if let Err(error) = archivist_db::set_document_inventory_ocr_content_hash(
        pool,
        job.paperless_document_id,
        &content_hash,
    )
    .await
    {
        warn!(
            document_id = job.paperless_document_id,
            error = %error,
            "set_document_inventory_ocr_content_hash failed; dedup will not apply"
        );
    }

    // #217: persist the OCR body locally so chat search can full-text
    // rank against it. NUL bytes are stripped because Postgres `text`
    // cannot store them; the body is otherwise the same text sent to
    // Paperless. Best-effort write — a failure here doesn't fail OCR, it
    // only means this document won't surface via body FTS until re-OCR'd.
    let ocr_body = text.replace('\0', "");
    if let Err(error) =
        archivist_db::set_document_inventory_ocr_body(pool, job.paperless_document_id, &ocr_body)
            .await
    {
        warn!(
            document_id = job.paperless_document_id,
            error = %error,
            "set_document_inventory_ocr_body failed; body full-text search will not apply"
        );
    }

    let patch = DocumentPatch {
        content: Some(text),
        title: None,
        tags: None,
        correspondent: None,
        document_type: None,
        created: None,
        custom_fields: None,
    };
    handle_patch_result(
        pool,
        paperless,
        settings,
        job,
        patch,
        Vec::new(),
        None,
        lease_owner,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_vision_usage_handles_both_wire_shapes() {
        // OpenAI/Anthropic usage + Ollama top-level counters across pages.
        let pages = vec![
            json!({ "usage": { "prompt_tokens": 100, "completion_tokens": 40 } }),
            json!({ "prompt_eval_count": 7, "eval_count": 3 }),
            json!({ "usage": { "input_tokens": 5, "output_tokens": 2 } }),
        ];
        assert_eq!(sum_vision_usage(&pages), Some((112, 45)));
    }

    #[test]
    fn sum_vision_usage_returns_none_without_tokens() {
        let pages = vec![json!({ "response": "text only" }), json!({})];
        assert_eq!(sum_vision_usage(&pages), None);
    }
}
