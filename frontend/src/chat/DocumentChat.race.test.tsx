import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  api,
  type DocumentChatMessage,
  type DocumentChatSession
} from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

const { chatSessions, createChatSession, chatMessages, postChatMessage } = vi.hoisted(() => ({
  chatSessions: vi.fn(),
  createChatSession: vi.fn(),
  chatMessages: vi.fn(),
  postChatMessage: vi.fn()
}));

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      chatSessions,
      createChatSession,
      chatMessages,
      postChatMessage
    }
  };
});

type Deferred<T> = {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason: unknown) => void;
};

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

const sessions: DocumentChatSession[] = [
  {
    id: 'session-a',
    title: 'Session A',
    created_at: '2026-07-17T10:00:00Z',
    updated_at: '2026-07-17T10:00:00Z'
  },
  {
    id: 'session-b',
    title: 'Session B',
    created_at: '2026-07-17T11:00:00Z',
    updated_at: '2026-07-17T11:00:00Z'
  }
];

function message(sessionId: string, content: string): DocumentChatMessage {
  return {
    id: `${sessionId}-${content}`,
    session_id: sessionId,
    role: 'assistant',
    content,
    sources: [],
    created_at: '2026-07-17T12:00:00Z'
  };
}

const messagesResponse = (sessionId: string, content: string) => ({
  items: [message(sessionId, content)]
});

const chatSessionsMock = vi.mocked(api.chatSessions);
const chatMessagesMock = vi.mocked(api.chatMessages);
const postChatMessageMock = vi.mocked(api.postChatMessage);

async function renderChat() {
  const { DocumentChat } = await import('./DocumentChat');
  const setError = vi.fn();
  render(
    <I18nProvider>
      <DocumentChat setError={setError} />
    </I18nProvider>
  );
  await screen.findByTitle('Session A');
  return setError;
}

async function beginPostSendRefresh(
  refresh: Deferred<{ items: DocumentChatMessage[] }>,
  sessionB: Deferred<{ items: DocumentChatMessage[] }>
) {
  let sessionACalls = 0;
  chatMessagesMock.mockImplementation((sessionId: string) => {
    if (sessionId === 'session-a') {
      sessionACalls += 1;
      return sessionACalls === 1
        ? Promise.resolve(messagesResponse('session-a', 'A initial'))
        : refresh.promise;
    }
    return sessionB.promise;
  });

  const setError = await renderChat();
  expect(await screen.findByText('A initial')).toBeInTheDocument();
  fireEvent.change(screen.getByRole('textbox', { name: 'Question' }), {
    target: { value: 'Slow question for session A' }
  });
  fireEvent.click(screen.getByRole('button', { name: 'Send' }));
  await waitFor(() => {
    expect(postChatMessageMock).toHaveBeenCalledTimes(1);
    expect(chatMessagesMock.mock.calls.filter(([id]) => id === 'session-a')).toHaveLength(2);
  });
  fireEvent.click(screen.getByTitle('Session B'));
  await waitFor(() => expect(chatMessagesMock).toHaveBeenCalledWith('session-b'));
  return setError;
}

describe('<DocumentChat> request ownership', () => {
  beforeEach(() => {
    cleanup();
    window.localStorage.clear();
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
    chatSessionsMock.mockReset();
    chatMessagesMock.mockReset();
    postChatMessageMock.mockReset();
    vi.mocked(api.createChatSession).mockReset();
    chatSessionsMock.mockResolvedValue({ items: sessions });
    postChatMessageMock.mockResolvedValue({
      session_id: 'session-a',
      user_message_id: 'user-1',
      assistant_message_id: 'assistant-1',
      answer: 'answer',
      sources: []
    });
  });

  it('keeps B visible when the initial A load resolves after switching sessions', async () => {
    const sessionA = deferred<{ items: DocumentChatMessage[] }>();
    const sessionB = deferred<{ items: DocumentChatMessage[] }>();
    chatMessagesMock.mockImplementation((sessionId: string) =>
      sessionId === 'session-a' ? sessionA.promise : sessionB.promise
    );

    await renderChat();
    await waitFor(() => expect(chatMessagesMock).toHaveBeenCalledWith('session-a'));
    fireEvent.click(screen.getByTitle('Session B'));
    await waitFor(() => expect(chatMessagesMock).toHaveBeenCalledWith('session-b'));

    await act(async () => sessionB.resolve(messagesResponse('session-b', 'B current')));
    expect(await screen.findByText('B current')).toBeInTheDocument();
    await act(async () => sessionA.resolve(messagesResponse('session-a', 'A stale')));

    expect(screen.getByText('B current')).toBeInTheDocument();
    expect(screen.queryByText('A stale')).not.toBeInTheDocument();
  });

  it('rejects old generations when navigation returns from A to B to A', async () => {
    const firstA = deferred<{ items: DocumentChatMessage[] }>();
    const secondA = deferred<{ items: DocumentChatMessage[] }>();
    const sessionB = deferred<{ items: DocumentChatMessage[] }>();
    let sessionACalls = 0;
    chatMessagesMock.mockImplementation((sessionId: string) => {
      if (sessionId === 'session-a') {
        sessionACalls += 1;
        return sessionACalls === 1 ? firstA.promise : secondA.promise;
      }
      return sessionB.promise;
    });

    await renderChat();
    await waitFor(() => expect(chatMessagesMock).toHaveBeenCalledWith('session-a'));
    fireEvent.click(screen.getByTitle('Session B'));
    await waitFor(() => expect(chatMessagesMock).toHaveBeenCalledWith('session-b'));
    fireEvent.click(screen.getByTitle('Session A'));
    await waitFor(() =>
      expect(chatMessagesMock.mock.calls.filter(([id]) => id === 'session-a')).toHaveLength(2)
    );

    await act(async () => secondA.resolve(messagesResponse('session-a', 'A newest')));
    expect(await screen.findByText('A newest')).toBeInTheDocument();
    await act(async () => sessionB.resolve(messagesResponse('session-b', 'B stale')));
    await act(async () => firstA.resolve(messagesResponse('session-a', 'A oldest')));

    expect(screen.getByText('A newest')).toBeInTheDocument();
    expect(screen.queryByText('B stale')).not.toBeInTheDocument();
    expect(screen.queryByText('A oldest')).not.toBeInTheDocument();
  });

  it('keeps B visible when the post-send A refresh resolves late', async () => {
    const refreshA = deferred<{ items: DocumentChatMessage[] }>();
    const sessionB = deferred<{ items: DocumentChatMessage[] }>();
    await beginPostSendRefresh(refreshA, sessionB);

    await act(async () => sessionB.resolve(messagesResponse('session-b', 'B current')));
    expect(await screen.findByText('B current')).toBeInTheDocument();
    await act(async () => refreshA.resolve(messagesResponse('session-a', 'A stale answer')));

    expect(screen.getByText('B current')).toBeInTheDocument();
    expect(screen.queryByText('A stale answer')).not.toBeInTheDocument();
  });

  it('does not surface a stale post-send refresh error in session B', async () => {
    const refreshA = deferred<{ items: DocumentChatMessage[] }>();
    const sessionB = deferred<{ items: DocumentChatMessage[] }>();
    const setError = await beginPostSendRefresh(refreshA, sessionB);

    await act(async () => sessionB.resolve(messagesResponse('session-b', 'B current')));
    expect(await screen.findByText('B current')).toBeInTheDocument();
    setError.mockClear();
    await act(async () => refreshA.reject(new Error('stale A failure')));

    expect(screen.getByText('B current')).toBeInTheDocument();
    expect(setError).not.toHaveBeenCalled();
  });
});
