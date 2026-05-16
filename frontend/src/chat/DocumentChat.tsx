import { useEffect, useState } from 'react';
import { MessageSquare, Send } from 'lucide-react';
import { api, DocumentChatMessage, DocumentChatSession } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { PageHeader, localizedErrorMessage, run } from '../lib/ui';

export function DocumentChat({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime } = useI18n();
  const [sessions, setSessions] = useState<DocumentChatSession[]>([]);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [messages, setMessages] = useState<DocumentChatMessage[]>([]);
  const [sessionTitle, setSessionTitle] = useState('Document chat');
  const [question, setQuestion] = useState('');
  const [documentIds, setDocumentIds] = useState('');
  const [busy, setBusy] = useState(false);

  const loadSessions = () =>
    api.chatSessions().then((data) => {
      setSessions(data.items);
      setActiveSessionId((current) => current ?? data.items[0]?.id ?? null);
    }).catch((err) => setError(localizedErrorMessage(err, t)));

  const loadMessages = (sessionId: string) =>
    api.chatMessages(sessionId).then((data) => setMessages(data.items)).catch((err) => setError(localizedErrorMessage(err, t)));

  useEffect(() => {
    void loadSessions();
  }, []);

  useEffect(() => {
    if (activeSessionId) {
      void loadMessages(activeSessionId);
    } else {
      setMessages([]);
    }
  }, [activeSessionId]);

  const createSession = async () => {
    const created = await api.createChatSession(sessionTitle);
    setSessions((current) => [{ id: created.id, title: created.title, created_at: new Date().toISOString(), updated_at: new Date().toISOString() }, ...current]);
    setActiveSessionId(created.id);
    setMessages([]);
  };

  const sendMessage = async () => {
    const trimmed = question.trim();
    if (!trimmed) return;
    const ids = parseDocumentIds(documentIds);
    if (ids === false) {
      setError('Document IDs must be up to 50 comma-separated positive numbers');
      return;
    }

    const sessionId = activeSessionId ?? (await api.createChatSession(chatTitleFromQuestion(trimmed))).id;
    if (!activeSessionId) {
      setActiveSessionId(sessionId);
      await loadSessions();
    }

    await api.postChatMessage(sessionId, {
      question: trimmed,
      document_ids: ids,
      max_sources: 6
    });
    setQuestion('');
    await Promise.all([loadSessions(), loadMessages(sessionId)]);
  };

  return (
    <section className="page chat-page">
      <PageHeader title="Document Chat" />
      <div className="chat-layout">
        <aside className="chat-sessions">
          <form
            className="chat-session-form"
            onSubmit={(event) => {
              event.preventDefault();
              void run(setBusy, setError, createSession);
            }}
          >
            <input value={sessionTitle} onChange={(event) => setSessionTitle(event.target.value)} />
            <button title="New chat" disabled={busy}><MessageSquare size={16} /></button>
          </form>
          <div className="chat-session-list">
            {sessions.map((session) => (
              <button
                key={session.id}
                className={session.id === activeSessionId ? 'active' : ''}
                title={session.title}
                onClick={() => setActiveSessionId(session.id)}
              >
                <span>{session.title}</span>
                <small>{formatDateTime(session.updated_at)}</small>
              </button>
            ))}
          </div>
        </aside>
        <div className="chat-panel">
          <div className="chat-messages">
            {messages.length === 0 && <div className="empty-state">No messages</div>}
            {messages.map((message) => (
              <article className={`chat-message ${message.role}`} key={message.id}>
                <header>
                  <strong>{message.role === 'assistant' ? 'Archivist' : 'You'}</strong>
                  {message.model && <span>{message.provider} / {message.model}</span>}
                </header>
                <p>{message.content}</p>
                {message.sources.length > 0 && (
                  <div className="chat-sources">
                    {message.sources.map((source, index) => (
                      <details key={`${message.id}-${source.paperless_document_id}-${index}`}>
                        <summary>
                          Document {source.paperless_document_id}
                          {source.title ? ` - ${source.title}` : ''}
                        </summary>
                        <p>{source.snippet}</p>
                      </details>
                    ))}
                  </div>
                )}
              </article>
            ))}
          </div>
          <form
            className="chat-composer"
            onSubmit={(event) => {
              event.preventDefault();
              void run(setBusy, setError, sendMessage);
            }}
          >
            <label>
              Document IDs
              <input value={documentIds} onChange={(event) => setDocumentIds(event.target.value)} placeholder="12, 98" />
            </label>
            <label className="wide">
              Question
              <textarea value={question} onChange={(event) => setQuestion(event.target.value)} required />
            </label>
            <button className="primary-button" title="Send" disabled={busy || !question.trim()}>
              <Send size={16} /> Send
            </button>
          </form>
        </div>
      </div>
    </section>
  );
}

function parseDocumentIds(value: string): number[] | null | false {
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
