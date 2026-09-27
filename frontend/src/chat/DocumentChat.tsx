import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Check, ExternalLink, MessageSquare, Pencil, Send, Trash2, X } from 'lucide-react';
import { api, DocumentChatMessage, DocumentChatSession, DocumentChatSource } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { useConfirm } from '../lib/ConfirmDialog';
import { replaceLocation, routePath } from '../lib/router';
import { useToast } from '../lib/toast';
import { Button, PageHeader, localizedErrorMessage, run } from '../lib/ui';
import { useResource } from '../lib/useResource';
import { Markdown, safeHttpUrl, type DocumentUrl } from './markdown';

const NO_SESSIONS: DocumentChatSession[] = [];
const NO_MESSAGES: DocumentChatMessage[] = [];

type PendingAnswer = {
  /** null while the session is still being created. */
  sessionId: string | null;
  question: string;
  /** Streamed answer so far (#449). */
  answer: string;
  sources: DocumentChatSource[];
};

/**
 * Document ids handed over from the Inventory ("Ask in chat", #449) via
 * `/chat?documents=1,2`. Read once on mount; the query is then removed so a
 * reload does not re-apply it.
 */
function initialDocumentIds(): string {
  if (typeof window === 'undefined') return '';
  const ids = parseDocumentIds(new URLSearchParams(window.location.search).get('documents') ?? '');
  return ids ? ids.join(', ') : '';
}

export function DocumentChat({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime } = useI18n();
  const toast = useToast();
  const { confirm, dialog: confirmDialog } = useConfirm();
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  // Mirrors activeSessionId synchronously (a click updates it before React
  // commits), so sendMessage can tell whether the user switched sessions
  // while its request was in flight. (#272, #286)
  const activeSessionIdRef = useRef<string | null>(null);
  const [sessionTitle, setSessionTitle] = useState(t('chat.default_session_title'));
  const [question, setQuestion] = useState('');
  const [documentIds, setDocumentIds] = useState(initialDocumentIds);
  // "Ask in chat" starts a fresh conversation instead of continuing the
  // most recent session.
  const startFreshRef = useRef(documentIds !== '');
  const [busy, setBusy] = useState(false);
  const [renaming, setRenaming] = useState<{ id: string; title: string } | null>(null);
  const onError = (err: unknown) => setError(localizedErrorMessage(err, t));
  // Aborts an in-flight stream when the page unmounts; the backend still
  // stores the answer.
  const streamAbortRef = useRef<AbortController | null>(null);

  useEffect(() => {
    if (startFreshRef.current) replaceLocation(routePath('chat'));
    return () => streamAbortRef.current?.abort();
  }, []);

  // Message state is owned by the newest request for the active session:
  // useResource aborts and ignores older requests (A -> B -> A, a late
  // post-send refresh after a switch), including their errors. (#286, #444)
  const messagesResource = useResource(
    async (signal) => (activeSessionId ? (await api.chatMessages(activeSessionId, { signal })).items : NO_MESSAGES),
    [activeSessionId],
    { onError }
  );
  const messages = messagesResource.data ?? NO_MESSAGES;
  // #428: optimistic user message + "thinking" indicator while the answer is
  // generated; #449: the streamed answer grows in place.
  const [pending, setPending] = useState<PendingAnswer | null>(null);
  // Auto-scroll only while the reader is at (or near) the bottom, so reading
  // older messages is not interrupted by a new answer.
  const logRef = useRef<HTMLDivElement>(null);
  const stickToBottomRef = useRef(true);

  const selectSession = (sessionId: string | null) => {
    if (activeSessionIdRef.current === sessionId) return;
    activeSessionIdRef.current = sessionId;
    // A response for the previous session landing before the switch commits
    // must not be shown.
    messagesResource.cancel();
    setActiveSessionId(sessionId);
  };

  const sessionsResource = useResource((signal) => api.chatSessions({ signal }), [], {
    onError,
    onSuccess: (data) => {
      if (activeSessionIdRef.current === null && !startFreshRef.current && data.items[0]) selectSession(data.items[0].id);
    }
  });
  const sessions = sessionsResource.data?.items ?? NO_SESSIONS;
  const paperlessBase = safeHttpUrl(sessionsResource.data?.paperless_base ?? '')?.replace(/\/+$/, '') ?? null;
  const documentUrl: DocumentUrl = (id) => (paperlessBase ? `${paperlessBase}/documents/${id}/details` : null);
  const loadSessions = sessionsResource.reload;
  const updateSessions = (update: (current: DocumentChatSession[]) => DocumentChatSession[]) =>
    sessionsResource.setData((current) => ({
      paperless_base: current?.paperless_base ?? '',
      items: update(current?.items ?? NO_SESSIONS)
    }));

  const createSession = async () => {
    const created = await api.createChatSession(sessionTitle);
    const now = new Date().toISOString();
    updateSessions((current) => [{ id: created.id, title: created.title, created_at: now, updated_at: now }, ...current]);
    startFreshRef.current = false;
    selectSession(created.id);
  };

  const renameSession = async (sessionId: string, title: string) => {
    const renamed = await api.renameChatSession(sessionId, title);
    updateSessions((current) => current.map((item) => (item.id === sessionId ? { ...item, title: renamed.title } : item)));
    setRenaming(null);
    toast.notify({ tone: 'success', message: t('chat.renamed') });
  };

  const deleteSession = async (session: DocumentChatSession) => {
    const confirmed = await confirm({
      title: t('chat.delete_confirm_title'),
      description: t('chat.delete_confirm_description', { title: session.title }),
      confirmLabel: t('chat.delete_confirm'),
      tone: 'danger'
    });
    if (!confirmed) return;
    await api.deleteChatSession(session.id);
    const remaining = sessions.filter((item) => item.id !== session.id);
    updateSessions(() => remaining);
    if (activeSessionIdRef.current === session.id) selectSession(remaining[0]?.id ?? null);
    toast.notify({ tone: 'success', message: t('chat.deleted') });
  };

  const sendMessage = async () => {
    const trimmed = question.trim();
    if (!trimmed) return;
    const ids = parseDocumentIds(documentIds);
    if (ids === false) {
      setError(t('chat.error_invalid_document_ids'));
      return;
    }

    // Show the question immediately and follow it to the bottom (#428).
    setQuestion('');
    stickToBottomRef.current = true;
    setPending({ sessionId: activeSessionId, question: trimmed, answer: '', sources: [] });
    const abort = new AbortController();
    streamAbortRef.current = abort;
    try {
      const sessionId = activeSessionId ?? (await api.createChatSession(chatTitleFromQuestion(trimmed))).id;
      if (!activeSessionId) {
        startFreshRef.current = false;
        setPending((current) => (current ? { ...current, sessionId } : current));
        selectSession(sessionId);
        await loadSessions();
      }

      // #449: stream the answer; the provider is only ever called by the API.
      await api.streamChatMessage(
        sessionId,
        { question: trimmed, document_ids: ids, max_sources: 6 },
        {
          onSources: (sources) => setPending((current) => (current ? { ...current, sources } : current)),
          onDelta: (text) => setPending((current) => (current ? { ...current, answer: current.answer + text } : current))
        },
        { signal: abort.signal }
      );
      await loadSessions();
      // Avoid an unnecessary request after a switch; a switch during this
      // refresh supersedes it inside useResource. (#286)
      if (activeSessionIdRef.current === sessionId) {
        await messagesResource.reload();
      }
    } catch (err) {
      if (abort.signal.aborted) return;
      // Give the unsent question back unless the user already typed a new one.
      setQuestion((current) => current || trimmed);
      throw err;
    } finally {
      if (streamAbortRef.current === abort) streamAbortRef.current = null;
      setPending(null);
    }
  };

  const showPending = pending !== null && pending.sessionId === activeSessionId;
  const streaming = showPending && pending !== null && pending.answer !== '';

  const onLogScroll = () => {
    const log = logRef.current;
    if (!log) return;
    stickToBottomRef.current = log.scrollHeight - log.scrollTop - log.clientHeight < 48;
  };

  // Keep the newest message in view when the reader is following along.
  useLayoutEffect(() => {
    const log = logRef.current;
    if (!log || !stickToBottomRef.current) return;
    log.scrollTop = log.scrollHeight;
  }, [messages, showPending, pending?.answer, activeSessionId]);

  // A session switch starts at the latest message again.
  useEffect(() => {
    stickToBottomRef.current = true;
  }, [activeSessionId]);

  const scopedIds = parseDocumentIds(documentIds);

  return (
    <section className="page chat-page">
      <PageHeader title={t('chat.title')} />
      <div className="chat-layout">
        <aside className="chat-sessions" aria-label={t('chat.sessions_label')}>
          <form
            className="chat-session-form"
            onSubmit={(event) => {
              event.preventDefault();
              void run(setBusy, setError, createSession, t);
            }}
          >
            <input value={sessionTitle} onChange={(event) => setSessionTitle(event.target.value)} aria-label={t('chat.new_chat')} />
            <Button variant="secondary" icon={<MessageSquare size={16} />} title={t('chat.new_chat')} aria-label={t('chat.new_chat')} disabled={busy} />
          </form>
          <ul className="chat-session-list">
            {sessions.map((session) =>
              renaming?.id === session.id ? (
                <li key={session.id}>
                  <form
                    className="chat-session-rename"
                    onSubmit={(event) => {
                      event.preventDefault();
                      void run(setBusy, setError, () => renameSession(session.id, renaming.title), t);
                    }}
                  >
                    <input
                      value={renaming.title}
                      autoFocus
                      maxLength={200}
                      aria-label={t('chat.rename_label')}
                      onChange={(event) => setRenaming({ id: session.id, title: event.target.value })}
                      onKeyDown={(event) => {
                        if (event.key === 'Escape') setRenaming(null);
                      }}
                    />
                    <button type="submit" className="ghost-button" title={t('chat.rename_save')} aria-label={t('chat.rename_save')} disabled={busy || !renaming.title.trim()}>
                      <Check size={16} aria-hidden="true" />
                    </button>
                    <button type="button" className="ghost-button" title={t('generic.cancel')} aria-label={t('generic.cancel')} onClick={() => setRenaming(null)}>
                      <X size={16} aria-hidden="true" />
                    </button>
                  </form>
                </li>
              ) : (
                <li key={session.id} className={session.id === activeSessionId ? 'active' : ''}>
                  <button
                    type="button"
                    className="chat-session-select"
                    title={session.title}
                    aria-current={session.id === activeSessionId ? 'true' : undefined}
                    onClick={() => {
                      startFreshRef.current = false;
                      selectSession(session.id);
                    }}
                  >
                    <span>{session.title}</span>
                    <small>{formatDateTime(session.updated_at)}</small>
                  </button>
                  <span className="chat-session-actions">
                    <button
                      type="button"
                      className="ghost-button"
                      title={t('chat.rename')}
                      aria-label={t('chat.rename_session', { title: session.title })}
                      onClick={() => setRenaming({ id: session.id, title: session.title })}
                    >
                      <Pencil size={14} aria-hidden="true" />
                    </button>
                    <button
                      type="button"
                      className="ghost-button"
                      title={t('chat.delete_confirm')}
                      aria-label={t('chat.delete_session', { title: session.title })}
                      disabled={busy}
                      onClick={() => void run(setBusy, setError, () => deleteSession(session), t)}
                    >
                      <Trash2 size={14} aria-hidden="true" />
                    </button>
                  </span>
                </li>
              )
            )}
          </ul>
        </aside>
        <div className="chat-panel">
          {/* #428: transcript is a polite live log, so new answers (and the
              thinking indicator) are announced without stealing focus.
              #449: aria-busy while an answer streams, so it is announced
              once complete instead of token by token. */}
          <div
            className="chat-messages"
            ref={logRef}
            role="log"
            aria-live="polite"
            aria-relevant="additions"
            aria-busy={streaming || undefined}
            aria-label={t('chat.transcript')}
            onScroll={onLogScroll}
          >
            {messages.length === 0 && !showPending && <div className="empty-state">{t('chat.no_messages')}</div>}
            {messages.map((message) => (
              <article className={`chat-message ${message.role}`} key={message.id}>
                <header>
                  <strong>{message.role === 'assistant' ? t('chat.role_assistant') : t('chat.role_user')}</strong>
                  {message.model && <span>{message.provider} / {message.model}</span>}
                </header>
                {message.role === 'assistant' ? (
                  <Markdown text={message.content} documentUrl={documentUrl} />
                ) : (
                  <p className="chat-question">{message.content}</p>
                )}
                <ChatSources sources={message.sources} documentUrl={documentUrl} keyPrefix={message.id} />
              </article>
            ))}
            {showPending && pending && (
              <>
                <article className="chat-message user pending">
                  <header>
                    <strong>{t('chat.role_user')}</strong>
                  </header>
                  <p className="chat-question">{pending.question}</p>
                </article>
                <article className="chat-message assistant pending">
                  <header>
                    <strong>{t('chat.role_assistant')}</strong>
                  </header>
                  {pending.answer ? (
                    <>
                      <Markdown text={pending.answer} documentUrl={documentUrl} />
                      <p className="chat-thinking">
                        <span className="chat-thinking-dots" aria-hidden="true"><i /><i /><i /></span>
                        {t('chat.streaming')}
                      </p>
                    </>
                  ) : (
                    <p className="chat-thinking">
                      <span className="chat-thinking-dots" aria-hidden="true"><i /><i /><i /></span>
                      {t('chat.thinking')}
                    </p>
                  )}
                  <ChatSources sources={pending.sources} documentUrl={documentUrl} keyPrefix="pending" />
                </article>
              </>
            )}
          </div>
          <form
            className="chat-composer"
            onSubmit={(event) => {
              event.preventDefault();
              void run(setBusy, setError, sendMessage, t);
            }}
          >
            <label>
              {t('chat.document_ids_label')}
              <input value={documentIds} onChange={(event) => setDocumentIds(event.target.value)} placeholder="12, 98" />
            </label>
            <label className="wide">
              {t('chat.question_label')}
              <textarea value={question} onChange={(event) => setQuestion(event.target.value)} required />
            </label>
            <Button variant="primary" icon={<Send size={16} />} title={t('chat.send')} disabled={busy || !question.trim()}>
              {pending ? t('chat.sending') : t('chat.send')}
            </Button>
            {Array.isArray(scopedIds) && (
              <small className="field-hint chat-scope-hint">{t('chat.scope_documents', { ids: scopedIds.map((id) => `#${id}`).join(', ') })}</small>
            )}
          </form>
        </div>
      </div>
      {confirmDialog}
    </section>
  );
}

/**
 * Stored/streamed sources (#449): a link to the Paperless document when the
 * browser-facing Paperless URL is known, plus the retrieved snippet. The
 * browser only follows the link; it never fetches from Paperless itself.
 */
function ChatSources({
  sources,
  documentUrl,
  keyPrefix
}: {
  sources: DocumentChatSource[];
  documentUrl: DocumentUrl;
  keyPrefix: string;
}) {
  const { t } = useI18n();
  if (sources.length === 0) return null;
  return (
    <div className="chat-sources">
      <strong className="chat-sources-title">{t('chat.sources')}</strong>
      {sources.map((source, index) => {
        const href = documentUrl(source.paperless_document_id);
        const label = `${t('chat.source_document', { id: source.paperless_document_id })}${source.title ? ` - ${source.title}` : ''}`;
        return (
          // The link sits next to (not inside) <summary>: interactive content
          // inside a summary is not reachable reliably by assistive tech.
          <div className="chat-source-row" key={`${keyPrefix}-${source.paperless_document_id}-${index}`}>
            <details>
              <summary>{label}</summary>
              <p>{source.snippet}</p>
            </details>
            {href && (
              <a
                className="chat-source-link"
                href={href}
                target="_blank"
                rel="noopener noreferrer"
                title={t('chat.open_in_paperless', { id: source.paperless_document_id })}
                aria-label={t('chat.open_in_paperless', { id: source.paperless_document_id })}
              >
                <ExternalLink size={14} aria-hidden="true" />
              </a>
            )}
          </div>
        );
      })}
    </div>
  );
}

export function parseDocumentIds(value: string): number[] | null | false {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const ids = trimmed.split(',').map((part) => Number(part.trim()));
  if (ids.some((id) => !Number.isInteger(id) || id <= 0)) return false;
  const uniqueIds = Array.from(new Set(ids));
  if (uniqueIds.length > 50) return false;
  return uniqueIds;
}

function chatTitleFromQuestion(question: string) {
  return question.length > 70 ? `${question.slice(0, 67)}...` : question;
}
