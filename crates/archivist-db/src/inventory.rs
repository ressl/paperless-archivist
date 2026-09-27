//! Paperless catalog/inventory sync upserts, inventory queries, saved views and duplicates.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomFieldRecord {
    pub id: i32,
    pub name: String,
    pub data_type: Option<String>,
}

/// Sync upsert for Paperless tags.
///
/// The full-pass sync re-upserts every row each round, so the `do update` is
/// guarded with `IS DISTINCT FROM` (#302): a payload identical to the stored
/// row writes nothing instead of physically rewriting the tuple (previously
/// ~98.5 % of all SQL statements and ~9.4 GB WAL/day were such no-ops). This
/// shifts the meaning of `last_seen_at` on all four sync tables from "last
/// time the sync saw this row" to "last time the synced payload changed";
/// the column has no predicate consumers (display + sort tiebreak only), so
/// the relaxation is safe.
pub async fn upsert_paperless_tag(
    tx: &mut Transaction<'_, Postgres>,
    id: i32,
    name: &str,
    slug: Option<&str>,
    color: Option<&str>,
    is_workflow: bool,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into paperless_tags (id, name, slug, color, is_workflow, last_seen_at, updated_at)
        values ($1, $2, $3, $4, $5, now(), now())
        on conflict (id)
        do update set name = excluded.name,
                      slug = excluded.slug,
                      color = excluded.color,
                      is_workflow = excluded.is_workflow,
                      last_seen_at = now(),
                      updated_at = now()
        where (paperless_tags.name, paperless_tags.slug, paperless_tags.color, paperless_tags.is_workflow)
              is distinct from
              (excluded.name, excluded.slug, excluded.color, excluded.is_workflow)
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(slug)
    .bind(color)
    .bind(is_workflow)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn upsert_paperless_named_entity(
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
    id: i32,
    name: &str,
) -> Result<()> {
    let table = match table {
        "paperless_correspondents" => "paperless_correspondents",
        "paperless_document_types" => "paperless_document_types",
        "paperless_custom_fields" => "paperless_custom_fields",
        _ => return Err(anyhow!("unsupported metadata table: {table}")),
    };
    // No-op guard + last_seen_at semantics: see upsert_paperless_tag (#302).
    let sql = format!(
        r#"
        insert into {table} (id, name, last_seen_at, updated_at)
        values ($1, $2, now(), now())
        on conflict (id)
        do update set name = excluded.name,
                      last_seen_at = now(),
                      updated_at = now()
        where {table}.name is distinct from excluded.name
        "#
    );
    // SAFETY: `sql` is a static literal built above with no user-controlled
    // interpolation; only bind parameters carry caller data.
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .bind(name)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn upsert_paperless_custom_field(
    tx: &mut Transaction<'_, Postgres>,
    id: i32,
    name: &str,
    data_type: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into paperless_custom_fields (id, name, data_type, last_seen_at, updated_at)
        values ($1, $2, $3, now(), now())
        on conflict (id)
        do update set name = excluded.name,
                      data_type = excluded.data_type,
                      last_seen_at = now(),
                      updated_at = now()
        where (paperless_custom_fields.name, paperless_custom_fields.data_type)
              is distinct from
              (excluded.name, excluded.data_type)
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(data_type)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryUpsert {
    pub paperless_document_id: i32,
    pub title: Option<String>,
    pub original_file_name: Option<String>,
    pub current_tags: Vec<String>,
    pub current_tag_ids: Vec<i32>,
    pub correspondent_id: Option<i32>,
    pub document_type_id: Option<i32>,
    /// Typed since migration 0043 (document_inventory.document_date is a real
    /// `date` now); built from the Paperless `created` field via
    /// [`parse_paperless_document_date`].
    pub document_date: Option<NaiveDate>,
    pub paperless_modified_at: Option<DateTime<Utc>>,
    pub has_ocr_completion_tag: bool,
    pub has_tagging_completion_tag: bool,
    pub has_full_completion_tag: bool,
}

/// Parse the Paperless `created` field into the typed inventory document
/// date. Paperless reports a plain ISO date on current releases (verified on
/// the live data: every row matches `YYYY-MM-DD`); older releases used a full
/// RFC3339 timestamp — accept both by reading the leading date. Lenient by
/// design: a malformed value yields None instead of failing the whole sync.
pub fn parse_paperless_document_date(created: Option<&str>) -> Option<NaiveDate> {
    let raw = created?.trim();
    let prefix = raw.get(..10)?;
    NaiveDate::parse_from_str(prefix, "%Y-%m-%d").ok()
}

/// Parse Paperless' RFC3339 `modified` value into the UTC instant stored by
/// the inventory. Missing or malformed values are treated as unavailable so a
/// partial sync can preserve the last known timestamp.
pub fn parse_paperless_modified_at(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

/// Sync upsert for the document inventory.
///
/// No-op guard + last_seen_at semantics: see upsert_paperless_tag (#302). The
/// guard compares every column the UPDATE would write — including the
/// computed `ocr_status` ratchet and the `complete` overwrite — so it fires
/// exactly when the row would actually change (e.g. re-ratcheting a status
/// another writer downgraded), and skips the physical write otherwise.
pub async fn upsert_inventory_item(
    tx: &mut Transaction<'_, Postgres>,
    item: &InventoryUpsert,
) -> Result<()> {
    let ocr_status = if item.has_ocr_completion_tag || item.has_full_completion_tag {
        "succeeded"
    } else {
        "unknown"
    };
    let metadata_status = if item.has_tagging_completion_tag || item.has_full_completion_tag {
        "succeeded"
    } else {
        "unknown"
    };
    let complete = item.has_full_completion_tag;
    sqlx::query(
        r#"
        insert into document_inventory (
          paperless_document_id, title, original_file_name, current_tags, current_tag_ids,
          correspondent_id, document_type_id, document_date, paperless_modified_at,
          has_ocr_completion_tag, has_tagging_completion_tag, has_full_completion_tag,
          ocr_status, metadata_status, complete, last_seen_at, updated_at
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, now(), now())
        on conflict (paperless_document_id)
        do update set title = excluded.title,
                      original_file_name = excluded.original_file_name,
                      current_tags = excluded.current_tags,
                      current_tag_ids = excluded.current_tag_ids,
                      correspondent_id = excluded.correspondent_id,
                      document_type_id = excluded.document_type_id,
                      document_date = excluded.document_date,
                      paperless_modified_at = coalesce(excluded.paperless_modified_at, document_inventory.paperless_modified_at),
                      has_ocr_completion_tag = excluded.has_ocr_completion_tag,
                      has_tagging_completion_tag = excluded.has_tagging_completion_tag,
                      has_full_completion_tag = excluded.has_full_completion_tag,
                      ocr_status = case when excluded.has_ocr_completion_tag or excluded.has_full_completion_tag then 'succeeded' else document_inventory.ocr_status end,
                      metadata_status = case when excluded.has_tagging_completion_tag or excluded.has_full_completion_tag then 'succeeded' else document_inventory.metadata_status end,
                      complete = excluded.has_full_completion_tag,
                      last_seen_at = now(),
                      updated_at = now()
        where (document_inventory.title,
               document_inventory.original_file_name,
               document_inventory.current_tags,
               document_inventory.current_tag_ids,
               document_inventory.correspondent_id,
               document_inventory.document_type_id,
               document_inventory.document_date,
               document_inventory.paperless_modified_at,
               document_inventory.has_ocr_completion_tag,
               document_inventory.has_tagging_completion_tag,
               document_inventory.has_full_completion_tag,
               document_inventory.ocr_status,
               document_inventory.metadata_status,
               document_inventory.complete)
              is distinct from
              (excluded.title,
               excluded.original_file_name,
               excluded.current_tags,
               excluded.current_tag_ids,
               excluded.correspondent_id,
               excluded.document_type_id,
               excluded.document_date,
               coalesce(excluded.paperless_modified_at, document_inventory.paperless_modified_at),
               excluded.has_ocr_completion_tag,
               excluded.has_tagging_completion_tag,
               excluded.has_full_completion_tag,
               case when excluded.has_ocr_completion_tag or excluded.has_full_completion_tag then 'succeeded' else document_inventory.ocr_status end,
               case when excluded.has_tagging_completion_tag or excluded.has_full_completion_tag then 'succeeded' else document_inventory.metadata_status end,
               excluded.has_full_completion_tag)
        "#,
    )
    .bind(item.paperless_document_id)
    .bind(&item.title)
    .bind(&item.original_file_name)
    .bind(&item.current_tags)
    .bind(&item.current_tag_ids)
    .bind(item.correspondent_id)
    .bind(item.document_type_id)
    .bind(item.document_date)
    .bind(item.paperless_modified_at)
    .bind(item.has_ocr_completion_tag)
    .bind(item.has_tagging_completion_tag)
    .bind(item.has_full_completion_tag)
    .bind(ocr_status)
    .bind(metadata_status)
    .bind(complete)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn paperless_sync_cursor(
    pool: &DbPool,
    archive_name: &str,
) -> Result<Option<DateTime<Utc>>> {
    let row =
        sqlx::query("select last_delta_cursor from paperless_sync_state where archive_name = $1")
            .bind(archive_name)
            .fetch_optional(pool)
            .await?;
    Ok(row
        .map(|row| row.try_get("last_delta_cursor"))
        .transpose()?)
}

pub async fn update_paperless_sync_cursor(
    tx: &mut Transaction<'_, Postgres>,
    archive_name: &str,
    mode: &str,
    cursor: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into paperless_sync_state (archive_name, last_sync_at, last_delta_cursor, last_mode, updated_at)
        values ($1, now(), $2, $3, now())
        on conflict (archive_name)
        do update set last_sync_at = excluded.last_sync_at,
                      last_delta_cursor = excluded.last_delta_cursor,
                      last_mode = excluded.last_mode,
                      updated_at = now()
        "#,
    )
    .bind(archive_name)
    .bind(cursor)
    .bind(mode)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn claim_notification_delivery(
    pool: &DbPool,
    event_key: &str,
    cooldown_minutes: i32,
) -> Result<bool> {
    let row = sqlx::query(
        r#"
        insert into notification_state (event_key, last_sent_at, updated_at)
        values ($1, now(), now())
        on conflict (event_key)
        do update set last_sent_at = excluded.last_sent_at,
                      updated_at = now()
        where notification_state.last_sent_at < now() - make_interval(mins => $2)
        returning last_sent_at
        "#,
    )
    .bind(event_key)
    .bind(cooldown_minutes)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

pub async fn record_document_language(
    pool: &DbPool,
    paperless_document_id: i32,
    detection: &LanguageDetection,
    run_id: Option<Uuid>,
    job_id: Option<Uuid>,
    actor: &str,
) -> Result<()> {
    let existing = sqlx::query(
        r#"
        select detected_language, detected_language_confidence, detected_language_source
          from document_inventory
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .fetch_optional(pool)
    .await?;

    let existing_language = existing
        .as_ref()
        .and_then(|row| row.try_get::<Option<String>, _>("detected_language").ok())
        .flatten();
    let existing_confidence = existing
        .as_ref()
        .and_then(|row| {
            row.try_get::<Option<f32>, _>("detected_language_confidence")
                .ok()
        })
        .flatten();
    let should_update = match (&existing_language, existing_confidence) {
        (Some(language), Some(confidence))
            if language == &detection.language && confidence + 0.01 >= detection.confidence =>
        {
            false
        }
        _ => detection.confidence > 0.0 || existing_language.is_none(),
    };
    if !should_update {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        update document_inventory
           set detected_language = $2,
               detected_language_confidence = $3,
               detected_language_source = $4,
               detected_language_updated_at = now(),
               updated_at = now()
         where paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .bind(&detection.language)
    .bind(detection.confidence)
    .bind(&detection.source)
    .execute(&mut *tx)
    .await?;
    append_audit_tx(
        &mut tx,
        AuditEventInput {
            event_type: "document.language_detected".to_owned(),
            actor_type: actor.to_owned(),
            actor_id: None,
            run_id,
            job_id,
            paperless_document_id: Some(paperless_document_id),
            before: Some(json!({
                "language": existing_language,
                "confidence": existing_confidence
            })),
            after: Some(json!({
                "language": detection.language,
                "confidence": detection.confidence,
                "source": detection.source
            })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_allowed_tag_names(pool: &DbPool) -> Result<Vec<String>> {
    let rows =
        sqlx::query("select name from paperless_tags where is_workflow = false order by name")
            .fetch_all(pool)
            .await?;
    rows.into_iter()
        .map(|row| row.try_get("name").context("tag name"))
        .collect()
}

pub async fn list_allowed_named_entities(pool: &DbPool, table: &str) -> Result<Vec<String>> {
    let table = match table {
        "paperless_correspondents" => "paperless_correspondents",
        "paperless_document_types" => "paperless_document_types",
        _ => return Err(anyhow!("unsupported metadata table: {table}")),
    };
    // SAFETY: `table` is matched to a closed allow-list of literal table names above.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "select name from {table} order by name"
    )))
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get("name").context("entity name"))
        .collect()
}

pub async fn list_custom_fields(pool: &DbPool) -> Result<Vec<CustomFieldRecord>> {
    let rows = sqlx::query("select id, name, data_type from paperless_custom_fields order by name")
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(CustomFieldRecord {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                data_type: row.try_get("data_type")?,
            })
        })
        .collect()
}

pub async fn custom_field_ids_for_names(
    pool: &DbPool,
    names: &[String],
) -> Result<Vec<(String, i32, Option<String>)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    // Fold both sides in SQL under the builtin `pg_unicode_fast` collation
    // (PostgreSQL 18) so matching doesn't depend on the database locale: under
    // `C`, `lower('Ä')` stays 'Ä'. Rust's ASCII-only folding never matched
    // catalog names with non-ASCII capitals ("Ärzte"). #409
    let rows = sqlx::query(
        "select name, id, data_type from paperless_custom_fields \
         where lower(name collate pg_unicode_fast) = any(select lower(requested collate pg_unicode_fast) from unnest($1::text[]) as requested) \
         order by name",
    )
    .bind(names)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("name")?,
                row.try_get("id")?,
                row.try_get("data_type")?,
            ))
        })
        .collect()
}

pub async fn tag_ids_for_names(pool: &DbPool, names: &[String]) -> Result<Vec<i32>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    // See `custom_field_ids_for_names` for why both sides fold in SQL. #409
    let rows = sqlx::query(
        "select id from paperless_tags \
         where lower(name collate pg_unicode_fast) = any(select lower(requested collate pg_unicode_fast) from unnest($1::text[]) as requested) \
         order by name",
    )
    .bind(names)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get("id").context("tag id"))
        .collect()
}

/// Like `tag_ids_for_names` but also returns the matched name alongside each id, so callers
/// can diff a requested name list against what was actually known in the local mirror
/// (e.g. to decide whether to create unknown tags in Paperless or drop them).
pub async fn tag_id_pairs_for_names(pool: &DbPool, names: &[String]) -> Result<Vec<(String, i32)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    // See `custom_field_ids_for_names` for why both sides fold in SQL. #409
    let rows = sqlx::query(
        "select name, id from paperless_tags \
         where lower(name collate pg_unicode_fast) = any(select lower(requested collate pg_unicode_fast) from unnest($1::text[]) as requested) \
         order by name",
    )
    .bind(names)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| Ok((row.try_get("name")?, row.try_get("id")?)))
        .collect()
}

pub async fn named_entity_id_for_name(
    pool: &DbPool,
    table: &str,
    name: &str,
) -> Result<Option<i32>> {
    let table = match table {
        "paperless_correspondents" => "paperless_correspondents",
        "paperless_document_types" => "paperless_document_types",
        _ => return Err(anyhow!("unsupported metadata table: {table}")),
    };
    // SAFETY: `table` is matched to a closed allow-list of literal table names above.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "select id from {table} where lower(name) = lower($1)"
    )))
    .bind(name)
    .fetch_optional(pool)
    .await?;
    row.map(|row| row.try_get("id").context("entity id"))
        .transpose()
}

/// Filter shape for `list_inventory` / `count_inventory`. All fields are
/// optional; the empty default matches every row. Built up by the
/// `/api/inventory` handler from query-string parameters and passed through
/// to the SQL layer, which translates the populated fields into WHERE
/// clauses dynamically.
#[derive(Debug, Clone, Default)]
pub struct InventoryQuery {
    pub id: Option<i32>,
    /// ILIKE match on `title` OR `original_file_name`. The handler wraps
    /// the user-supplied string with `%…%` before binding, so callers
    /// pass plain text.
    pub q: Option<String>,
    pub ocr_status: Vec<String>,
    pub metadata_status: Vec<String>,
    pub run_status: Vec<String>,
    /// All listed tag names must be present on `current_tags` (AND).
    pub tags_include: Vec<String>,
    /// None of the listed tag names may be present on `current_tags`.
    pub tags_exclude: Vec<String>,
    pub language: Option<String>,
    /// Inclusive lower bound on the typed `document_date` column. The API
    /// handler parses (and 400s) the raw query string, so the SQL layer only
    /// ever sees a valid date.
    pub date_from: Option<NaiveDate>,
    /// Inclusive upper bound on `document_date`.
    pub date_to: Option<NaiveDate>,
    pub has_error: Option<bool>,
    pub needs_review: Option<bool>,
    /// Paperless correspondent filter (#447).
    pub correspondent: InventoryIdFilter,
    /// Paperless document type filter (#447).
    pub document_type: InventoryIdFilter,
}

/// Match a nullable Paperless object id column against a set of ids and/or
/// "unset" (#447). Empty matches every row; ids and `include_none` are OR-ed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InventoryIdFilter {
    pub ids: Vec<i32>,
    /// Also match rows whose column is null ("without correspondent").
    pub include_none: bool,
}

impl InventoryIdFilter {
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty() && !self.include_none
    }
}

fn push_inventory_id_filter(
    builder: &mut QueryBuilder<Postgres>,
    column: &'static str,
    filter: &InventoryIdFilter,
) {
    match (filter.ids.is_empty(), filter.include_none) {
        (true, false) => {}
        (true, true) => {
            builder.push(" and ").push(column).push(" is null");
        }
        (false, include_none) => {
            builder
                .push(" and (")
                .push(column)
                .push(" = any(")
                .push_bind(filter.ids.clone())
                .push(")");
            if include_none {
                builder.push(" or ").push(column).push(" is null");
            }
            builder.push(")");
        }
    }
}

impl InventoryQuery {
    fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.q.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.ocr_status.is_empty()
            && self.metadata_status.is_empty()
            && self.run_status.is_empty()
            && self.tags_include.is_empty()
            && self.tags_exclude.is_empty()
            && self.language.is_none()
            && self.date_from.is_none()
            && self.date_to.is_none()
            && self.has_error.is_none()
            && self.needs_review.is_none()
            && self.correspondent.is_empty()
            && self.document_type.is_empty()
    }
}

/// Push the `WHERE` predicates derived from an [`InventoryQuery`] onto a
/// `QueryBuilder`. The caller is expected to have appended the table
/// reference (and any leading clauses) before calling. Each pushed
/// predicate is prefixed with ` AND ` so the caller's clause acts as the
/// implicit `1=1`. Returns early without writing anything when the query
/// is empty.
fn push_inventory_filters(builder: &mut QueryBuilder<Postgres>, query: &InventoryQuery) {
    if let Some(id) = query.id {
        builder.push(" and paperless_document_id = ").push_bind(id);
    }
    if let Some(text) = query.q.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let like = format!("%{}%", text);
        builder
            .push(" and (title ilike ")
            .push_bind(like.clone())
            .push(" or original_file_name ilike ")
            .push_bind(like)
            .push(")");
    }
    if !query.ocr_status.is_empty() {
        builder
            .push(" and ocr_status = any(")
            .push_bind(&query.ocr_status)
            .push(")");
    }
    if !query.metadata_status.is_empty() {
        builder
            .push(" and metadata_status = any(")
            .push_bind(&query.metadata_status)
            .push(")");
    }
    if !query.run_status.is_empty() {
        builder
            .push(" and coalesce(current_run_status, '') = any(")
            .push_bind(&query.run_status)
            .push(")");
    }
    if !query.tags_include.is_empty() {
        builder
            .push(" and current_tags @> ")
            .push_bind(&query.tags_include);
    }
    if !query.tags_exclude.is_empty() {
        builder
            .push(" and not (current_tags && ")
            .push_bind(&query.tags_exclude)
            .push(")");
    }
    if let Some(lang) = query.language.as_ref().filter(|s| !s.is_empty()) {
        builder
            .push(" and detected_language = ")
            .push_bind(lang.clone());
    }
    if let Some(from) = query.date_from {
        builder.push(" and document_date >= ").push_bind(from);
    }
    if let Some(to) = query.date_to {
        builder.push(" and document_date <= ").push_bind(to);
    }
    if let Some(has_error) = query.has_error {
        if has_error {
            builder.push(" and last_error is not null");
        } else {
            builder.push(" and last_error is null");
        }
    }
    if let Some(needs_review) = query.needs_review {
        builder.push(" and needs_review = ").push_bind(needs_review);
    }
    push_inventory_id_filter(builder, "correspondent_id", &query.correspondent);
    push_inventory_id_filter(builder, "document_type_id", &query.document_type);
}

pub async fn list_inventory(
    pool: &DbPool,
    query: &InventoryQuery,
    limit: i64,
    offset: i64,
) -> Result<Vec<DocumentInventoryItem>> {
    select_inventory(pool, query, None, limit, offset).await
}

/// Keyset page of the filtered inventory for the export (#447): rows with
/// `paperless_document_id < before_id` (all rows when `None`), in the same
/// descending id order as [`list_inventory`]. Each page is one short query,
/// so a slow export client never pins a pool connection.
pub async fn list_inventory_keyset(
    pool: &DbPool,
    query: &InventoryQuery,
    before_id: Option<i32>,
    limit: i64,
) -> Result<Vec<DocumentInventoryItem>> {
    select_inventory(pool, query, before_id, limit, 0).await
}

async fn select_inventory(
    pool: &DbPool,
    query: &InventoryQuery,
    before_id: Option<i32>,
    limit: i64,
    offset: i64,
) -> Result<Vec<DocumentInventoryItem>> {
    // Correspondent / document type names are resolved per returned row with
    // scalar subqueries on the tiny, primary-keyed paperless_* tables (#447);
    // unlike a join this keeps every filter column unambiguous.
    let mut builder = QueryBuilder::<Postgres>::new(
        "select paperless_document_id, title, original_file_name, current_tags, ocr_status, \
         metadata_status, current_run_status, \
         last_run_id, last_error, next_required_stage, needs_review, complete, \
         document_date, detected_language, detected_language_confidence, \
         detected_language_source, last_seen_at, correspondent_id, document_type_id, \
         (select c.name from paperless_correspondents c \
           where c.id = document_inventory.correspondent_id) as correspondent_name, \
         (select t.name from paperless_document_types t \
           where t.id = document_inventory.document_type_id) as document_type_name \
         from document_inventory where 1=1",
    );
    push_inventory_filters(&mut builder, query);
    if let Some(before_id) = before_id {
        builder
            .push(" and paperless_document_id < ")
            .push_bind(before_id);
    }
    builder
        .push(" order by paperless_document_id desc limit ")
        .push_bind(limit)
        .push(" offset ")
        .push_bind(offset);
    let rows = builder.build().fetch_all(pool).await?;

    rows.into_iter()
        .map(|row| {
            Ok(DocumentInventoryItem {
                paperless_document_id: row.try_get("paperless_document_id")?,
                title: row.try_get("title")?,
                original_file_name: row.try_get("original_file_name")?,
                current_tags: row.try_get("current_tags")?,
                ocr_status: row.try_get("ocr_status")?,
                metadata_status: row.try_get("metadata_status")?,
                current_run_status: row.try_get("current_run_status")?,
                last_run_id: row.try_get("last_run_id")?,
                last_error: row.try_get("last_error")?,
                next_required_stage: row.try_get("next_required_stage")?,
                needs_review: row.try_get("needs_review")?,
                complete: row.try_get("complete")?,
                document_date: row.try_get("document_date")?,
                detected_language: row.try_get("detected_language")?,
                detected_language_confidence: row.try_get("detected_language_confidence")?,
                detected_language_source: row.try_get("detected_language_source")?,
                last_seen_at: row.try_get("last_seen_at")?,
                correspondent_id: row.try_get("correspondent_id")?,
                correspondent_name: row.try_get("correspondent_name")?,
                document_type_id: row.try_get("document_type_id")?,
                document_type_name: row.try_get("document_type_name")?,
            })
        })
        .collect()
}

/// Count inventory rows matching the same filters as `list_inventory`. Used
/// by `/api/inventory` to compute `total` so the frontend's "Showing N of M"
/// counter reflects the FILTERED total, not the unfiltered table size.
pub async fn count_inventory(pool: &DbPool, query: &InventoryQuery) -> Result<i64> {
    if query.is_empty() {
        // Fast path for the unfiltered case — avoids the QueryBuilder
        // overhead and lets Postgres serve the answer from the relation
        // count cache rather than scanning.
        let count: i64 = sqlx::query_scalar("select count(*)::bigint from document_inventory")
            .fetch_one(pool)
            .await?;
        return Ok(count);
    }
    let mut builder =
        QueryBuilder::<Postgres>::new("select count(*)::bigint from document_inventory where 1=1");
    push_inventory_filters(&mut builder, query);
    let count: i64 = builder.build_query_scalar().fetch_one(pool).await?;
    Ok(count)
}

/// A named inventory filter, private to the user who saved it (#447).
/// `query` is the canonical `/api/inventory` filter query string (no
/// leading `?`), validated by the API before it is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventorySavedView {
    pub id: Uuid,
    pub name: String,
    pub query: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Maximum saved views per user (#447).
pub const INVENTORY_SAVED_VIEW_LIMIT: i64 = 50;

/// Expected, client-caused outcomes of saved-view writes (#447).
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum InventoryViewError {
    #[error("saved view not found")]
    NotFound,
    #[error("a saved view with this name already exists")]
    DuplicateName,
    #[error("saved view limit reached")]
    LimitReached,
}

fn saved_view_from_row(row: &PgRow) -> Result<InventorySavedView> {
    Ok(InventorySavedView {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        query: row.try_get("query")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn map_saved_view_unique_violation(error: sqlx::Error) -> anyhow::Error {
    match &error {
        sqlx::Error::Database(db)
            if db.code().as_deref() == Some("23505")
                && db.constraint() == Some("inventory_saved_views_user_name_idx") =>
        {
            anyhow::Error::new(InventoryViewError::DuplicateName)
        }
        _ => error.into(),
    }
}

pub async fn list_inventory_views(pool: &DbPool, user_id: Uuid) -> Result<Vec<InventorySavedView>> {
    let rows = sqlx::query(
        "select id, name, query, created_at, updated_at from inventory_saved_views \
         where user_id = $1 order by lower(name), id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(saved_view_from_row).collect()
}

/// Insert a view unless the user already has [`INVENTORY_SAVED_VIEW_LIMIT`]
/// views. A per-user transaction-scoped advisory lock serialises concurrent
/// saves, so the count check cannot be raced past the limit.
pub async fn create_inventory_view(
    pool: &DbPool,
    user_id: Uuid,
    name: &str,
    query: &str,
) -> Result<InventorySavedView> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "select pg_advisory_xact_lock(hashtextextended('inventory_saved_views:' || $1::text, 0))",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query(
        r#"
        insert into inventory_saved_views (user_id, name, query)
        select $1, $2, $3
         where (select count(*) from inventory_saved_views where user_id = $1) < $4
        returning id, name, query, created_at, updated_at
        "#,
    )
    .bind(user_id)
    .bind(name)
    .bind(query)
    .bind(INVENTORY_SAVED_VIEW_LIMIT)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_saved_view_unique_violation)?;
    let Some(row) = row else {
        return Err(InventoryViewError::LimitReached.into());
    };
    let view = saved_view_from_row(&row)?;
    tx.commit().await?;
    Ok(view)
}

/// Rename and/or re-point one of the user's own views. Another user's view
/// id answers NotFound, never revealing that it exists.
pub async fn update_inventory_view(
    pool: &DbPool,
    user_id: Uuid,
    view_id: Uuid,
    name: &str,
    query: &str,
) -> Result<InventorySavedView> {
    let row = sqlx::query(
        r#"
        update inventory_saved_views
           set name = $3, query = $4, updated_at = now()
         where id = $1 and user_id = $2
        returning id, name, query, created_at, updated_at
        "#,
    )
    .bind(view_id)
    .bind(user_id)
    .bind(name)
    .bind(query)
    .fetch_optional(pool)
    .await
    .map_err(map_saved_view_unique_violation)?;
    match row {
        Some(row) => saved_view_from_row(&row),
        None => Err(InventoryViewError::NotFound.into()),
    }
}

pub async fn delete_inventory_view(pool: &DbPool, user_id: Uuid, view_id: Uuid) -> Result<()> {
    let deleted = sqlx::query("delete from inventory_saved_views where id = $1 and user_id = $2")
        .bind(view_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(InventoryViewError::NotFound.into());
    }
    Ok(())
}

/// One `{id, name}` entry of a synced Paperless metadata mirror table. #420
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperlessNamedOption {
    pub id: i32,
    pub name: String,
}

/// Synced correspondents (local mirror, no Paperless round-trip), ordered by
/// name. Returns at most `limit` rows. #420
pub async fn list_paperless_correspondent_options(
    pool: &DbPool,
    limit: i64,
) -> Result<Vec<PaperlessNamedOption>> {
    list_paperless_named_options(
        pool,
        "select id, name from paperless_correspondents order by lower(name), id limit $1",
        limit,
    )
    .await
}

/// Synced document types (local mirror), ordered by name. #420
pub async fn list_paperless_document_type_options(
    pool: &DbPool,
    limit: i64,
) -> Result<Vec<PaperlessNamedOption>> {
    list_paperless_named_options(
        pool,
        "select id, name from paperless_document_types order by lower(name), id limit $1",
        limit,
    )
    .await
}

async fn list_paperless_named_options(
    pool: &DbPool,
    sql: &'static str,
    limit: i64,
) -> Result<Vec<PaperlessNamedOption>> {
    let rows = sqlx::query(sql).bind(limit.max(1)).fetch_all(pool).await?;
    rows.into_iter()
        .map(|row| {
            Ok(PaperlessNamedOption {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
            })
        })
        .collect()
}

/// Maximum number of duplicate groups returned by [`list_inventory_duplicates`].
/// Keeps the read-only dedup endpoint cheap and the payload bounded; the
/// caller logs when the result is truncated at this cap.
pub const DUPLICATE_GROUP_LIMIT: i64 = 200;

/// Group `document_inventory` rows by `ocr_content_hash`, returning every hash
/// shared by more than one document (#216 dedup view). Rows with a null hash
/// are excluded. When more than [`DUPLICATE_GROUP_LIMIT`] groups exist the
/// largest (most-duplicated) groups are kept; the returned groups themselves
/// are ordered by hash.
pub async fn list_inventory_duplicates(pool: &DbPool) -> Result<Vec<DuplicateGroup>> {
    let rows = sqlx::query(
        r#"
        select ocr_content_hash as hash,
               paperless_document_id,
               title
          from document_inventory
         where ocr_content_hash is not null
           and ocr_content_hash in (
                 select ocr_content_hash
                   from document_inventory
                  where ocr_content_hash is not null
                  group by ocr_content_hash
                 having count(*) > 1
                  order by count(*) desc
                  limit $1
               )
         order by ocr_content_hash, paperless_document_id
        "#,
    )
    .bind(DUPLICATE_GROUP_LIMIT)
    .fetch_all(pool)
    .await?;

    let mut groups: Vec<DuplicateGroup> = Vec::new();
    for row in rows {
        let hash: String = row.try_get("hash")?;
        let document = DuplicateDocument {
            paperless_document_id: row.try_get("paperless_document_id")?,
            title: row.try_get("title")?,
        };
        match groups.last_mut() {
            Some(group) if group.hash == hash => group.documents.push(document),
            _ => groups.push(DuplicateGroup {
                hash,
                documents: vec![document],
            }),
        }
    }
    Ok(groups)
}
