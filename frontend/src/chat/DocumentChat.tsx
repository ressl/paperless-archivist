import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { MessageSquare, Send } from 'lucide-react';
import { api, DocumentChatMessage, DocumentChatSession } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { Button, PageHeader, localizedErrorMessage, run } from '../lib/ui';

export function DocumentChat({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime } = useI18n();
  const [sessions, setSessions] = useState<DocumentChatSession[]>([]);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  // Session identity plus a monotonically increasing request generation form
  // the message-state ownership token. The ref is updated synchronously on a
  // click, before React commits the state change, so even a response resolving
  // in that gap is stale. The generation also covers A -> B -> A and two
  // overlapping requests for the same session. (#272, #286)
  const activeSessionIdRef = useRef<string | null>(null);
  const messageRequestGenerationRef = useRef(0);
  const [messages, setMessages] = useState<DocumentChatMessage[]>([]);
  const [sessionTitle, setSessionTitle] = useState(t('chat.default_session_title'));
  const [question, setQuestion] = useState('');
  const [documentIds, setDocumentIds] = useState('');
  const [busy, setBusy] = useState(false);
  // #428: optimistic user message + "thinking" indicator while the answer is
  // generated. `sessionId` null means the session is still being created.
  const [pending, setPending] = useState<{ sessionId: string | null; question: string } | null>(null);
  // Auto-scroll only while the reader is at (or near) the bottom, so reading
  // older messages is not interrupted by a new answer.
  const logRef = useRef<HTMLDivElement>(null);
  const stickToBottomRef = useRef(true);

  const selectSession = (sessionId: string) => {
    if (activeSessionIdRef.current === sessionId) return;
    activeSessionIdRef.current = sessionId;
    messageRequestGenerationRef.current += 1;
    setActiveSessionId(sessionId);
  };

  const loadSessions = () =>
    api.chatSessions().then((data) => {
      setSessions(data.items);
      if (activeSessionIdRef.current === null && data.items[0]) {
        selectSession(data.items[0].id);
      }
    }).catch((err) => setError(localizedErrorMessage(err, t)));

  const loadMessages = async (sessionId: string) => {
    const generation = messageRequestGenerationRef.current + 1;
    messageRequestGenerationRef.current = generation;
    const ownsMessageState = () =>
      activeSessionIdRef.current === sessionId &&
      messageRequestGenerationRef.current === generation;
    try {
      const data = await api.chatMessages(sessionId);
      if (ownsMessageState()) setMessages(data.items);
    } catch (err) {
      if (ownsMessageState()) setError(localizedErrorMessage(err, t));
    }
  };

  useEffect(() => {
    void loadSessions();
  }, []);

  useEffect(() => {
    if (!activeSessionId) {
      messageRequestGenerationRef.current += 1;
      setMessages([]);
      return;
    }
    void loadMessages(activeSessionId);
    return () => {
      messageRequestGenerationRef.current += 1;
    };
  }, [activeSessionId, t]);

  const createSession = async () => {
    const created = await api.createChatSession(sessionTitle);
    setSessions((current) => [{ id: created.id, title: created.title, created_at: new Date().toISOString(), updated_at: new Date().toISOString() }, ...current]);
    selectSession(created.id);
    setMessages([]);
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
    setPending({ sessionId: activeSessionId, question: trimmed });
    try {
      const sessionId = activeSessionId ?? (await api.createChatSession(chatTitleFromQuestion(trimmed))).id;
      if (!activeSessionId) {
        setPending((current) => (current ? { ...current, sessionId } : current));
        selectSession(sessionId);
        await loadSessions();
      }

      await api.postChatMessage(sessionId, {
        question: trimmed,
        document_ids: ids,
        max_sources: 6
      });
      await loadSessions();
      // Avoid an unnecessary request after a switch. `loadMessages` also checks
      // session + generation when the response resolves, which closes the race
      // when the switch happens during this request. (#286)
      if (activeSessionIdRef.current === sessionId) {
        await loadMessages(sessionId);
      }
    } catch (err) {
      // Give the unsent question back unless the user already typed a new one.
      setQuestion((current) => current || trimmed);
      throw err;
    } finally {
      setPending(null);
    }
  };

  const showPending = pending !== null && pending.sessionId === activeSessionId;

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
  }, [messages, showPending, activeSessionId]);

  // A session switch starts at the latest message again.
  useEffect(() => {
    stickToBottomRef.current = true;
  }, [activeSessionId]);

  return (
    <section className="page chat-page">
      <PageHeader title={t('chat.title')} />
      <div className="chat-layout">
        <aside className="chat-sessions">
          <form
            className="chat-session-form"
            onSubmit={(event) => {
              event.preventDefault();
              void run(setBusy, setError, createSession);
            }}
          >
            <input value={sessionTitle} onChange={(event) => setSessionTitle(event.target.value)} aria-label={t('chat.new_chat')} />
            <Button variant="secondary" icon={<MessageSquare size={16} />} title={t('chat.new_chat')} aria-label={t('chat.new_chat')} disabled={busy} />
          </form>
          <div className="chat-session-list">
            {sessions.map((session) => (
              <button
                key={session.id}
                className={session.id === activeSessionId ? 'active' : ''}
                title={session.title}
                onClick={() => selectSession(session.id)}
              >
                <span>{session.title}</span>
                <small>{formatDateTime(session.updated_at)}</small>
              </button>
            ))}
          </div>
        </aside>
        <div className="chat-panel">
          {/* #428: transcript is a polite live log, so new answers (and the
              thinking indicator) are announced without stealing focus. */}
          <div
            className="chat-messages"
            ref={logRef}
            role="log"
            aria-live="polite"
            aria-relevant="additions"
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
                <p>{message.content}</p>
                {message.sources.length > 0 && (
                  <div className="chat-sources">
                    {message.sources.map((source, index) => (
                      <details key={`${message.id}-${source.paperless_document_id}-${index}`}>
                        <summary>
                          {t('chat.source_document', { id: source.paperless_document_id })}
                          {source.title ? ` - ${source.title}` : ''}
                        </summary>
                        <p>{source.snippet}</p>
                      </details>
                    ))}
                  </div>
                )}
              </article>
            ))}
            {showPending && pending && (
              <>
                <article className="chat-message user pending">
                  <header>
                    <strong>{t('chat.role_user')}</strong>
                  </header>
                  <p>{pending.question}</p>
                </article>
                <article className="chat-message assistant pending">
                  <header>
                    <strong>{t('chat.role_assistant')}</strong>
                  </header>
                  <p className="chat-thinking">
                    <span className="chat-thinking-dots" aria-hidden="true"><i /><i /><i /></span>
                    {t('chat.thinking')}
                  </p>
                </article>
              </>
            )}
          </div>
          <form
            className="chat-composer"
            onSubmit={(event) => {
              event.preventDefault();
              void run(setBusy, setError, sendMessage);
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
          </form>
        </div>
      </div>
    </section>
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
