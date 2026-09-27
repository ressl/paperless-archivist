//! Inventory listing, filters, saved views, export and duplicates.

use crate::*;

#[derive(Debug, Deserialize)]
pub(crate) struct InventoryQueryParams {
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
    pub(crate) id: Option<i32>,
    pub(crate) q: Option<String>,
    pub(crate) ocr_status: Option<String>,
    pub(crate) metadata_status: Option<String>,
    pub(crate) run_status: Option<String>,
    pub(crate) tag: Option<String>,
    pub(crate) not_tag: Option<String>,
    pub(crate) lang: Option<String>,
    pub(crate) date_from: Option<String>,
    pub(crate) date_to: Option<String>,
    pub(crate) has_error: Option<bool>,
    pub(crate) needs_review: Option<bool>,
    /// Comma-separated Paperless correspondent ids and/or `none` (#447).
    pub(crate) correspondent: Option<String>,
    /// Comma-separated Paperless document type ids and/or `none` (#447).
    pub(crate) document_type: Option<String>,
}

pub(crate) fn split_csv(value: Option<String>) -> Vec<String> {
    value
        .map(|s| {
            s.split(',')
                .map(|part| part.trim().to_owned())
                .filter(|part| !part.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an inventory `date_from`/`date_to` filter. Absent or blank means "no
/// filter"; a present but unparseable value is rejected with 400 instead of
/// being silently ignored (same contract as the statistics range, #312). The
/// column is a real `date` since migration 0043, so only `YYYY-MM-DD` is
/// meaningful here.
pub(crate) fn parse_inventory_date_filter(
    name: &str,
    raw: Option<&str>,
) -> Result<Option<chrono::NaiveDate>, ApiError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| ApiError::bad_request(format!("'{name}' must be a YYYY-MM-DD date"))),
    }
}

pub(crate) async fn inventory(
    State(state): State<AppState>,
    _auth: Authenticated,
    Query(query): Query<InventoryQueryParams>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let offset = query.offset.unwrap_or(0).max(0);
    let inventory_query = inventory_query_from_params(query)?;
    let settings = get_runtime_settings(&state.pool).await?;
    let (items, total) = tokio::try_join!(
        async {
            list_inventory(&state.pool, &inventory_query, limit, offset)
                .await?
                .into_iter()
                .map(|item| inventory_item_with_debug(item, &settings))
                .collect::<Result<Vec<_>>>()
        },
        async { archivist_db::count_inventory(&state.pool, &inventory_query).await }
    )?;
    Ok(Json(json!({
        "items": items,
        "total": total,
        "offset": offset,
        "limit": limit,
    })))
}

/// Maximum ids per `correspondent` / `document_type` filter (#447).
pub(crate) const INVENTORY_ID_FILTER_MAX: usize = 100;

/// Parse a `correspondent` / `document_type` filter (#447): comma-separated
/// positive Paperless ids and/or the literal `none` (object unset). Anything
/// else is rejected with 400 rather than silently ignored.
pub(crate) fn parse_inventory_id_filter(
    name: &str,
    raw: Option<String>,
) -> Result<archivist_db::InventoryIdFilter, ApiError> {
    let mut filter = archivist_db::InventoryIdFilter::default();
    for part in split_csv(raw) {
        if part.eq_ignore_ascii_case("none") {
            filter.include_none = true;
            continue;
        }
        let id = part
            .parse::<i32>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| {
                ApiError::bad_request(format!(
                    "'{name}' must be comma-separated Paperless ids or 'none'"
                ))
            })?;
        if !filter.ids.contains(&id) {
            filter.ids.push(id);
        }
    }
    if filter.ids.len() > INVENTORY_ID_FILTER_MAX {
        return Err(ApiError::bad_request(format!(
            "'{name}' accepts at most {INVENTORY_ID_FILTER_MAX} ids"
        )));
    }
    Ok(filter)
}

/// Translate the `/api/inventory` query parameters into the SQL-layer filter.
/// Shared by the list, the export and saved-view validation (#447), so all
/// three accept and reject exactly the same filters.
pub(crate) fn inventory_query_from_params(
    query: InventoryQueryParams,
) -> Result<archivist_db::InventoryQuery, ApiError> {
    Ok(archivist_db::InventoryQuery {
        id: query.id,
        q: query.q,
        ocr_status: split_csv(query.ocr_status),
        metadata_status: split_csv(query.metadata_status),
        run_status: split_csv(query.run_status),
        tags_include: split_csv(query.tag),
        tags_exclude: split_csv(query.not_tag),
        language: query.lang.filter(|s| !s.is_empty()),
        date_from: parse_inventory_date_filter("date_from", query.date_from.as_deref())?,
        date_to: parse_inventory_date_filter("date_to", query.date_to.as_deref())?,
        has_error: query.has_error,
        needs_review: query.needs_review,
        correspondent: parse_inventory_id_filter("correspondent", query.correspondent)?,
        document_type: parse_inventory_id_filter("document_type", query.document_type)?,
    })
}

/// Filter keys a saved view / export may carry, in canonical order (#447).
/// Paging (`limit`, `offset`) is deliberately not part of a view.
pub(crate) const INVENTORY_FILTER_KEYS: [&str; 14] = [
    "id",
    "q",
    "ocr_status",
    "metadata_status",
    "run_status",
    "tag",
    "not_tag",
    "lang",
    "date_from",
    "date_to",
    "has_error",
    "needs_review",
    "correspondent",
    "document_type",
];

/// Maximum length of a stored / exported filter query string (#447); matches
/// the `inventory_saved_views.query` CHECK in migration 0056.
pub(crate) const INVENTORY_FILTER_QUERY_MAX_BYTES: usize = 2000;

/// Validate an inventory filter query string and return it in canonical form
/// (known keys only, each at most once, empty values dropped, fixed key
/// order) together with the parsed filter (#447). The canonical string is
/// what saved views store and what the export audit event records.
pub(crate) fn canonical_inventory_filter_query(
    raw: &str,
) -> Result<(String, archivist_db::InventoryQuery), ApiError> {
    let raw = raw.trim().trim_start_matches('?');
    if raw.len() > INVENTORY_FILTER_QUERY_MAX_BYTES {
        return Err(ApiError::bad_request("inventory filter query is too long"));
    }
    let mut pairs: Vec<(usize, String, String)> = Vec::new();
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        let Some(position) = INVENTORY_FILTER_KEYS.iter().position(|known| *known == key) else {
            return Err(ApiError::bad_request(format!(
                "unknown inventory filter '{key}'"
            )));
        };
        if pairs.iter().any(|(existing, _, _)| *existing == position) {
            return Err(ApiError::bad_request(format!(
                "inventory filter '{key}' given more than once"
            )));
        }
        let value = value.trim();
        if !value.is_empty() {
            pairs.push((position, key.into_owned(), value.to_owned()));
        }
    }
    pairs.sort_by_key(|(position, _, _)| *position);
    let canonical = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(_, key, value)| (key, value)))
        .finish();
    if canonical.len() > INVENTORY_FILTER_QUERY_MAX_BYTES {
        return Err(ApiError::bad_request("inventory filter query is too long"));
    }
    let uri: axum::http::Uri = format!("/?{canonical}")
        .parse()
        .map_err(|_| ApiError::bad_request("invalid inventory filter query"))?;
    let Query(params) = Query::<InventoryQueryParams>::try_from_uri(&uri)
        .map_err(|error| ApiError::bad_request(format!("invalid inventory filter: {error}")))?;
    Ok((canonical, inventory_query_from_params(params)?))
}

#[derive(Debug, Deserialize)]
pub(crate) struct InventoryViewRequest {
    pub(crate) name: String,
    pub(crate) query: String,
}

/// Validate a saved-view write (#447): a trimmed 1..=80 character name
/// without control characters and a canonicalised filter query.
pub(crate) fn validate_inventory_view(
    request: &InventoryViewRequest,
) -> Result<(String, String), ApiError> {
    let name = request.name.trim();
    let length = name.chars().count();
    if length == 0 || length > 80 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "view name must be 1 to 80 characters without control characters",
        ));
    }
    let (query, _) = canonical_inventory_filter_query(&request.query)?;
    Ok((name.to_owned(), query))
}

/// Map the expected saved-view outcomes to 404/409; everything else stays a
/// server error (#447).
pub(crate) fn inventory_view_error(error: anyhow::Error) -> ApiError {
    match error.downcast_ref::<archivist_db::InventoryViewError>() {
        Some(archivist_db::InventoryViewError::NotFound) => {
            ApiError::not_found("saved view not found")
        }
        Some(view_error @ archivist_db::InventoryViewError::DuplicateName)
        | Some(view_error @ archivist_db::InventoryViewError::LimitReached) => {
            ApiError::conflict(view_error.to_string())
        }
        None => ApiError::from(error),
    }
}

// Saved inventory views (#447) are UI state private to one user: the route
// table admits them for sessions only (an API token has no personal views)
// with inventory:read.
pub(crate) async fn inventory_views(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let user_id = auth.session_user_id()?;
    Ok(Json(json!({
        "items": archivist_db::list_inventory_views(&state.pool, user_id).await?
    })))
}

pub(crate) async fn create_inventory_view_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<InventoryViewRequest>,
) -> ApiResult<Json<Value>> {
    let user_id = auth.session_user_id()?;
    let (name, query) = validate_inventory_view(&request)?;
    let view = archivist_db::create_inventory_view(&state.pool, user_id, &name, &query)
        .await
        .map_err(inventory_view_error)?;
    Ok(Json(json!(view)))
}

pub(crate) async fn update_inventory_view_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<InventoryViewRequest>,
) -> ApiResult<Json<Value>> {
    let user_id = auth.session_user_id()?;
    let (name, query) = validate_inventory_view(&request)?;
    let view = archivist_db::update_inventory_view(&state.pool, user_id, id, &name, &query)
        .await
        .map_err(inventory_view_error)?;
    Ok(Json(json!(view)))
}

pub(crate) async fn delete_inventory_view_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let user_id = auth.session_user_id()?;
    archivist_db::delete_inventory_view(&state.pool, user_id, id)
        .await
        .map_err(inventory_view_error)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InventoryExportFormat {
    Csv,
    Json,
}

impl InventoryExportFormat {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }
}

/// Hard wall-clock budget for one inventory export (#447, same policy as the
/// audit export #394).
pub(crate) const INVENTORY_EXPORT_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);
/// Rows per keyset page of the inventory export (#447).
pub(crate) const INVENTORY_EXPORT_PAGE_SIZE: i64 = 500;

// `GET /api/inventory/export?format=csv|json&<filters>` (#447)
//
// Streams every inventory row matching the same filters as `/api/inventory`
// (inventory:read, like the list). Bounded like the audit export (#394):
// keyset pages that release their pool connection between pages, a bounded
// channel for backpressure, one running export per actor (sharing the global
// export slot cap) and a hard deadline. Each export is itself audited
// (`inventory.exported`, with the canonical filter string).
pub(crate) async fn inventory_export(
    State(state): State<AppState>,
    auth: Authenticated,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> ApiResult<Response> {
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    let (format, canonical, inventory_query) =
        parse_inventory_export_query(raw_query.as_deref().unwrap_or_default())?;

    let actor = format!(
        "inventory:{}:{}",
        auth.0.actor_type,
        auth.0.actor_id.as_deref().unwrap_or_default()
    );
    // The audit export's slot registry doubles as the global export cap; the
    // "inventory:" key prefix keeps one inventory and one audit export per
    // actor independent of each other.
    let slot = AuditExportSlot::acquire(actor).ok_or_else(|| {
        ApiError::too_many_requests(
            "an inventory export is already running; retry when it finishes",
        )
    })?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "inventory.exported".to_owned(),
            actor_type: auth.0.actor_type.clone(),
            actor_id: auth.0.actor_id.clone(),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({ "format": format.as_str(), "filters": canonical })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    let (tx, rx) = mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let _slot = slot;
        run_inventory_export(
            &pool,
            inventory_query,
            format,
            tx,
            INVENTORY_EXPORT_DEADLINE,
        )
        .await;
    });

    let (content_type, disposition) = match format {
        InventoryExportFormat::Csv => (
            "text/csv; charset=utf-8",
            "attachment; filename=\"paperless-archivist-inventory.csv\"",
        ),
        InventoryExportFormat::Json => (
            "application/json",
            "attachment; filename=\"paperless-archivist-inventory.json\"",
        ),
    };
    let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static(disposition),
    );
    Ok(response)
}

/// Split the export query into its `format` and the canonicalised filters
/// (#447). Kept synchronous: the form-urlencoded serializer is not `Send`
/// and must not live across an await in the handler.
pub(crate) fn parse_inventory_export_query(
    raw_query: &str,
) -> Result<(InventoryExportFormat, String, archivist_db::InventoryQuery), ApiError> {
    let mut format = InventoryExportFormat::Csv;
    let mut filters = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        if key == "format" {
            format = match value.as_ref() {
                "csv" => InventoryExportFormat::Csv,
                "json" => InventoryExportFormat::Json,
                _ => return Err(ApiError::bad_request("'format' must be csv or json")),
            };
        } else {
            filters.append_pair(&key, &value);
        }
    }
    let (canonical, query) = canonical_inventory_filter_query(&filters.finish())?;
    Ok((format, canonical, query))
}

/// Stream the inventory export into `tx` until done, the client disconnects
/// or `deadline` elapses; a deadline ends the stream with an error so the
/// client sees a truncated download rather than a silent cut (#447).
pub(crate) async fn run_inventory_export(
    pool: &DbPool,
    query: archivist_db::InventoryQuery,
    format: InventoryExportFormat,
    tx: AuditExportSender,
    deadline: std::time::Duration,
) {
    let error_tx = tx.clone();
    if tokio::time::timeout(deadline, write_inventory_export(pool, &query, format, tx))
        .await
        .is_err()
    {
        warn!(
            deadline_seconds = deadline.as_secs(),
            "inventory export aborted at its deadline"
        );
        let _ = error_tx.try_send(Err(std::io::Error::other(
            "inventory export exceeded its time limit",
        )));
    }
}

pub(crate) const INVENTORY_CSV_HEADER: &str = "paperless_document_id,title,original_file_name,correspondent_id,correspondent,document_type_id,document_type,document_date,tags,ocr_status,metadata_status,run_status,needs_review,complete,detected_language,last_error,last_seen_at\n";

pub(crate) async fn write_inventory_export(
    pool: &DbPool,
    query: &archivist_db::InventoryQuery,
    format: InventoryExportFormat,
    tx: AuditExportSender,
) {
    use bytes::Bytes;

    let opening: &'static str = match format {
        InventoryExportFormat::Csv => INVENTORY_CSV_HEADER,
        InventoryExportFormat::Json => "[",
    };
    if tx
        .send(Ok(Bytes::from_static(opening.as_bytes())))
        .await
        .is_err()
    {
        return;
    }
    let mut before_id: Option<i32> = None;
    let mut first = true;
    loop {
        let rows = match archivist_db::list_inventory_keyset(
            pool,
            query,
            before_id,
            INVENTORY_EXPORT_PAGE_SIZE,
        )
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                let _ = tx
                    .send(Err(std::io::Error::other(format!(
                        "stream inventory: {error}"
                    ))))
                    .await;
                return;
            }
        };
        for item in &rows {
            let chunk = match format {
                InventoryExportFormat::Csv => inventory_csv_row(item),
                InventoryExportFormat::Json => match serde_json::to_string(item) {
                    Ok(json) => format!("{}\n{json}", if first { "" } else { "," }),
                    Err(error) => {
                        let _ = tx
                            .send(Err(std::io::Error::other(format!(
                                "encode inventory row: {error}"
                            ))))
                            .await;
                        return;
                    }
                },
            };
            first = false;
            if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                return;
            }
        }
        before_id = rows.last().map(|item| item.paperless_document_id);
        if (rows.len() as i64) < INVENTORY_EXPORT_PAGE_SIZE {
            break;
        }
    }
    if format == InventoryExportFormat::Json {
        let _ = tx.send(Ok(Bytes::from_static(b"\n]\n"))).await;
    }
}

pub(crate) fn inventory_csv_row(item: &DocumentInventoryItem) -> String {
    let optional_id = |id: Option<i32>| id.map(|id| id.to_string()).unwrap_or_default();
    let cells = [
        item.paperless_document_id.to_string(),
        item.title.clone().unwrap_or_default(),
        item.original_file_name.clone().unwrap_or_default(),
        optional_id(item.correspondent_id),
        item.correspondent_name.clone().unwrap_or_default(),
        optional_id(item.document_type_id),
        item.document_type_name.clone().unwrap_or_default(),
        item.document_date
            .map(|date| date.to_string())
            .unwrap_or_default(),
        item.current_tags.join("; "),
        item.ocr_status.clone(),
        item.metadata_status.clone(),
        item.current_run_status.clone().unwrap_or_default(),
        item.needs_review.to_string(),
        item.complete.to_string(),
        item.detected_language.clone().unwrap_or_default(),
        item.last_error.clone().unwrap_or_default(),
        item.last_seen_at.to_rfc3339(),
    ];
    let mut out = String::with_capacity(256);
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&csv_escape(cell));
    }
    out.push('\n');
    out
}

// `GET /api/inventory/duplicates`
//
// Read-only dedup view (#216): groups `document_inventory` by the already
// persisted `ocr_content_hash`, returning every hash shared by more than one
// document. Capped at `DUPLICATE_GROUP_LIMIT` groups; logs a warning when the
// result is truncated so operators know the view is incomplete.
pub(crate) async fn inventory_duplicates(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let settings = get_runtime_settings(&state.pool).await?;
    let groups = archivist_db::list_inventory_duplicates(&state.pool).await?;
    if groups.len() as i64 >= archivist_db::DUPLICATE_GROUP_LIMIT {
        warn!(
            cap = archivist_db::DUPLICATE_GROUP_LIMIT,
            "inventory duplicate groups truncated at cap; some duplicates not shown"
        );
    }
    // Externally reachable Paperless base for browser deep-links: prefer the
    // configured public_url, fall back to the internal base_url. Trailing slash
    // trimmed so the frontend can append `/documents/{id}/details`. Returned
    // here (rather than read from /api/settings) because the Inventory view is
    // available to users without the ReadSettings permission.
    let paperless_base = settings
        .paperless
        .public_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(settings.paperless.base_url.trim())
        .trim_end_matches('/')
        .to_owned();
    Ok(Json(json!({
        "groups": groups,
        "paperless_base": paperless_base,
    })))
}

pub(crate) fn inventory_item_with_debug(
    item: DocumentInventoryItem,
    settings: &RuntimeSettings,
) -> Result<Value> {
    let debug_context = inventory_debug_context(&item, settings);
    let mut value = serde_json::to_value(item)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("debug_context".to_owned(), debug_context);
    }
    Ok(value)
}

pub(crate) fn inventory_debug_context(
    item: &DocumentInventoryItem,
    settings: &RuntimeSettings,
) -> Value {
    let include_tags = WorkflowRules::normalized_tags(&settings.workflow.rules.include_tags);
    let exclude_tags = WorkflowRules::normalized_tags(&settings.workflow.rules.exclude_tags);
    let current_tags = item
        .current_tags
        .iter()
        .map(|tag| tag.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let reason = if settings.workflow.paused {
        "workflow_paused"
    } else if item.complete {
        "complete"
    } else if item.current_run_status.as_deref().is_some_and(|status| {
        matches!(status, "queued" | "running" | "waiting_review" | "applying")
    }) {
        "already_active"
    } else if !exclude_tags.is_empty() && exclude_tags.iter().any(|tag| current_tags.contains(tag))
    {
        "excluded_by_tag"
    } else if !include_tags.is_empty() && !include_tags.iter().any(|tag| current_tags.contains(tag))
    {
        "missing_include_tag"
    } else if item.needs_review {
        "waiting_review"
    } else if item.next_required_stage.is_some() {
        "missing_enabled_stage"
    } else {
        "no_missing_enabled_stage"
    };
    json!({
        "selector_reason": reason,
        "workflow_mode": settings.workflow.mode,
        "workflow_paused": settings.workflow.paused,
        "dry_run": settings.workflow.dry_run,
        "prompt_language": item.detected_language.as_deref().unwrap_or("und"),
        "tag_output_language": settings.tagging.tag_output_language,
        "detected_language": item.detected_language.clone(),
        "detected_language_confidence": item.detected_language_confidence,
        "detected_language_source": item.detected_language_source.clone(),
        "next_required_stage": item.next_required_stage.clone(),
        "last_error": item.last_error.clone()
    })
}
