import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen, waitFor } from '@testing-library/react';
import { I18nProvider } from '../i18n/I18nProvider';
import type { ReviewItem } from '../api/client';

const review = (id: string, documentId: number): ReviewItem => ({
  id,
  paperless_document_id: documentId,
  stage: 'title',
  status: 'pending',
  suggested_patch: { title: `Title ${documentId}` },
  edited_patch: null,
  validation_warnings: [],
  debug_context: null,
  created_at: '2026-05-15T09:10:00Z'
});

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      reviews: vi.fn(async () => ({ items: [review('r-1', 1), review('r-2', 2)], total: 2, has_more: false }))
    }
  };
});

async function renderReviews(focusReviewId?: string) {
  const { Reviews } = await import('./Reviews');
  render(
    <I18nProvider>
      <Reviews setError={() => undefined} setSuccess={() => undefined} focusReviewId={focusReviewId} />
    </I18nProvider>
  );
}

afterEach(() => cleanup());

describe('<Reviews> deep link (#424)', () => {
  it('focuses and highlights the linked review once the queue has loaded', async () => {
    await renderReviews('r-2');
    await waitFor(() => expect(document.getElementById('review-r-2')).toHaveFocus(), { timeout: 2_000 });
    expect(document.getElementById('review-r-2')).toHaveClass('review-item--focused');
    expect(document.getElementById('review-r-1')).not.toHaveClass('review-item--focused');
    expect(screen.queryByText(/linked review is not in the pending queue/)).not.toBeInTheDocument();
  });

  it('explains when the linked review is no longer pending', async () => {
    await renderReviews('gone');
    expect(await screen.findByText(/linked review is not in the pending queue/)).toBeInTheDocument();
  });
});
