//! Paperless metadata / inventory sync into the local mirror.

use std::collections::HashMap;

use anyhow::Result;
use archivist_core::RuntimeSettings;
use archivist_db::{
    DbPool, paperless_sync_cursor, update_paperless_sync_cursor, upsert_inventory_item,
    upsert_paperless_custom_field, upsert_paperless_named_entity, upsert_paperless_tag,
};
use archivist_paperless::{PaperlessClient, PaperlessDocumentSummary, PaperlessTag};
use chrono::{Duration as ChronoDuration, Utc};

use crate::drain::ensure_tag_cached;

pub(crate) struct PaperlessSyncSnapshot {
    pub(crate) tags: Vec<PaperlessTag>,
    pub(crate) documents: Vec<PaperlessDocumentSummary>,
}

pub(crate) async fn sync_metadata(
    pool: &DbPool,
    paperless: &PaperlessClient,
    settings: &RuntimeSettings,
) -> Result<PaperlessSyncSnapshot> {
    let archive_name = settings.paperless.active_archive.clone();
    let sync_started_at = Utc::now();
    let mut tags = paperless.list_tags().await?;
    // Reuse the already-fetched catalog via `ensure_tag_cached`, which only
    // calls Paperless when a workflow tag is genuinely missing. The previous
    // unconditional `ensure_tag` per workflow tag re-fetched the entire tag
    // catalog every iteration — O(workflow_tags × all_tags). With a few
    // thousand tags that alone overran the 300s trigger-poll timeout, so the
    // poll never completed and document ingestion stalled entirely.
    for workflow_tag in settings.workflow.tags.all() {
        ensure_tag_cached(paperless, &mut tags, workflow_tag).await?;
    }
    // Delta sync: when enabled and a prior cursor exists, fetch only documents
    // modified since the cursor (minus an overlap window to absorb clock skew)
    // instead of the full catalog — mirroring the API sync path. No cursor
    // (first run) or a delta error falls back to a full list. Tags,
    // correspondents and types stay full; they are small relative to documents.
    let cursor = paperless_sync_cursor(pool, &archive_name).await?;
    let delta_cursor = cursor.map(|cursor| {
        cursor - ChronoDuration::minutes(settings.paperless.delta_sync_overlap_minutes)
    });
    // These four catalog fetches are independent GETs against Paperless; run
    // them concurrently rather than serially. The tag list above must stay
    // sequential because `ensure_tag_cached` mutates it in place. custom_fields
    // keeps its best-effort `unwrap_or_default` semantics inside the join.
    let (correspondents, document_types, custom_fields, (sync_mode, documents)) = tokio::try_join!(
        paperless.list_correspondents(),
        paperless.list_document_types(),
        async { anyhow::Ok(paperless.list_custom_fields().await.unwrap_or_default()) },
        async {
            if settings.paperless.delta_sync_enabled {
                if let Some(cursor) = delta_cursor {
                    match paperless
                        .list_documents_modified_since(&cursor.to_rfc3339())
                        .await
                    {
                        Ok(documents) => anyhow::Ok(("delta", documents)),
                        Err(_) => anyhow::Ok((
                            "full_after_delta_error",
                            paperless.list_documents().await?,
                        )),
                    }
                } else {
                    anyhow::Ok(("full_initial", paperless.list_documents().await?))
                }
            } else {
                anyhow::Ok(("full", paperless.list_documents().await?))
            }
        },
    )?;

    let mut tx = pool.begin().await?;
    for tag in &tags {
        upsert_paperless_tag(
            &mut tx,
            tag.id,
            &tag.name,
            tag.slug.as_deref(),
            tag.color.as_deref(),
            settings.workflow.tags.is_workflow_tag(&tag.name),
        )
        .await?;
    }
    for entity in &correspondents {
        upsert_paperless_named_entity(&mut tx, "paperless_correspondents", entity.id, &entity.name)
            .await?;
    }
    for entity in &document_types {
        upsert_paperless_named_entity(&mut tx, "paperless_document_types", entity.id, &entity.name)
            .await?;
    }
    for field in &custom_fields {
        upsert_paperless_custom_field(&mut tx, field.id, &field.name, field.data_type.as_deref())
            .await?;
    }
    // #408: the catalog upserts above are small; commit them now instead of
    // holding one transaction (and every inventory row lock) across the whole
    // document sync, where `claim_jobs`/`complete_job`/`fail_job` queued up
    // behind it.
    tx.commit().await?;
    // O(1) id→name lookups: building this map once avoids the previous
    // O(documents × tags) nested linear scan, which was pure CPU burned inside
    // the sync transaction on instances with many tags/documents.
    let tag_names_by_id: HashMap<i32, &str> =
        tags.iter().map(|tag| (tag.id, tag.name.as_str())).collect();
    // #408: upsert in short, id-ordered batches — each batch locks at most
    // SYNC_UPSERT_BATCH_SIZE inventory rows, always in ascending id order (the
    // same order `claim_jobs` locks them in), so there is no long-held lock
    // set and no lock-order deadlock with the claim path.
    let mut ordered: Vec<&PaperlessDocumentSummary> = documents.iter().collect();
    ordered.sort_unstable_by_key(|document| document.id);
    for batch in ordered.chunks(SYNC_UPSERT_BATCH_SIZE) {
        let mut tx = pool.begin().await?;
        upsert_inventory_batch(&mut tx, batch, &tag_names_by_id, settings).await?;
        tx.commit().await?;
    }
    // The cursor only advances after every batch landed; a crash mid-sync
    // re-covers the same window on the next run.
    let mut tx = pool.begin().await?;
    update_paperless_sync_cursor(&mut tx, &archive_name, sync_mode, sync_started_at).await?;
    tx.commit().await?;
    Ok(PaperlessSyncSnapshot { tags, documents })
}

/// Inventory rows upserted per sync transaction. #408
const SYNC_UPSERT_BATCH_SIZE: usize = 500;

async fn upsert_inventory_batch(
    tx: &mut archivist_db::DbTransaction<'_>,
    documents: &[&PaperlessDocumentSummary],
    tag_names_by_id: &HashMap<i32, &str>,
    settings: &RuntimeSettings,
) -> Result<()> {
    for document in documents {
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tag_names_by_id.get(id).copied())
            .map(|name| name.to_owned())
            .collect::<Vec<_>>();
        upsert_inventory_item(
            tx,
            &archivist_db::InventoryUpsert {
                paperless_document_id: document.id,
                title: document.title.clone(),
                original_file_name: document.original_file_name.clone(),
                current_tags: tag_names.clone(),
                current_tag_ids: document.tags.clone(),
                correspondent_id: document.correspondent,
                document_type_id: document.document_type,
                document_date: archivist_db::parse_paperless_document_date(
                    document.created.as_deref(),
                ),
                paperless_modified_at: archivist_db::parse_paperless_modified_at(
                    document.modified.as_deref(),
                ),
                has_ocr_completion_tag: tag_names
                    .iter()
                    .any(|tag| tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_ocr)),
                has_tagging_completion_tag: tag_names.iter().any(|tag| {
                    tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_tagging)
                }),
                has_full_completion_tag: tag_names.iter().any(|tag| {
                    tag.eq_ignore_ascii_case(&settings.workflow.tags.completion_processed)
                }),
            },
        )
        .await?;
    }
    Ok(())
}
