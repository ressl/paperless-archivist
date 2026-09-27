import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type ChatStreamHandlers, type DocumentChatMessage } from '../api/client';
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
      streamChatMessage: vi.fn(),
      renameChatSession: vi.fn(),
      deleteChatSession: vi.fn()
    }
  };
});

type StreamResult = Awaited<ReturnType<typeof api.streamChatMessage>>;

const sessions = [
  { id: 'session-a', title: 'Session A', created_at: '2026-09-27T10:00:00Z', updated_at: '2026-09-27T10:00:00Z' },
  { id: 'session-b', title: 'Session B', created_at: '2026-09-27T09:00:00Z', updated_at: '2026-09-27T09:00:00Z' }
];

const stored: DocumentChatMessage[] = [
  {
    id: 'm1',
    session_id: 'session-a',
    role: 'assistant',
    content: 'Invoice **42** is due [doc:42].',
    sources: [{ paperless_document_id: 42, title: 'ACME invoice', snippet: 'Due Friday', score: 1, source_kind: 'paperless_content' }],
    created_at: '2026-09-27T12:00:00Z'
  }
];

async function renderChat() {
  const { DocumentChat } = await import('./DocumentChat');
  const setError = vi.fn();
  const view = render(
    <I18nProvider>
      <DocumentChat setError={setError} />
    </I18nProvider>
  );
  return { setError, view };
}

describe('<DocumentChat> streaming, sources and session management (#449)', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
    window.history.replaceState(null, '', '/chat');
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
    vi.mocked(api.chatSessions).mockResolvedValue({ items: sessions, paperless_base: 'https://paperless.example/' });
    vi.mocked(api.chatMessages).mockResolvedValue({ items: stored });
  });

  it('renders stored answers as Markdown with citations and sources linking to Paperless', async () => {
    const { view } = await renderChat();
    const log = await screen.findByRole('log', { name: 'Chat transcript' });
    expect(await within(log).findByText('42')).toHaveProperty('tagName', 'STRONG');
    expect(within(log).getByRole('link', { name: '#42' })).toHaveAttribute(
      'href',
      'https://paperless.example/documents/42/details'
    );
    const sourceLink = within(log).getByRole('link', { name: 'Open document 42 in Paperless' });
    expect(sourceLink).toHaveAttribute('href', 'https://paperless.example/documents/42/details');
    expect(sourceLink).toHaveAttribute('rel', 'noopener noreferrer');
    expect(await axe(view.container)).toHaveNoViolations();
  });

  it('shows streamed deltas as they arrive and the stored answer afterwards', async () => {
    let handlers: ChatStreamHandlers = {};
    let finish!: (value: StreamResult) => void;
    vi.mocked(api.streamChatMessage).mockImplementation((_id, _input, streamHandlers) => {
      handlers = streamHandlers ?? {};
      return new Promise<StreamResult>((resolve) => {
        finish = resolve;
      });
    });
    await renderChat();
    const log = await screen.findByRole('log', { name: 'Chat transcript' });
    await within(log).findByText('ACME invoice', { exact: false });

    fireEvent.change(screen.getByRole('textbox', { name: 'Question' }), { target: { value: 'When is it due?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    expect(await within(log).findByText('Archivist is thinking…')).toBeInTheDocument();

    act(() => handlers.onSources?.([{ paperless_document_id: 7, title: 'Contract', snippet: 'x', score: 1, source_kind: 'paperless_content' }]));
    act(() => handlers.onDelta?.('It is due '));
    act(() => handlers.onDelta?.('**Friday**.'));
    expect(within(log).getByText('Friday').tagName).toBe('STRONG');
    expect(within(log).getByText('Writing the answer…')).toBeInTheDocument();
    expect(log).toHaveAttribute('aria-busy', 'true');
    expect(within(log).getByText('Document 7 - Contract', { exact: false })).toBeInTheDocument();

    await act(async () =>
      finish({ session_id: 'session-a', user_message_id: 'u', assistant_message_id: 'a', answer: 'It is due Friday.', sources: [] })
    );
    await waitFor(() => expect(within(log).queryByText('Writing the answer…')).not.toBeInTheDocument());
    expect(log).not.toHaveAttribute('aria-busy');
    expect(api.streamChatMessage).toHaveBeenCalledWith(
      'session-a',
      { question: 'When is it due?', document_ids: null, max_sources: 6 },
      expect.any(Object),
      expect.objectContaining({ signal: expect.any(AbortSignal) })
    );
  });

  it('renames a session inline', async () => {
    vi.mocked(api.renameChatSession).mockResolvedValue({ id: 'session-b', title: 'Taxes 2026' });
    await renderChat();
    fireEvent.click(await screen.findByRole('button', { name: 'Rename chat “Session B”' }));
    const input = screen.getByRole('textbox', { name: 'Chat title' });
    fireEvent.change(input, { target: { value: 'Taxes 2026' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save title' }));
    expect(await screen.findByTitle('Taxes 2026')).toBeInTheDocument();
    expect(api.renameChatSession).toHaveBeenCalledWith('session-b', 'Taxes 2026');
  });

  it('deletes a session only after confirmation and selects the next one', async () => {
    vi.mocked(api.deleteChatSession).mockResolvedValue({ id: 'session-a', deleted: true });
    await renderChat();
    fireEvent.click(await screen.findByRole('button', { name: 'Delete chat “Session A”' }));
    const dialog = await screen.findByRole('alertdialog', { name: 'Delete this chat?' });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    expect(api.deleteChatSession).not.toHaveBeenCalled();

    const deleteButton = screen.getByRole('button', { name: 'Delete chat “Session A”' });
    await waitFor(() => expect(deleteButton).toBeEnabled());
    fireEvent.click(deleteButton);
    fireEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Delete chat' }));
    await waitFor(() => expect(screen.queryByTitle('Session A')).not.toBeInTheDocument());
    expect(api.deleteChatSession).toHaveBeenCalledWith('session-a');
    await waitFor(() => expect(api.chatMessages).toHaveBeenLastCalledWith('session-b', expect.anything()));
  });

  it('starts a fresh chat scoped to documents handed over from the Inventory', async () => {
    window.history.replaceState(null, '', '/chat?documents=12,98');
    vi.mocked(api.createChatSession).mockResolvedValue({ id: 'session-new', title: 'About these' });
    vi.mocked(api.streamChatMessage).mockResolvedValue({
      session_id: 'session-new',
      user_message_id: 'u',
      assistant_message_id: 'a',
      answer: 'ok',
      sources: []
    });
    await renderChat();
    expect(await screen.findByRole('textbox', { name: 'Document IDs' })).toHaveValue('12, 98');
    expect(screen.getByText('Questions are limited to documents #12, #98.')).toBeInTheDocument();
    // The query is consumed so a reload does not re-apply it.
    expect(window.location.search).toBe('');
    // No existing session is auto-selected.
    await screen.findByTitle('Session A');
    expect(api.chatMessages).not.toHaveBeenCalled();

    fireEvent.change(screen.getByRole('textbox', { name: 'Question' }), { target: { value: 'About these' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    await waitFor(() =>
      expect(api.streamChatMessage).toHaveBeenCalledWith(
        'session-new',
        expect.objectContaining({ document_ids: [12, 98] }),
        expect.any(Object),
        expect.any(Object)
      )
    );
  });
});
