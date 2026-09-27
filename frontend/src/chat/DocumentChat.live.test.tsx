import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type DocumentChatMessage } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

expect.extend(toHaveNoViolations);

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      chatSessions: vi.fn(),
      createChatSession: vi.fn(),
      chatMessages: vi.fn(),
      postChatMessage: vi.fn()
    }
  };
});

type PostResult = Awaited<ReturnType<typeof api.postChatMessage>>;

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const message = (id: string, role: 'user' | 'assistant', content: string): DocumentChatMessage => ({
  id,
  session_id: 'session-a',
  role,
  content,
  sources: [],
  created_at: '2026-09-27T12:00:00Z'
});

const answered = [message('m1', 'user', 'What is due?'), message('m2', 'assistant', 'Invoice 42 is due Friday.')];
const postResult: PostResult = {
  session_id: 'session-a',
  user_message_id: 'm1',
  assistant_message_id: 'm2',
  answer: 'Invoice 42 is due Friday.',
  sources: []
};

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

// jsdom has no layout: give the transcript a controllable scroll geometry.
function scrollGeometry(log: HTMLElement, height: { scroll: number; client: number }) {
  let top = 0;
  Object.defineProperty(log, 'scrollHeight', { configurable: true, get: () => height.scroll });
  Object.defineProperty(log, 'clientHeight', { configurable: true, get: () => height.client });
  Object.defineProperty(log, 'scrollTop', {
    configurable: true,
    get: () => top,
    set: (value: number) => {
      top = value;
    }
  });
  return { get top() { return top; }, set top(value: number) { top = value; } };
}

describe('<DocumentChat> live transcript (#428)', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
    vi.mocked(api.chatSessions).mockResolvedValue({
      items: [{ id: 'session-a', title: 'Session A', created_at: '2026-09-27T10:00:00Z', updated_at: '2026-09-27T10:00:00Z' }]
    });
  });

  it('shows the question and a thinking indicator immediately in a polite log', async () => {
    vi.mocked(api.chatMessages).mockResolvedValueOnce({ items: [] }).mockResolvedValue({ items: answered });
    const post = deferred<PostResult>();
    vi.mocked(api.postChatMessage).mockReturnValue(post.promise);
    await renderChat();

    const log = screen.getByRole('log', { name: 'Chat transcript' });
    expect(log).toHaveAttribute('aria-live', 'polite');

    fireEvent.change(screen.getByRole('textbox', { name: 'Question' }), { target: { value: 'What is due?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));

    expect(await within(log).findByText('What is due?')).toBeInTheDocument();
    expect(within(log).getByText('Archivist is thinking…')).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Question' })).toHaveValue('');
    expect(await axe(log)).toHaveNoViolations();

    await act(async () => post.resolve(postResult));
    expect(await within(log).findByText('Invoice 42 is due Friday.')).toBeInTheDocument();
    expect(within(log).queryByText('Archivist is thinking…')).not.toBeInTheDocument();
    expect(within(log).getAllByText('What is due?')).toHaveLength(1);
  });

  it('restores the question and removes the optimistic message when sending fails', async () => {
    vi.mocked(api.chatMessages).mockResolvedValue({ items: [] });
    vi.mocked(api.postChatMessage).mockRejectedValue(new Error('provider down'));
    const setError = await renderChat();

    fireEvent.change(screen.getByRole('textbox', { name: 'Question' }), { target: { value: 'Will this fail?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));

    await waitFor(() => expect(setError).toHaveBeenCalledWith(expect.stringMatching(/provider down/)));
    expect(screen.getByRole('textbox', { name: 'Question' })).toHaveValue('Will this fail?');
    expect(screen.queryByText('Archivist is thinking…')).not.toBeInTheDocument();
  });

  it('auto-scrolls to new messages unless the reader scrolled up', async () => {
    const post = deferred<PostResult>();
    vi.mocked(api.chatMessages).mockResolvedValueOnce({ items: [] }).mockResolvedValue({ items: answered });
    vi.mocked(api.postChatMessage).mockReturnValue(post.promise);
    await renderChat();
    const log = screen.getByRole('log', { name: 'Chat transcript' });
    const geometry = scrollGeometry(log, { scroll: 1000, client: 200 });

    fireEvent.change(screen.getByRole('textbox', { name: 'Question' }), { target: { value: 'What is due?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    await within(log).findByText('Archivist is thinking…');
    expect(geometry.top).toBe(1000);

    // The reader scrolls up to re-read something while the answer arrives.
    geometry.top = 100;
    fireEvent.scroll(log);
    await act(async () => post.resolve(postResult));
    await within(log).findByText('Invoice 42 is due Friday.');
    expect(geometry.top).toBe(100);
  });
});
