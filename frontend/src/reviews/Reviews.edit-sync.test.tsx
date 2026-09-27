import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { I18nProvider } from '../i18n/I18nProvider';
import { api, type ReviewItem } from '../api/client';

const review = (id: string, correspondent: string): ReviewItem => ({
  id,
  paperless_document_id: id === 'r-1' ? 1 : 2,
  stage: 'correspondent',
  status: 'pending',
  suggested_patch: { correspondent, standard_metadata: { confidence: 0.9 } },
  edited_patch: null,
  validation_warnings: [],
  debug_context: null,
  created_at: '2026-05-15T09:10:00Z'
});

let queue: ReviewItem[] = [];

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      // Fresh objects on every call, like a real reload.
      reviews: vi.fn(async () => ({ items: queue.map((item) => structuredClone(item)), total: queue.length, has_more: false })),
      approveReview: vi.fn(async () => ({ ok: true }))
    }
  };
});

function card(documentId: number) {
  return screen.getByText(`Document ${documentId}`).closest('article') as HTMLElement;
}

afterEach(() => cleanup());

describe('<ReviewCard> edit state follows the suggestion (#435)', () => {
  it('keeps in-progress edits across a reload with the same suggestion, re-seeds on a new one', async () => {
    queue = [review('r-1', 'Acme'), review('r-2', 'Other')];
    const { Reviews } = await import('./Reviews');
    render(
      <I18nProvider>
        <Reviews setError={() => undefined} setSuccess={() => undefined} />
      </I18nProvider>
    );
    await screen.findByText('Document 1', {}, { timeout: 2_000 });
    const input = within(card(1)).getByRole('textbox', { name: /Edit/ }) as HTMLInputElement;
    expect(input.value).toBe('Acme');
    fireEvent.change(input, { target: { value: 'Acme GmbH' } });

    // Approving another card reloads the queue; r-1's suggestion is unchanged.
    fireEvent.click(within(card(2)).getByRole('button', { name: 'Approve' }));
    await waitFor(() => expect(vi.mocked(api.reviews)).toHaveBeenCalledTimes(2));
    expect((within(card(1)).getByRole('textbox', { name: /Edit/ }) as HTMLInputElement).value).toBe('Acme GmbH');

    // The server now suggests something else for r-1: the form follows it.
    queue = [review('r-1', 'Acme Holding'), review('r-2', 'Other')];
    fireEvent.click(within(card(2)).getByRole('button', { name: 'Approve' }));
    await waitFor(() =>
      expect((within(card(1)).getByRole('textbox', { name: /Edit/ }) as HTMLInputElement).value).toBe('Acme Holding')
    );
  });
});
