//! Document chat sessions, retrieval and SSE answer streaming.

use crate::*;

pub(crate) const MAX_CHAT_DOCUMENT_FILTER_IDS: usize = 50;

pub(crate) fn build_document_chat_request(
    provider: &ApiProvider,
    system_prompt: String,
    user_prompt: String,
) -> ChatRequest {
    let mut request = ChatRequest {
        model: provider.model.clone(),
        system_prompt,
        user_prompt,
        temperature: 0.1,
        num_ctx: None,
        response_schema: None,
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    };
    apply_api_provider_tuning(provider, &mut request);
    request
}

pub(crate) async fn ensure_chat_visible(
    pool: &DbPool,
    session_id: Uuid,
    user_id: Option<Uuid>,
    include_all: bool,
) -> ApiResult<()> {
    if document_chat_session_visible(pool, session_id, user_id, include_all).await? {
        Ok(())
    } else {
        Err(ApiError::forbidden("chat session is not available"))
    }
}

/// Externally reachable Paperless base URL for browser deep links (#449):
/// the configured `public_url`, else the internal `base_url`, without a
/// trailing slash. Same rule as the duplicates view. Both URLs are
/// scheme-validated (http/https) when settings are saved.
pub(crate) fn paperless_browser_base(settings: &RuntimeSettings) -> String {
    settings
        .paperless
        .public_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(settings.paperless.base_url.trim())
        .trim_end_matches('/')
        .to_owned()
}

pub(crate) fn chat_title(title: &str) -> String {
    let mut title = title.trim().replace(char::is_whitespace, " ");
    while title.contains("  ") {
        title = title.replace("  ", " ");
    }
    if title.chars().count() > 80 {
        title = title.chars().take(77).collect::<String>();
        title.push_str("...");
    }
    if title.is_empty() {
        "New document chat".to_owned()
    } else {
        title
    }
}

pub(crate) async fn retrieve_document_chat_sources(
    state: &AppState,
    settings: &RuntimeSettings,
    question: &str,
    document_ids: Option<&[i32]>,
    max_sources: usize,
) -> Result<Vec<DocumentChatSource>> {
    let max_sources = max_sources.clamp(1, 10);
    let candidates = search_document_chat_candidates(
        &state.pool,
        question,
        document_ids,
        (max_sources as i64 * 5).max(20),
    )
    .await?;
    let paperless = paperless_client_from_settings(&state.pool, &state.config, settings).await?;
    let terms = document_chat_terms(question);
    let mut sources = Vec::new();

    for candidate in candidates {
        match paperless
            .get_document(candidate.paperless_document_id)
            .await
        {
            Ok(document) => {
                let content = document.content.unwrap_or_default();
                let metadata = chat_candidate_metadata(&candidate);
                let combined = if content.trim().is_empty() {
                    metadata
                } else {
                    format!("{metadata}\n\n{content}")
                };
                let score = score_document_chat_source(&terms, candidate.metadata_score, &combined);
                let snippet = document_chat_snippet(&combined, &terms, 1800);
                if snippet.is_empty() {
                    continue;
                }
                sources.push(DocumentChatSource {
                    paperless_document_id: candidate.paperless_document_id,
                    title: document.title.or(candidate.title),
                    snippet,
                    score,
                    source_kind: "paperless_content".to_owned(),
                });
            }
            Err(error) => {
                warn!(
                    document_id = candidate.paperless_document_id,
                    error = %error,
                    "skipping document chat source because Paperless document fetch failed"
                );
            }
        }
    }

    sources.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sources.truncate(max_sources);
    Ok(sources)
}

pub(crate) fn chat_candidate_metadata(candidate: &DocumentChatCandidate) -> String {
    let mut parts = vec![format!("Document ID: {}", candidate.paperless_document_id)];
    if let Some(title) = candidate.title.as_deref().filter(|title| !title.is_empty()) {
        parts.push(format!("Title: {title}"));
    }
    if let Some(file_name) = candidate
        .original_file_name
        .as_deref()
        .filter(|file_name| !file_name.is_empty())
    {
        parts.push(format!("Original file: {file_name}"));
    }
    if !candidate.current_tags.is_empty() {
        parts.push(format!("Tags: {}", candidate.current_tags.join(", ")));
    }
    parts.join("\n")
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateChatSessionRequest {
    pub(crate) title: Option<String>,
}

pub(crate) async fn chat_sessions(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let user_id = auth.session_user_id()?;
    let include_all = roles_have_permission(&auth.0.roles, Permission::ManageUsers);
    let settings = get_runtime_settings(&state.pool).await?;
    Ok(Json(json!({
        "items": list_document_chat_sessions(&state.pool, Some(user_id), include_all, 100).await?,
        // #449: lets the chat link sources to Paperless without the browser
        // reading /api/settings (chat users may lack ReadSettings).
        "paperless_base": paperless_browser_base(&settings)
    })))
}

pub(crate) async fn create_chat_session(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<CreateChatSessionRequest>,
) -> ApiResult<Json<Value>> {
    let user_id = auth.session_user_id()?;
    let title = chat_title(request.title.as_deref().unwrap_or("New document chat"));
    let id = create_document_chat_session(&state.pool, &title, Some(user_id)).await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "chat.session_created".to_owned(),
            actor_type: auth.0.actor_type,
            actor_id: auth.0.actor_id,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({ "session_id": id, "title": title })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    Ok(Json(json!({ "id": id, "title": title })))
}

pub(crate) async fn chat_messages(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(session_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let user_id = auth.session_user_id()?;
    let include_all = roles_have_permission(&auth.0.roles, Permission::ManageUsers);
    ensure_chat_visible(&state.pool, session_id, Some(user_id), include_all).await?;
    Ok(Json(json!({
        "items": list_document_chat_messages(&state.pool, session_id).await?
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PostChatMessageRequest {
    pub(crate) question: String,
    pub(crate) document_ids: Option<Vec<i32>>,
    pub(crate) max_sources: Option<usize>,
}

/// A validated chat question, shared by the buffered and the streamed
/// message endpoint so both enforce identical rules (#449).
pub(crate) struct PreparedChatQuestion {
    pub(crate) session_id: Uuid,
    pub(crate) question: String,
    pub(crate) document_ids: Option<Vec<i32>>,
    pub(crate) max_sources: usize,
    pub(crate) settings: RuntimeSettings,
    pub(crate) provider: ApiProvider,
    pub(crate) actor_type: String,
    pub(crate) actor_id: Option<String>,
}

pub(crate) async fn prepare_chat_question(
    state: &AppState,
    auth: &Authenticated,
    session_id: Uuid,
    request: PostChatMessageRequest,
) -> ApiResult<PreparedChatQuestion> {
    let user_id = auth.session_user_id()?;
    let include_all = roles_have_permission(&auth.0.roles, Permission::ManageUsers);
    ensure_chat_visible(&state.pool, session_id, Some(user_id), include_all).await?;

    let question = request.question.trim();
    if question.chars().count() < 3 {
        return Err(ApiError::bad_request(
            "question must be at least 3 characters",
        ));
    }
    if question.chars().count() > 4000 {
        return Err(ApiError::bad_request(
            "question must be at most 4000 characters",
        ));
    }
    let document_ids = normalize_chat_document_ids(request.document_ids)?;

    let settings = get_runtime_settings(&state.pool).await?;
    let provider = provider_for_default_text(&settings)?;
    Ok(PreparedChatQuestion {
        session_id,
        question: question.to_owned(),
        document_ids,
        max_sources: request.max_sources.unwrap_or(6),
        settings,
        provider,
        actor_type: auth.0.actor_type.clone(),
        actor_id: auth.0.actor_id.clone(),
    })
}

/// Store the question, the answer and its sources, write the audit event and
/// build the response body both message endpoints return (#449).
pub(crate) async fn persist_chat_exchange(
    state: &AppState,
    prepared: &PreparedChatQuestion,
    sources: &[DocumentChatSource],
    response: &AiResponse,
    streamed: bool,
) -> ApiResult<Value> {
    let session_id = prepared.session_id;
    let answer = response.text.clone();
    let provider_name = response.provider.clone();
    let model = response.model.clone();
    let user_message_id = insert_document_chat_message(
        &state.pool,
        session_id,
        "user",
        &prepared.question,
        None,
        None,
        Some(json!({ "document_ids": prepared.document_ids })),
    )
    .await?;
    let assistant_message_id = insert_document_chat_message(
        &state.pool,
        session_id,
        "assistant",
        &answer,
        Some(&provider_name),
        Some(&model),
        Some(json!({
            "duration_ms": response.duration_ms,
            "source_count": sources.len(),
            "user_message_id": user_message_id,
            "streamed": streamed
        })),
    )
    .await?;
    insert_document_chat_sources(&state.pool, assistant_message_id, sources).await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "chat.message_created".to_owned(),
            actor_type: prepared.actor_type.clone(),
            actor_id: prepared.actor_id.clone(),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: Some(json!({
                "session_id": session_id,
                "user_message_id": user_message_id,
                "assistant_message_id": assistant_message_id,
                "provider": provider_name,
                "model": model,
                "source_documents": sources.iter().map(|source| source.paperless_document_id).collect::<Vec<_>>()
            })),
            metadata: Some(json!({
                "question_hash": hash_token(&prepared.question),
                "streamed": streamed
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;

    Ok(json!({
        "session_id": session_id,
        "user_message_id": user_message_id,
        "assistant_message_id": assistant_message_id,
        "answer": answer,
        "sources": sources
    }))
}

pub(crate) async fn post_chat_message(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(session_id): Path<Uuid>,
    Json(request): Json<PostChatMessageRequest>,
) -> ApiResult<Json<Value>> {
    let prepared = prepare_chat_question(&state, &auth, session_id, request).await?;
    let sources = retrieve_document_chat_sources(
        &state,
        &prepared.settings,
        &prepared.question,
        prepared.document_ids.as_deref(),
        prepared.max_sources,
    )
    .await?;
    let prompt = build_document_chat_prompt(&prepared.question, &sources);
    let response = chat_with_api_provider(
        &state,
        &prepared.provider,
        build_document_chat_request(&prepared.provider, prompt.system_prompt, prompt.user_prompt),
    )
    .await?;
    Ok(Json(
        persist_chat_exchange(&state, &prepared, &sources, &response, false).await?,
    ))
}

/// One server-sent event of a streamed chat answer (#449).
#[derive(Debug)]
pub(crate) enum ChatStreamEvent {
    /// The retrieved sources, sent before the provider is called.
    Sources(Vec<DocumentChatSource>),
    /// A piece of answer text.
    Delta(String),
    /// The stored exchange; same body as `POST .../messages`.
    Done(Value),
    /// The request failed after the stream started.
    Error(String),
}

impl ChatStreamEvent {
    /// Every event carries compact JSON, so multi-line answer text never
    /// breaks the `data:` framing.
    pub(crate) fn to_sse(&self) -> axum::response::sse::Event {
        let (name, data) = match self {
            Self::Sources(sources) => ("sources", json!({ "sources": sources })),
            Self::Delta(text) => ("delta", json!({ "text": text })),
            Self::Done(body) => ("done", body.clone()),
            Self::Error(message) => ("error", json!({ "error": message })),
        };
        axum::response::sse::Event::default()
            .event(name)
            .data(data.to_string())
    }
}

/// `POST /api/chat/sessions/{id}/messages/stream` (#449): same request and
/// validation as the buffered endpoint, but the answer arrives as
/// `text/event-stream` (`sources`, `delta`*, then `done` or `error`).
/// Validation and permission failures are ordinary JSON errors before the
/// stream starts. The provider is still called only by the API; the answer is
/// generated in a detached task, so it is stored (and its cost accounted)
/// even when the browser disconnects mid-stream.
pub(crate) async fn post_chat_message_stream(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(session_id): Path<Uuid>,
    Json(request): Json<PostChatMessageRequest>,
) -> ApiResult<Response> {
    use tokio_stream::StreamExt as _;

    let prepared = prepare_chat_question(&state, &auth, session_id, request).await?;
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel::<ChatStreamEvent>();
    tokio::spawn(async move {
        if let Err(error) = stream_chat_answer(&state, &prepared, &events).await {
            warn!(
                session_id = %prepared.session_id,
                status = %error.status,
                "streamed document chat failed"
            );
            let _ = events.send(ChatStreamEvent::Error(error.message));
        }
    });
    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(receiver)
        .map(|event| Ok::<_, std::convert::Infallible>(event.to_sse()));
    let mut response = axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response();
    // `no-transform` keeps compressing proxies (Caddy `encode`) from
    // buffering the stream; `X-Accel-Buffering` does the same for nginx.
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    Ok(response)
}

pub(crate) async fn stream_chat_answer(
    state: &AppState,
    prepared: &PreparedChatQuestion,
    events: &tokio::sync::mpsc::UnboundedSender<ChatStreamEvent>,
) -> ApiResult<()> {
    let sources = retrieve_document_chat_sources(
        state,
        &prepared.settings,
        &prepared.question,
        prepared.document_ids.as_deref(),
        prepared.max_sources,
    )
    .await?;
    // A closed receiver only means the browser went away; keep generating so
    // the answer is stored.
    let _ = events.send(ChatStreamEvent::Sources(sources.clone()));
    let prompt = build_document_chat_prompt(&prepared.question, &sources);
    let delta_events = events.clone();
    let mut on_delta = move |text: &str| {
        let _ = delta_events.send(ChatStreamEvent::Delta(text.to_owned()));
    };
    let response = chat_stream_with_api_provider(
        state,
        &prepared.provider,
        build_document_chat_request(&prepared.provider, prompt.system_prompt, prompt.user_prompt),
        &mut on_delta,
    )
    .await?;
    let body = persist_chat_exchange(state, prepared, &sources, &response, true).await?;
    let _ = events.send(ChatStreamEvent::Done(body));
    Ok(())
}

/// Streamed counterpart of [`chat_with_api_provider`] (#449).
pub(crate) async fn chat_stream_with_api_provider(
    state: &AppState,
    provider: &ApiProvider,
    request: ChatRequest,
    on_delta: &mut (dyn FnMut(&str) + Send),
) -> Result<AiResponse> {
    let timeout =
        std::time::Duration::from_secs(u64::from(provider.tuning.request_timeout_seconds));
    match provider.kind {
        AiProviderKind::Ollama => {
            OllamaClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(state, provider).await?,
                timeout,
            )?
            .chat_stream(request, on_delta)
            .await
        }
        AiProviderKind::Openai | AiProviderKind::OpenaiCompatible => {
            OpenAiCompatibleClient::new_with_timeout(
                &provider.name,
                &provider.base_url,
                provider_secret(state, provider).await?,
                timeout,
            )?
            .chat_stream(request, on_delta)
            .await
        }
        AiProviderKind::Anthropic => {
            let secret = provider_secret(state, provider).await?.ok_or_else(|| {
                anyhow!("AI provider '{}' requires an API key secret", provider.name)
            })?;
            AnthropicClient::new_with_timeout(&provider.name, &provider.base_url, secret, timeout)?
                .chat_stream(request, on_delta)
                .await
        }
        AiProviderKind::Mineru => Err(anyhow!(
            "AI provider '{}' uses kind \"mineru\" which is vision-only (OCR); \
             select a text-capable provider for this stage",
            provider.name
        )),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RenameChatSessionRequest {
    pub(crate) title: String,
}

/// `PATCH /api/chat/sessions/{id}` (#449): rename a session the caller may
/// see (its owner, or a user manager). The title is normalized like on
/// creation; an empty title is rejected instead of silently defaulted.
pub(crate) async fn rename_chat_session(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(session_id): Path<Uuid>,
    Json(request): Json<RenameChatSessionRequest>,
) -> ApiResult<Json<Value>> {
    let user_id = auth.session_user_id()?;
    let include_all = roles_have_permission(&auth.0.roles, Permission::ManageUsers);
    ensure_chat_visible(&state.pool, session_id, Some(user_id), include_all).await?;
    if request.title.trim().is_empty() {
        return Err(ApiError::bad_request("title must not be empty"));
    }
    let title = chat_title(&request.title);
    let Some(before) =
        archivist_db::rename_document_chat_session(&state.pool, session_id, &title).await?
    else {
        return Err(ApiError::forbidden("chat session is not available"));
    };
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "chat.session_renamed".to_owned(),
            actor_type: auth.0.actor_type,
            actor_id: auth.0.actor_id,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: Some(json!({ "session_id": session_id, "title": before })),
            after: Some(json!({ "session_id": session_id, "title": title })),
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    Ok(Json(json!({ "id": session_id, "title": title })))
}

/// `DELETE /api/chat/sessions/{id}` (#449): delete a session with its
/// messages and stored sources (FK cascade). The audit event keeps only the
/// id and title, never message content.
pub(crate) async fn delete_chat_session(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(session_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let user_id = auth.session_user_id()?;
    let include_all = roles_have_permission(&auth.0.roles, Permission::ManageUsers);
    ensure_chat_visible(&state.pool, session_id, Some(user_id), include_all).await?;
    let Some(deleted) = archivist_db::delete_document_chat_session(&state.pool, session_id).await?
    else {
        return Err(ApiError::forbidden("chat session is not available"));
    };
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "chat.session_deleted".to_owned(),
            actor_type: auth.0.actor_type,
            actor_id: auth.0.actor_id,
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: Some(json!({
                "session_id": session_id,
                "title": deleted.title,
                "message_count": deleted.message_count
            })),
            after: None,
            metadata: None,
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: None,
            user_agent: None,
        },
    )
    .await?;
    Ok(Json(json!({ "id": session_id, "deleted": true })))
}

pub(crate) fn normalize_chat_document_ids(
    document_ids: Option<Vec<i32>>,
) -> ApiResult<Option<Vec<i32>>> {
    let Some(document_ids) = document_ids else {
        return Ok(None);
    };
    if document_ids.len() > MAX_CHAT_DOCUMENT_FILTER_IDS {
        return Err(ApiError::bad_request(format!(
            "document_ids may contain at most {MAX_CHAT_DOCUMENT_FILTER_IDS} entries"
        )));
    }

    let mut normalized = Vec::with_capacity(document_ids.len());
    for document_id in document_ids {
        if document_id <= 0 {
            return Err(ApiError::bad_request(
                "document_ids must contain positive Paperless document IDs",
            ));
        }
        if !normalized.contains(&document_id) {
            normalized.push(document_id);
        }
    }

    if normalized.is_empty() {
        Ok(None)
    } else {
        Ok(Some(normalized))
    }
}
