//! Audit log listing, detail, export, integrity and retention endpoints.

use crate::*;

#[derive(Debug, Deserialize)]
pub(crate) struct AuditQuery {
    pub(crate) limit: Option<i64>,
    /// Username, user UUID or raw actor id (e.g. an API token name). #448
    pub(crate) actor: Option<String>,
    pub(crate) actor_type: Option<String>,
    pub(crate) document_id: Option<i32>,
    /// Comma-separated exact event types. #448
    pub(crate) event_type: Option<String>,
    pub(crate) outcome: Option<String>,
    /// Inclusive lower bound (RFC 3339 or YYYY-MM-DD). #448
    pub(crate) from: Option<String>,
    /// Exclusive upper bound (RFC 3339); a YYYY-MM-DD date includes that whole day. #448
    pub(crate) to: Option<String>,
    /// Opaque keyset cursor from a previous page's `next_cursor`. #448
    pub(crate) cursor: Option<String>,
}

/// Maximum event types per audit filter (#448).
pub(crate) const AUDIT_EVENT_TYPE_FILTER_MAX: usize = 20;
/// Maximum length of any single free-text audit filter value (#448).
pub(crate) const AUDIT_FILTER_VALUE_MAX_CHARS: usize = 200;

pub(crate) fn audit_filter_text(
    name: &str,
    raw: Option<String>,
) -> Result<Option<String>, ApiError> {
    match raw.map(|value| value.trim().to_owned()) {
        Some(value) if value.is_empty() => Ok(None),
        Some(value) if value.chars().count() > AUDIT_FILTER_VALUE_MAX_CHARS => {
            Err(ApiError::bad_request(format!("'{name}' is too long")))
        }
        other => Ok(other),
    }
}

/// Parse an audit time bound (#448): RFC 3339, or a plain `YYYY-MM-DD` date
/// meaning midnight UTC of that day (`end_of_day` moves a date `to` bound to
/// the next midnight so the named day is included).
pub(crate) fn parse_audit_time_bound(
    name: &str,
    raw: Option<&str>,
    end_of_day: bool,
) -> Result<Option<DateTime<Utc>>, ApiError> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if let Ok(instant) = DateTime::parse_from_rfc3339(value) {
        return Ok(Some(instant.with_timezone(&Utc)));
    }
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
        ApiError::bad_request(format!(
            "'{name}' must be an RFC 3339 timestamp or a YYYY-MM-DD date"
        ))
    })?;
    let date = if end_of_day {
        date.succ_opt()
            .ok_or_else(|| ApiError::bad_request(format!("'{name}' is out of range")))?
    } else {
        date
    };
    Ok(Some(date.and_time(chrono::NaiveTime::MIN).and_utc()))
}

/// Encode the keyset position of the last returned audit event (#448). The
/// cursor is opaque to clients: base64url of `<created_at micros>|<id>`.
pub(crate) fn encode_audit_cursor(created_at: DateTime<Utc>, id: Uuid) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
        "{}|{id}",
        created_at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    ))
}

pub(crate) fn decode_audit_cursor(raw: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
    use base64::Engine as _;
    let invalid = || ApiError::bad_request("invalid audit cursor");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw.trim())
        .map_err(|_| invalid())?;
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;
    let (created_at, id) = text.split_once('|').ok_or_else(invalid)?;
    let created_at = DateTime::parse_from_rfc3339(created_at)
        .map_err(|_| invalid())?
        .with_timezone(&Utc);
    let id = Uuid::parse_str(id).map_err(|_| invalid())?;
    Ok((created_at, id))
}

// `GET /api/audit` (#448): newest-first audit events with server-side
// filters (actor, actor type, document, event types, outcome, time range)
// and keyset pagination over (created_at, id). Every single-dimension filter
// is backed by a keyset-shaped index (migration 0057), so deep pages cost the
// same as the first one.
pub(crate) async fn audit_events(
    State(state): State<AppState>,
    _auth: Authenticated,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Value>> {
    // Clamp so a caller that only needs a handful of rows (the debug console)
    // doesn't pull the full 200, and a large value can't be requested. (#277)
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    let actor_id = match audit_filter_text("actor", query.actor)? {
        None => None,
        // User events store the user's UUID as actor_id; accept a username
        // too so the UI can filter by the name it displays. Other actors
        // (API tokens, the worker) are matched by their raw actor id.
        Some(actor) if Uuid::parse_str(&actor).is_ok() => Some(actor.to_lowercase()),
        Some(actor) => Some(
            match archivist_db::find_user_id_by_username(&state.pool, &actor).await? {
                Some(user_id) => user_id.to_string(),
                None => actor,
            },
        ),
    };
    let event_types = split_csv(query.event_type);
    if event_types.len() > AUDIT_EVENT_TYPE_FILTER_MAX
        || event_types
            .iter()
            .any(|value| value.chars().count() > AUDIT_FILTER_VALUE_MAX_CHARS)
    {
        return Err(ApiError::bad_request(format!(
            "'event_type' accepts at most {AUDIT_EVENT_TYPE_FILTER_MAX} event types"
        )));
    }
    let filter = archivist_db::AuditEventFilter {
        actor_id,
        actor_type: audit_filter_text("actor_type", query.actor_type)?,
        paperless_document_id: query.document_id,
        event_types,
        outcome: audit_filter_text("outcome", query.outcome)?,
        from: parse_audit_time_bound("from", query.from.as_deref(), false)?,
        to: parse_audit_time_bound("to", query.to.as_deref(), true)?,
        before: query
            .cursor
            .as_deref()
            .filter(|cursor| !cursor.trim().is_empty())
            .map(decode_audit_cursor)
            .transpose()?,
    };
    // One extra row tells whether another page exists without a count query.
    let mut items = list_audit_events(&state.pool, &filter, limit + 1).await?;
    let next_cursor = if items.len() as i64 > limit {
        items.truncate(limit as usize);
        items
            .last()
            .map(|last| encode_audit_cursor(last.created_at, last.id))
    } else {
        None
    };
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

// `GET /api/audit/{id}` (#448): one event with its before/after snapshots
// for the diff view. `append_audit` already redacts credential-like keys on
// write; redacting again on read (same rules) also covers rows written before
// that, so a snapshot can never surface a secret.
pub(crate) async fn audit_event_detail(
    State(state): State<AppState>,
    _auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let Some(mut detail) = archivist_db::get_audit_event(&state.pool, id).await? else {
        return Err(ApiError::not_found("audit event not found"));
    };
    for value in [
        detail.before.as_mut(),
        detail.after.as_mut(),
        detail.event.metadata.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        archivist_core::redact_sensitive_json(value);
    }
    Ok(Json(json!(detail)))
}

/// Hard wall-clock budget for one CSV export. A client that stops reading
/// without disconnecting can otherwise pin the export task forever. #394
pub(crate) const AUDIT_EXPORT_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);
/// Rows fetched per keyset page. Each page is a short query, so no pool
/// connection is held while the task waits on a slow client. #394
pub(crate) const AUDIT_EXPORT_PAGE_SIZE: i64 = 500;
/// Concurrent exports across all actors (the default pool has 10 connections). #394
pub(crate) const MAX_CONCURRENT_AUDIT_EXPORTS: usize = 4;

pub(crate) static AUDIT_EXPORTS_IN_FLIGHT: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// One running audit export. At most one per actor and
/// [`MAX_CONCURRENT_AUDIT_EXPORTS`] in total; the slot is released on drop,
/// i.e. when the export task ends for any reason. #394
pub(crate) struct AuditExportSlot {
    pub(crate) actor: String,
}

impl AuditExportSlot {
    pub(crate) fn acquire(actor: String) -> Option<Self> {
        let mut in_flight = AUDIT_EXPORTS_IN_FLIGHT
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if in_flight.len() >= MAX_CONCURRENT_AUDIT_EXPORTS || in_flight.contains(&actor) {
            return None;
        }
        in_flight.insert(actor.clone());
        Some(Self { actor })
    }
}

impl Drop for AuditExportSlot {
    fn drop(&mut self) {
        AUDIT_EXPORTS_IN_FLIGHT
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.actor);
    }
}

pub(crate) type AuditExportSender = tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>;

pub(crate) async fn audit_export(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Response> {
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    let actor = format!(
        "{}:{}",
        auth.0.actor_type,
        auth.0.actor_id.as_deref().unwrap_or_default()
    );
    let slot = AuditExportSlot::acquire(actor).ok_or_else(|| {
        ApiError::too_many_requests("an audit export is already running; retry when it finishes")
    })?;
    // Exporting the whole audit trail is itself an auditable action. #394
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "audit.exported".to_owned(),
            actor_type: auth.0.actor_type.clone(),
            actor_id: auth.0.actor_id.clone(),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({ "format": "csv" })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    // Use a bounded channel so the writer task applies backpressure when
    // the HTTP client (or proxy) is slow. Capacity 16 is plenty for
    // one-CSV-row-at-a-time delivery.
    let (tx, rx) = mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let _slot = slot;
        run_audit_export(&pool, tx, AUDIT_EXPORT_DEADLINE).await;
    });

    let body = Body::from_stream(ReceiverStream::new(rx));
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"));
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"paperless-archivist-audit.csv\""),
    );
    Ok(response)
}

/// Stream the CSV into `tx` until done, the client disconnects, or
/// `deadline` elapses; on the deadline the stream is terminated with an error
/// so the client sees a truncated download rather than a silent cut. #394
pub(crate) async fn run_audit_export(
    pool: &DbPool,
    tx: AuditExportSender,
    deadline: std::time::Duration,
) {
    let error_tx = tx.clone();
    if tokio::time::timeout(deadline, write_audit_export(pool, tx))
        .await
        .is_err()
    {
        warn!(
            deadline_seconds = deadline.as_secs(),
            "audit CSV export aborted at its deadline"
        );
        let _ = error_tx.try_send(Err(std::io::Error::other(
            "audit export exceeded its time limit",
        )));
    }
}

pub(crate) async fn write_audit_export(pool: &DbPool, tx: AuditExportSender) {
    use bytes::Bytes;

    const HEADER: &str = "id,created_at,event_type,actor_type,actor_id,paperless_document_id,outcome,error_message,metadata,prev_event_hash,event_hash,hash_version,source_ip,user_agent\n";
    if tx
        .send(Ok(Bytes::from_static(HEADER.as_bytes())))
        .await
        .is_err()
    {
        return;
    }
    // Keyset pagination on the (created_at, id) sort key instead of one
    // long-lived cursor: every page releases its pool connection before the
    // rows are pushed to the (possibly stalled) client. #394
    let mut cursor: Option<(DateTime<Utc>, Uuid)> = None;
    loop {
        let rows = match sqlx::query(
            r#"
            select id, event_type, actor_type, actor_id, paperless_document_id,
                   outcome, error_message, created_at, metadata,
                   prev_event_hash, event_hash, hash_version, source_ip, user_agent
              from audit_events
             where $1::timestamptz is null or (created_at, id) < ($1, $2)
             order by created_at desc, id desc
             limit $3
            "#,
        )
        .bind(cursor.map(|(created_at, _)| created_at))
        .bind(cursor.map(|(_, id)| id))
        .bind(AUDIT_EXPORT_PAGE_SIZE)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                let _ = tx
                    .send(Err(std::io::Error::other(format!(
                        "stream audit events: {error}"
                    ))))
                    .await;
                return;
            }
        };
        let page_len = rows.len();
        for row in &rows {
            let line = match audit_csv_row(row) {
                Ok(line) => line,
                Err(error) => {
                    let _ = tx
                        .send(Err(std::io::Error::other(format!(
                            "encode audit row: {error}"
                        ))))
                        .await;
                    return;
                }
            };
            if tx.send(Ok(Bytes::from(line))).await.is_err() {
                return;
            }
        }
        let Some(last) = rows.last() else {
            return;
        };
        match (
            last.try_get::<DateTime<Utc>, _>("created_at"),
            last.try_get::<Uuid, _>("id"),
        ) {
            (Ok(created_at), Ok(id)) => cursor = Some((created_at, id)),
            _ => {
                let _ = tx
                    .send(Err(std::io::Error::other("audit export cursor")))
                    .await;
                return;
            }
        }
        if (page_len as i64) < AUDIT_EXPORT_PAGE_SIZE {
            return;
        }
    }
}

pub(crate) fn audit_csv_row(row: &sqlx::postgres::PgRow) -> Result<String, sqlx::Error> {
    let id: Uuid = row.try_get("id")?;
    let event_type: String = row.try_get("event_type")?;
    let actor_type: String = row.try_get("actor_type")?;
    let actor_id: Option<String> = row.try_get("actor_id")?;
    let paperless_document_id: Option<i32> = row.try_get("paperless_document_id")?;
    let outcome: String = row.try_get("outcome")?;
    let error_message: Option<String> = row.try_get("error_message")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let metadata: Option<Value> = row.try_get("metadata")?;
    let prev_event_hash: Option<String> = row.try_get("prev_event_hash")?;
    let event_hash: Option<String> = row.try_get("event_hash")?;
    let hash_version: Option<i16> = row.try_get("hash_version")?;
    let source_ip: Option<String> = row.try_get("source_ip")?;
    let user_agent: Option<String> = row.try_get("user_agent")?;
    let cells = [
        id.to_string(),
        created_at.to_rfc3339(),
        event_type,
        actor_type,
        actor_id.unwrap_or_default(),
        paperless_document_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        outcome,
        error_message.unwrap_or_default(),
        metadata.map(|value| value.to_string()).unwrap_or_default(),
        prev_event_hash.unwrap_or_default(),
        event_hash.unwrap_or_default(),
        hash_version
            .map(|version| version.to_string())
            .unwrap_or_default(),
        source_ip.unwrap_or_default(),
        user_agent.unwrap_or_default(),
    ];
    let mut out = String::with_capacity(256);
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&csv_escape(cell));
    }
    out.push('\n');
    Ok(out)
}

pub(crate) async fn audit_integrity(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(verify_audit_integrity(&state.pool).await?)))
}

pub(crate) async fn apply_audit_retention(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    let settings = get_runtime_settings(&state.pool).await?;
    Ok(Json(json!(
        apply_security_retention(&state.pool, &settings, actor_id).await?
    )))
}

pub(crate) fn csv_escape(value: &str) -> String {
    // Neutralize spreadsheet formula injection (CWE-1236): a leading
    // = + - @ or tab/CR makes Excel/LibreOffice treat the cell as a formula.
    // Prefix such values with a single quote so they are rendered as text.
    let needs_formula_guard = value
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '=' | '+' | '-' | '@' | '\t' | '\r'));
    let guarded;
    let value = if needs_formula_guard {
        guarded = format!("'{value}");
        guarded.as_str()
    } else {
        value
    };
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}
