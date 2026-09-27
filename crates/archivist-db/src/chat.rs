//! Document chat sessions, messages and retrieval candidates.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChatSessionRecord {
    pub id: Uuid,
    pub title: String,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChatMessageRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub role: String,
    pub content: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub metadata: Option<Value>,
    pub sources: Vec<DocumentChatSource>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChatCandidate {
    pub paperless_document_id: i32,
    pub title: Option<String>,
    pub original_file_name: Option<String>,
    pub current_tags: Vec<String>,
    pub metadata_score: f64,
}

pub async fn create_document_chat_session(
    pool: &DbPool,
    title: &str,
    created_by: Option<Uuid>,
) -> Result<Uuid> {
    let id = sqlx::query(
        r#"
        insert into document_chat_sessions (title, created_by)
        values ($1, $2)
        returning id
        "#,
    )
    .bind(title)
    .bind(created_by)
    .fetch_one(pool)
    .await?
    .try_get("id")?;
    Ok(id)
}

pub async fn document_chat_session_visible(
    pool: &DbPool,
    session_id: Uuid,
    user_id: Option<Uuid>,
    include_all: bool,
) -> Result<bool> {
    let row = sqlx::query(
        r#"
        select exists(
          select 1
            from document_chat_sessions
           where id = $1
             and ($2::boolean or created_by = $3)
        ) as visible
        "#,
    )
    .bind(session_id)
    .bind(include_all)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    row.try_get("visible")
        .context("read chat session visibility")
}

pub async fn list_document_chat_sessions(
    pool: &DbPool,
    user_id: Option<Uuid>,
    include_all: bool,
    limit: i64,
) -> Result<Vec<DocumentChatSessionRecord>> {
    let rows = sqlx::query(
        r#"
        select id, title, created_by, created_at, updated_at
          from document_chat_sessions
         where $1::boolean or created_by = $2
         order by updated_at desc
         limit $3
        "#,
    )
    .bind(include_all)
    .bind(user_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(DocumentChatSessionRecord {
                id: row.try_get("id")?,
                title: row.try_get("title")?,
                created_by: row.try_get("created_by")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect()
}

pub async fn insert_document_chat_message(
    pool: &DbPool,
    session_id: Uuid,
    role: &str,
    content: &str,
    provider: Option<&str>,
    model: Option<&str>,
    metadata: Option<Value>,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        r#"
        insert into document_chat_messages (session_id, role, content, provider, model, metadata)
        values ($1, $2, $3, $4, $5, $6)
        returning id
        "#,
    )
    .bind(session_id)
    .bind(role)
    .bind(content)
    .bind(provider)
    .bind(model)
    .bind(metadata)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;

    sqlx::query("update document_chat_sessions set updated_at = now() where id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(id)
}

pub async fn insert_document_chat_sources(
    pool: &DbPool,
    message_id: Uuid,
    sources: &[DocumentChatSource],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    for source in sources {
        sqlx::query(
            r#"
            insert into document_chat_sources (
              message_id, paperless_document_id, title, snippet, score, source_kind
            )
            values ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(message_id)
        .bind(source.paperless_document_id)
        .bind(&source.title)
        .bind(&source.snippet)
        .bind(source.score)
        .bind(&source.source_kind)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Rename a document chat session (#449). Returns the previous title, or
/// `None` when the session does not exist. `updated_at` is left alone so a
/// rename does not reorder the session list (it tracks conversation activity).
pub async fn rename_document_chat_session(
    pool: &DbPool,
    session_id: Uuid,
    title: &str,
) -> Result<Option<String>> {
    let row = sqlx::query(
        r#"
        update document_chat_sessions s
           set title = $2
          from (select id, title from document_chat_sessions where id = $1 for update) previous
         where s.id = previous.id
        returning previous.title as previous_title
        "#,
    )
    .bind(session_id)
    .bind(title)
    .fetch_optional(pool)
    .await?;
    row.map(|row| row.try_get("previous_title"))
        .transpose()
        .context("read previous chat session title")
}

/// What a deleted chat session contained, for the audit trail (#449).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedDocumentChatSession {
    pub title: String,
    pub message_count: i64,
}

/// Delete a document chat session together with its messages and sources
/// (FK `on delete cascade`, migration 0007). Returns `None` when the session
/// does not exist (#449).
pub async fn delete_document_chat_session(
    pool: &DbPool,
    session_id: Uuid,
) -> Result<Option<DeletedDocumentChatSession>> {
    let row = sqlx::query(
        r#"
        with deleted as (
          delete from document_chat_sessions
           where id = $1
          returning id, title
        )
        select deleted.title,
               (select count(*) from document_chat_messages m where m.session_id = deleted.id)
                 as message_count
          from deleted
        "#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| -> Result<DeletedDocumentChatSession> {
        Ok(DeletedDocumentChatSession {
            title: row.try_get("title")?,
            message_count: row.try_get("message_count")?,
        })
    })
    .transpose()
}

pub async fn list_document_chat_messages(
    pool: &DbPool,
    session_id: Uuid,
) -> Result<Vec<DocumentChatMessageRecord>> {
    let rows = sqlx::query(
        r#"
        select id, session_id, role, content, provider, model, metadata, created_at
          from document_chat_messages
         where session_id = $1
         order by created_at
        "#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    let mut messages = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        messages.push(DocumentChatMessageRecord {
            id,
            session_id: row.try_get("session_id")?,
            role: row.try_get("role")?,
            content: row.try_get("content")?,
            provider: row.try_get("provider")?,
            model: row.try_get("model")?,
            metadata: row.try_get("metadata")?,
            sources: list_document_chat_sources(pool, id).await?,
            created_at: row.try_get("created_at")?,
        });
    }
    Ok(messages)
}

pub async fn list_document_chat_sources(
    pool: &DbPool,
    message_id: Uuid,
) -> Result<Vec<DocumentChatSource>> {
    let rows = sqlx::query(
        r#"
        select paperless_document_id, title, snippet, score, source_kind
          from document_chat_sources
         where message_id = $1
         order by score desc, created_at
        "#,
    )
    .bind(message_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(DocumentChatSource {
                paperless_document_id: row.try_get("paperless_document_id")?,
                title: row.try_get("title")?,
                snippet: row.try_get("snippet")?,
                score: row.try_get("score")?,
                source_kind: row.try_get("source_kind")?,
            })
        })
        .collect()
}

pub async fn search_document_chat_candidates(
    pool: &DbPool,
    query: &str,
    document_ids: Option<&[i32]>,
    limit: i64,
) -> Result<Vec<DocumentChatCandidate>> {
    let document_ids = document_ids.map(|ids| ids.to_vec());
    let rows = sqlx::query(
        r#"
        select paperless_document_id, title, original_file_name, current_tags,
               -- Blend the pg_trgm title/file/tags similarity with a
               -- full-text rank over the persisted OCR body (#217). Both
               -- terms live in [0, 1]: pg_trgm similarity is bounded by
               -- construction, and ts_rank with normalization flag 32
               -- (rank/(rank+1)) bounds the body score to [0, 1). Their
               -- sum is clamped back into [0, 1] so the combined score
               -- stays comparable with the metadata-only scores callers
               -- already feed into score_document_chat_source.
               least(
                 1.0,
                 greatest(
                   similarity(coalesce(title, ''), $1),
                   similarity(coalesce(original_file_name, ''), $1),
                   similarity(array_to_string(current_tags, ' '), $1)
                 )
                 + ts_rank(ocr_body_tsv, websearch_to_tsquery('simple', $1), 32)
               )::double precision as metadata_score
          from document_inventory
         where ($2::integer[] is null or paperless_document_id = any($2))
           and (
             $2::integer[] is not null
             or greatest(
               similarity(coalesce(title, ''), $1),
               similarity(coalesce(original_file_name, ''), $1),
               similarity(array_to_string(current_tags, ' '), $1)
             ) > 0
             or ocr_body_tsv @@ websearch_to_tsquery('simple', $1)
           )
         order by metadata_score desc, last_seen_at desc
         limit $3
        "#,
    )
    .bind(query)
    .bind(document_ids)
    .bind(limit.clamp(1, 100))
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(DocumentChatCandidate {
                paperless_document_id: row.try_get("paperless_document_id")?,
                title: row.try_get("title")?,
                original_file_name: row.try_get("original_file_name")?,
                current_tags: row.try_get("current_tags")?,
                metadata_score: row.try_get("metadata_score")?,
            })
        })
        .collect()
}
