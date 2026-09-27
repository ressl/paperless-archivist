import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type ReviewItem } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';
import { isTypingTarget } from './ReviewTriage';

expect.extend(toHaveNoViolations);

// #420 searchable select, #445 preview / keyboard triage / retry.

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      reviews: vi.fn(),
      approveReview: vi.fn(async () => ({ ok: true })),
      rejectReview: vi.fn(async () => ({ ok: true })),
      editReview: vi.fn(async () => ({ ok: true })),
      paperlessCorrespondents: vi.fn(async () => ({
        items: [
          { id: 4, name: 'ACME Corp' },
          { id: 9, name: 'Bank of Zurich' }
        ],
        truncated: false
      })),
      paperlessDocumentTypes: vi.fn(async () => ({ items: [{ id: 5, name: 'Invoice' }], truncated: false })),
      reviewRetryOptions: vi.fn(async () => ({
        providers: [
          { name: 'ollama', default_model: 'qwen3:8b' },
          { name: 'cloud', default_model: 'gpt-large' }
        ],
        default_provider: 'ollama',
        prompts: [
          { id: 'p-2', name: 'metadata-default', version: 2, active: true, created_at: '2026-09-01T00:00:00Z' },
          { id: 'p-1', name: 'metadata-default', version: 1, active: false, created_at: '2026-08-01T00:00:00Z' }
        ]
      })),
      retryReview: vi.fn(async () => ({ run_id: 'run-new', rejected_review_ids: ['r-1'] }))
    }
  };
});

function metadataReview(id: string, documentId: number): ReviewItem {
  return {
    id,
    paperless_document_id: documentId,
    stage: 'metadata',
    status: 'pending',
    suggested_patch: {
      correspondent: 9,
      standard_metadata: { field: 'correspondent', suggested_name: 'Bank of Zurich', confidence: 0.6 }
    },
    edited_patch: null,
    validation_warnings: [],
    debug_context: null,
    created_at: '2026-09-01T00:00:00Z'
  };
}

const items = [metadataReview('r-1', 101), metadataReview('r-2', 102)];

async function renderReviews() {
  vi.mocked(api.reviews).mockResolvedValue({ items, total: items.length, has_more: false });
  const { Reviews } = await import('./Reviews');
  const setError = vi.fn();
  const setSuccess = vi.fn();
  const view = render(
    <I18nProvider>
      <Reviews setError={setError} setSuccess={setSuccess} />
    </I18nProvider>
  );
  await screen.findByText('Document 101');
  return { ...view, setError, setSuccess };
}

function card(documentId: number) {
  return screen.getByText(`Document ${documentId}`).closest('article') as HTMLElement;
}

describe('<Reviews> triage features', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
    window.localStorage.clear();
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
  });

  it('picks the correspondent by name and sends the Paperless id (#420)', async () => {
    await renderReviews();
    const combo = await within(card(101)).findByRole('combobox', { name: 'Edit Correspondent' });
    await waitFor(() => expect(combo).toHaveValue('Bank of Zurich'));
    fireEvent.change(combo, { target: { value: 'acme' } });
    expect(combo).toHaveAttribute('aria-expanded', 'true');
    const listbox = within(card(101)).getByRole('listbox', { hidden: false });
    expect(within(listbox).getByRole('option', { name: 'ACME Corp' })).toBeInTheDocument();
    expect(within(listbox).queryByRole('option', { name: 'Bank of Zurich' })).toBeNull();
    // First match is highlighted after typing; Enter picks it.
    fireEvent.keyDown(combo, { key: 'Enter' });
    expect(combo).toHaveAttribute('aria-expanded', 'false');
    expect(combo).toHaveValue('ACME Corp');

    fireEvent.click(within(card(101)).getByRole('button', { name: /Apply edited/ }));
    await waitFor(() => expect(api.editReview).toHaveBeenCalledTimes(1));
    expect(vi.mocked(api.editReview).mock.calls[0]).toEqual(['r-1', { correspondent: 4 }]);
  });

  it('supports arrow keys, "none" and Escape in the select', async () => {
    await renderReviews();
    const combo = await within(card(102)).findByRole('combobox', { name: 'Edit Correspondent' });
    await waitFor(() => expect(combo).toHaveValue('Bank of Zurich'));
    fireEvent.keyDown(combo, { key: 'ArrowDown' });
    expect(combo).toHaveAttribute('aria-expanded', 'true');
    const active = combo.getAttribute('aria-activedescendant');
    expect(active && document.getElementById(active)).toHaveTextContent('Bank of Zurich');
    fireEvent.keyDown(combo, { key: 'Escape' });
    expect(combo).toHaveAttribute('aria-expanded', 'false');
    fireEvent.keyDown(combo, { key: 'ArrowDown' });
    fireEvent.keyDown(combo, { key: 'ArrowUp' });
    fireEvent.keyDown(combo, { key: 'ArrowUp' });
    fireEvent.keyDown(combo, { key: 'ArrowUp' });
    fireEvent.keyDown(combo, { key: 'Enter' });
    expect(combo).toHaveValue('');
    fireEvent.click(within(card(102)).getByRole('button', { name: /Apply edited/ }));
    await waitFor(() => expect(api.editReview).toHaveBeenCalledWith('r-2', { correspondent: null }));
  });

  it('shows the proxied thumbnail and a same-origin preview link (#445)', async () => {
    await renderReviews();
    const preview = within(card(101)).getByRole('link', { name: 'Open document preview in a new tab' });
    expect(preview).toHaveAttribute('href', '/api/reviews/r-1/preview');
    expect(preview).toHaveAttribute('target', '_blank');
    expect(preview).toHaveAttribute('rel', 'noopener noreferrer');
    const image = within(preview).getByRole('img', { name: 'Thumbnail of document 101' });
    expect(image).toHaveAttribute('src', '/api/reviews/r-1/thumbnail');
    fireEvent.error(image);
    expect(within(preview).getByText('Preview unavailable')).toBeInTheDocument();
  });

  it('moves with j/k, approves with a, rejects with r and ignores keys while typing (#445)', async () => {
    await renderReviews();
    await within(card(101)).findByRole('combobox', { name: 'Edit Correspondent' });
    fireEvent.keyDown(window, { key: 'j' });
    expect(document.activeElement).toBe(card(101));
    expect(card(101)).toHaveAttribute('aria-current', 'true');
    fireEvent.keyDown(window, { key: 'j' });
    expect(document.activeElement).toBe(card(102));
    fireEvent.keyDown(window, { key: 'k' });
    expect(document.activeElement).toBe(card(101));

    // Typing in the select must not trigger shortcuts.
    const combo = within(card(101)).getByRole('combobox', { name: 'Edit Correspondent' });
    combo.focus();
    fireEvent.keyDown(combo, { key: 'a' });
    fireEvent.keyDown(combo, { key: 'r' });
    expect(api.approveReview).not.toHaveBeenCalled();
    expect(api.rejectReview).not.toHaveBeenCalled();

    // Modifier combinations pass through.
    card(101).focus();
    fireEvent.keyDown(window, { key: 'a', ctrlKey: true });
    expect(api.approveReview).not.toHaveBeenCalled();

    fireEvent.keyDown(card(101), { key: 'a' });
    await waitFor(() => expect(api.approveReview).toHaveBeenCalledWith('r-1'));
    fireEvent.keyDown(window, { key: 'j' });
    fireEvent.keyDown(window, { key: 'r' });
    await waitFor(() => expect(api.rejectReview).toHaveBeenCalledWith('r-2'));

    // e moves focus into the current card's edit field.
    fireEvent.keyDown(window, { key: 'e' });
    expect(document.activeElement).toBe(within(card(102)).getByRole('combobox', { name: 'Edit Correspondent' }));
  });

  it('opens an accessible shortcut help with ? and closes it with Escape (#445)', async () => {
    const { container } = await renderReviews();
    fireEvent.keyDown(window, { key: '?' });
    const dialog = await screen.findByRole('dialog', { name: 'Keyboard shortcuts' });
    expect(within(dialog).getByText('Approve the current review')).toBeInTheDocument();
    expect(within(dialog).getByText('j')).toBeInTheDocument();
    // Shortcuts are suspended while the dialog is open.
    fireEvent.keyDown(window, { key: 'a' });
    expect(api.approveReview).not.toHaveBeenCalled();
    const results = await axe(document.body, {
      rules: { region: { enabled: false }, 'color-contrast': { enabled: false } }
    });
    expect(results).toHaveNoViolations();
    fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(container.querySelector('.page')).not.toBeNull();
  });

  it('retries a metadata review with the chosen provider, model and prompt version (#445)', async () => {
    const { setSuccess } = await renderReviews();
    fireEvent.click(within(card(101)).getByRole('button', { name: /Retry with/ }));
    const dialog = await screen.findByRole('dialog', { name: 'Retry with another model or prompt' });
    const provider = within(dialog).getByRole('combobox', { name: 'Provider' });
    await waitFor(() => expect(within(provider).getByRole('option', { name: 'cloud' })).toBeInTheDocument());
    fireEvent.change(provider, { target: { value: 'cloud' } });
    expect(within(dialog).getByRole('textbox', { name: 'Model' })).toHaveAttribute('placeholder', 'gpt-large');
    fireEvent.change(within(dialog).getByRole('textbox', { name: 'Model' }), { target: { value: ' gpt-x ' } });
    fireEvent.change(within(dialog).getByRole('combobox', { name: 'Prompt version' }), { target: { value: 'p-1' } });
    const results = await axe(document.body, {
      rules: { region: { enabled: false }, 'color-contrast': { enabled: false } }
    });
    expect(results).toHaveNoViolations();
    fireEvent.click(within(dialog).getByRole('button', { name: /Queue retry/ }));
    await waitFor(() =>
      expect(api.retryReview).toHaveBeenCalledWith('r-1', { provider_name: 'cloud', model: 'gpt-x', prompt_id: 'p-1' })
    );
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(setSuccess).toHaveBeenCalledWith(expect.stringMatching(/Retry queued/));
  });

  it('falls back to the numeric input when the metadata lists cannot be loaded (#420)', async () => {
    vi.mocked(api.paperlessCorrespondents).mockRejectedValueOnce(new Error('forbidden'));
    await renderReviews();
    await waitFor(() =>
      expect(within(card(101)).getByPlaceholderText('Paperless correspondent ID')).toBeInTheDocument()
    );
    expect(within(card(101)).queryByRole('combobox')).toBeNull();
  });

  it('treats text-entry controls as typing targets only', () => {
    const input = document.createElement('input');
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    const textarea = document.createElement('textarea');
    const button = document.createElement('button');
    expect(isTypingTarget(input)).toBe(true);
    expect(isTypingTarget(textarea)).toBe(true);
    expect(isTypingTarget(checkbox)).toBe(false);
    expect(isTypingTarget(button)).toBe(false);
    expect(isTypingTarget(null)).toBe(false);
  });
});
