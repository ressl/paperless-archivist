//! Paperless sync, consistency check, completion-tag reconciliation and metadata options.

use crate::*;

#[tracing::instrument(
    skip(state, auth),
    fields(user_id = tracing::field::Empty)
)]
pub(crate) async fn sync_paperless(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    let summary = sync_paperless_inventory(&state.pool, &client, &settings).await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "paperless.sync".to_owned(),
            actor_type: auth.0.actor_type,
            actor_id: auth.0.actor_id,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(summary.clone()),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    info!(?summary, "paperless sync completed");
    Ok(Json(summary))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConsistencyInventoryRow {
    pub(crate) title: Option<String>,
    pub(crate) current_tag_ids: Vec<i32>,
    pub(crate) correspondent: Option<i32>,
    pub(crate) document_type: Option<i32>,
    pub(crate) document_date: Option<chrono::NaiveDate>,
}

/// Load the inventory columns compared by the consistency check.
/// `document_date` has been a typed `date` since migration 0043; decoding it
/// as text failed for every non-null row and turned the endpoint into a
/// permanent 500. #386
pub(crate) async fn load_consistency_inventory(
    pool: &DbPool,
) -> anyhow::Result<HashMap<i32, ConsistencyInventoryRow>> {
    let rows = sqlx::query(
        r#"
        select paperless_document_id, title, current_tag_ids, correspondent_id,
               document_type_id, document_date
          from document_inventory
        "#,
    )
    .fetch_all(pool)
    .await?;
    let mut inventory = HashMap::with_capacity(rows.len());
    for row in rows {
        inventory.insert(
            row.try_get::<i32, _>("paperless_document_id")?,
            ConsistencyInventoryRow {
                title: row.try_get("title")?,
                current_tag_ids: row.try_get("current_tag_ids")?,
                correspondent: row.try_get("correspondent_id")?,
                document_type: row.try_get("document_type_id")?,
                document_date: row.try_get("document_date")?,
            },
        );
    }
    Ok(inventory)
}

#[tracing::instrument(
    skip(state, auth),
    fields(user_id = tracing::field::Empty, documents_checked = tracing::field::Empty)
)]
pub(crate) async fn paperless_consistency(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let settings = get_runtime_settings(&state.pool).await?;
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    // Only the compared fields are needed; never pull every document's full
    // OCR text into memory for a consistency check. #386
    let documents = client.list_documents_for_consistency().await?;
    let inventory = load_consistency_inventory(&state.pool).await?;

    let mut missing_local = Vec::new();
    let mut mismatches = Vec::new();
    let seen_remote = documents
        .iter()
        .map(|document| document.id)
        .collect::<HashSet<_>>();
    for document in &documents {
        let Some(local) = inventory.get(&document.id) else {
            missing_local.push(document.id);
            continue;
        };
        let mut fields = Vec::new();
        if local.title.as_deref() != document.title.as_deref() {
            fields.push("title");
        }
        if local.correspondent != document.correspondent {
            fields.push("correspondent");
        }
        if local.document_type != document.document_type {
            fields.push("document_type");
        }
        // Compare through the same parser the sync uses to write the typed
        // column, so RFC3339 vs plain-date `created` values don't mismatch.
        if local.document_date
            != archivist_db::parse_paperless_document_date(document.created.as_deref())
        {
            fields.push("document_date");
        }
        let mut local_tags = local.current_tag_ids.clone();
        let mut remote_tags = document.tags.clone();
        local_tags.sort_unstable();
        remote_tags.sort_unstable();
        if local_tags != remote_tags {
            fields.push("tags");
        }
        if !fields.is_empty() {
            mismatches.push(json!({ "paperless_document_id": document.id, "fields": fields }));
        }
    }
    let stale_local = inventory
        .keys()
        .filter(|id| !seen_remote.contains(id))
        .copied()
        .collect::<Vec<_>>();

    let documents_checked = documents.len();
    Span::current().record("documents_checked", documents_checked);
    info!(
        documents_checked,
        missing_local = missing_local.len(),
        stale_local = stale_local.len(),
        mismatches = mismatches.len(),
        "paperless consistency check completed"
    );
    Ok(Json(json!({
        "documents_checked": documents_checked,
        "missing_local": missing_local,
        "stale_local": stale_local,
        "mismatches": mismatches,
        "ok": missing_local.is_empty() && stale_local.is_empty() && mismatches.is_empty()
    })))
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ReconcileCompletionTagsRequest {
    pub(crate) dry_run: Option<bool>,
    pub(crate) document_ids: Option<Vec<i32>>,
}

#[tracing::instrument(
    skip(state, auth, request),
    fields(user_id = tracing::field::Empty, dry_run = tracing::field::Empty)
)]
pub(crate) async fn reconcile_completion_tags(
    State(state): State<AppState>,
    auth: Authenticated,
    request: Option<Json<ReconcileCompletionTagsRequest>>,
) -> ApiResult<Json<Value>> {
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }
    let request = request.map(|Json(request)| request).unwrap_or_default();
    let settings = get_runtime_settings(&state.pool).await?;
    let client = paperless_client_from_settings(&state.pool, &state.config, &settings).await?;
    let dry_run = request.dry_run.unwrap_or(true);
    Span::current().record("dry_run", dry_run);
    let mut tags = client.list_tags().await?;
    let mut full_tag: Option<PaperlessTag> = None;
    let stage_completion_tags = settings
        .workflow
        .enabled_stages
        .iter()
        .filter_map(|stage| settings.workflow.tags.completion_tag_for_stage(*stage))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let inventory_complete_ids = archivist_db::completed_document_ids_missing_full_tag(
        &state.pool,
        &settings.workflow.enabled_stages,
    )
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let documents = client.list_documents().await?;
    let allowed_ids = request
        .document_ids
        .map(|ids| ids.into_iter().collect::<HashSet<_>>());
    let mut planned = Vec::new();
    let mut applied = Vec::new();
    // #410: O(1) tag-name lookups instead of a linear scan per document tag.
    let mut tag_names_by_id: HashMap<i32, String> =
        tags.iter().map(|tag| (tag.id, tag.name.clone())).collect();
    // #410: collect per-document failures instead of `?`-returning mid-loop,
    // so documents already tagged in Paperless always end up in the audit
    // event below (outcome `failure` when anything failed).
    let mut failure: Option<anyhow::Error> = None;
    for document in documents {
        if let Some(allowed_ids) = &allowed_ids
            && !allowed_ids.contains(&document.id)
        {
            continue;
        }
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tag_names_by_id.get(id).cloned())
            .collect::<Vec<_>>();
        let stage_tags_complete = !stage_completion_tags.is_empty()
            && stage_completion_tags
                .iter()
                .all(|tag| tag_names.iter().any(|name| name.eq_ignore_ascii_case(tag)));
        let inventory_stages_complete = inventory_complete_ids.contains(&document.id);
        if !completion_tag_reconcile_needed(
            &tag_names,
            &stage_completion_tags,
            &settings.workflow.tags.completion_processed,
            inventory_stages_complete,
        ) {
            continue;
        }
        if dry_run {
            planned.push(json!({ "paperless_document_id": document.id, "add": [settings.workflow.tags.completion_processed.clone()] }));
            continue;
        }
        let result: anyhow::Result<bool> = async {
            // #410: short reservation transaction instead of holding the
            // advisory lock and a pooled connection across the Paperless PATCH.
            let reserved = inventory_stages_complete && !stage_tags_complete;
            if reserved
                && !archivist_db::reserve_completion_tag_reconcile(
                    &state.pool,
                    document.id,
                    &settings.workflow.enabled_stages,
                )
                .await?
            {
                return Ok(false);
            }
            let write = async {
                let tag = match &full_tag {
                    Some(tag) => tag.clone(),
                    None => {
                        let tag = ensure_workflow_tag_cached(
                            &client,
                            &mut tags,
                            &settings.workflow.tags.completion_processed,
                        )
                        .await?;
                        tag_names_by_id.insert(tag.id, tag.name.clone());
                        full_tag = Some(tag.clone());
                        tag
                    }
                };
                client
                    .add_and_remove_tags(document.id, &[tag.id], &[])
                    .await
            }
            .await;
            if let Err(error) = write {
                if reserved
                    && let Err(release_error) =
                        archivist_db::release_completion_tag_reservation(&state.pool, document.id)
                            .await
                {
                    warn!(error = %release_error, document_id = document.id, "failed to release completion-tag reservation; next sync repairs it");
                }
                return Err(error);
            }
            archivist_db::record_full_completion_tag(&state.pool, document.id).await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => {
                planned.push(json!({ "paperless_document_id": document.id, "add": [settings.workflow.tags.completion_processed.clone()] }));
                applied.push(document.id);
            }
            Ok(false) => {}
            Err(error) => {
                planned.push(json!({ "paperless_document_id": document.id, "add": [settings.workflow.tags.completion_processed.clone()] }));
                warn!(error = %error, document_id = document.id, "completion tag reconciliation failed; stopping");
                failure = Some(error.context(format!(
                    "reconcile completion tag for document {}",
                    document.id
                )));
                break;
            }
        }
    }
    append_audit(
        &state.pool,
        completion_tags_reconciled_audit(
            &auth.0,
            dry_run,
            planned.len(),
            &applied,
            failure.as_ref(),
        ),
    )
    .await?;
    if let Some(error) = failure {
        return Err(error.into());
    }
    info!(
        dry_run,
        planned = planned.len(),
        applied = applied.len(),
        "completion tag reconciliation completed"
    );
    Ok(Json(
        json!({ "dry_run": dry_run, "planned": planned, "applied": applied }),
    ))
}

/// Audit event for a completion-tag reconciliation pass. Written on success
/// and on a partial failure alike, always listing the documents already
/// tagged in Paperless. #410
pub(crate) fn completion_tags_reconciled_audit(
    auth: &AuthContext,
    dry_run: bool,
    planned: usize,
    applied: &[i32],
    failure: Option<&anyhow::Error>,
) -> AuditEventInput {
    AuditEventInput {
        event_type: "paperless.completion_tags_reconciled".to_owned(),
        actor_type: auth.actor_type.clone(),
        actor_id: auth.actor_id.clone(),
        run_id: None,
        job_id: None,
        paperless_document_id: None,
        before: None,
        after: Some(json!({
            "planned": planned,
            "applied": applied.len(),
            "applied_document_ids": applied,
            "dry_run": dry_run,
        })),
        metadata: None,
        outcome: if failure.is_some() {
            "partial_failure"
        } else {
            "success"
        }
        .to_owned(),
        error_message: failure.map(|error| format!("{error:#}")),
        source_ip: None,
        user_agent: None,
    }
}

pub(crate) fn completion_tag_reconcile_needed(
    tag_names: &[String],
    stage_completion_tags: &[String],
    full_completion_tag: &str,
    inventory_stages_complete: bool,
) -> bool {
    !stage_completion_tags.is_empty()
        && (inventory_stages_complete
            || stage_completion_tags
                .iter()
                .all(|tag| tag_names.iter().any(|name| name.eq_ignore_ascii_case(tag))))
        && !tag_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(full_completion_tag))
}

pub(crate) async fn ensure_workflow_tag_cached(
    client: &PaperlessClient,
    tags: &mut Vec<PaperlessTag>,
    name: &str,
) -> Result<PaperlessTag> {
    if let Some(tag) = tags.iter().find(|tag| tag.name.eq_ignore_ascii_case(name)) {
        return Ok(tag.clone());
    }
    let tag = client.ensure_tag(name).await?;
    tags.push(tag.clone());
    Ok(tag)
}

/// Upper bound for one metadata option list. Archives with more entries get
/// `truncated: true`; the review select then still offers the first page. #420
pub(crate) const MAX_METADATA_OPTIONS: usize = 5000;

pub(crate) fn metadata_options_body(
    mut items: Vec<archivist_db::PaperlessNamedOption>,
) -> Json<Value> {
    let truncated = items.len() > MAX_METADATA_OPTIONS;
    items.truncate(MAX_METADATA_OPTIONS);
    Json(json!({ "items": items, "truncated": truncated }))
}

/// Synced Paperless correspondents `{id, name}` from the local mirror, for
/// the review edit select. No Paperless round-trip. #420
pub(crate) async fn paperless_correspondent_options(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let items = archivist_db::list_paperless_correspondent_options(
        &state.pool,
        MAX_METADATA_OPTIONS as i64 + 1,
    )
    .await?;
    Ok(metadata_options_body(items))
}

/// Synced Paperless document types `{id, name}` from the local mirror. #420
pub(crate) async fn paperless_document_type_options(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let items = archivist_db::list_paperless_document_type_options(
        &state.pool,
        MAX_METADATA_OPTIONS as i64 + 1,
    )
    .await?;
    Ok(metadata_options_body(items))
}

pub(crate) async fn sync_paperless_inventory(
    pool: &DbPool,
    client: &PaperlessClient,
    settings: &RuntimeSettings,
) -> Result<Value> {
    let archive_name = settings.paperless.active_archive.clone();
    let sync_started_at = Utc::now();
    let mut tags = client.list_tags().await?;
    // Only hit `ensure_tag` for workflow tags that are genuinely absent. Each
    // `ensure_tag` re-fetches the entire Paperless tag catalog, so calling it
    // unconditionally per workflow tag was O(workflow_tags × all_tags) — with a
    // few thousand tags this added minutes to every sync. The catalog is already
    // in `tags`; match it the same case-insensitive way `ensure_tag` does.
    for workflow_tag in settings.workflow.tags.all() {
        if !tags
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(workflow_tag))
        {
            tags.push(client.ensure_tag(workflow_tag).await?);
        }
    }
    let cursor = paperless_sync_cursor(pool, &archive_name).await?;
    let delta_cursor = cursor
        .map(|cursor| cursor - Duration::minutes(settings.paperless.delta_sync_overlap_minutes));
    // These four catalog fetches are independent GETs against Paperless; run
    // them concurrently rather than serially. The tag list above must stay
    // sequential because the workflow-tag loop mutates it in place. custom_fields
    // keeps its best-effort `unwrap_or_default` semantics inside the join.
    let (correspondents, document_types, custom_fields, (sync_mode, documents)) = tokio::try_join!(
        client.list_correspondents(),
        client.list_document_types(),
        async { anyhow::Ok(client.list_custom_fields().await.unwrap_or_default()) },
        async {
            if settings.paperless.delta_sync_enabled {
                if let Some(cursor) = delta_cursor {
                    match client
                        .list_documents_modified_since(&cursor.to_rfc3339())
                        .await
                    {
                        Ok(documents) => anyhow::Ok(("delta", documents)),
                        Err(_) => {
                            anyhow::Ok(("full_after_delta_error", client.list_documents().await?))
                        }
                    }
                } else {
                    anyhow::Ok(("full_initial", client.list_documents().await?))
                }
            } else {
                anyhow::Ok(("full", client.list_documents().await?))
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

    // O(1) id→name lookups: building this map once avoids the previous
    // O(documents × tags) nested linear scan, which was pure CPU burned inside
    // the sync transaction on instances with many tags/documents.
    let tag_names_by_id: HashMap<i32, &str> =
        tags.iter().map(|tag| (tag.id, tag.name.as_str())).collect();
    for document in &documents {
        let tag_names = document
            .tags
            .iter()
            .filter_map(|id| tag_names_by_id.get(id).copied())
            .map(|name| name.to_owned())
            .collect::<Vec<_>>();
        upsert_inventory_item(
            &mut tx,
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
    update_paperless_sync_cursor(&mut tx, &archive_name, sync_mode, sync_started_at).await?;
    tx.commit().await?;

    Ok(json!({
        "archive": archive_name,
        "mode": sync_mode,
        "delta_cursor": delta_cursor.map(|cursor| cursor.to_rfc3339()),
        "tags": tags.len(),
        "correspondents": correspondents.len(),
        "document_types": document_types.len(),
        "custom_fields": custom_fields.len(),
        "documents": documents.len()
    }))
}
