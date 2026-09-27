import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type ReviewItem } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

expect.extend(toHaveNoViolations);

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      reviews: vi.fn(),
      approveReview: vi.fn(async () => ({ ok: true })),
      rejectReview: vi.fn(async () => ({ ok: true })),
      batchReview: vi.fn(async (ids: string[]) => ({ ok: true, succeeded: ids, failed: [] })),
      autoFixReviewPreview: vi.fn(),
      autoFixReviewBulk: vi.fn(async () => ({ applied: 2, rejected: 1, errors: [] })),
      autoFixReviewSingle: vi.fn(async () => ({ action: 'applied' as const }))
    }
  };
});

type ReviewsPage = { items: ReviewItem[]; total: number; has_more: boolean };

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function review(id: string, documentId: number): ReviewItem {
  return {
    id,
    paperless_document_id: documentId,
    stage: 'title',
    status: 'pending',
    suggested_patch: { title: `Title ${documentId}` },
    edited_patch: null,
    validation_warnings: [],
    debug_context: null,
    created_at: '2026-09-01T00:00:00Z'
  };
}

const r1 = review('r-1', 101);
const r2 = review('r-2', 102);
const r3 = review('r-3', 103);
const page = (items: ReviewItem[], total = items.length, hasMore = false): ReviewsPage => ({
  items,
  total,
  has_more: hasMore
});

const reviewsMock = vi.mocked(api.reviews);

async function renderReviews() {
  const { Reviews } = await import('./Reviews');
  const setError = vi.fn();
  const setSuccess = vi.fn();
  render(
    <I18nProvider>
      <Reviews setError={setError} setSuccess={setSuccess} />
    </I18nProvider>
  );
  return { setError, setSuccess };
}

function card(documentId: number) {
  return screen.getByText(`Document ${documentId}`).closest('article') as HTMLElement;
}

describe('<Reviews> behaviour', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('drops a single-decided item from the selection (#418)', async () => {
    reviewsMock.mockResolvedValueOnce(page([r1, r2])).mockResolvedValue(page([r2]));
    await renderReviews();
    await screen.findByText('Document 101');

    fireEvent.click(within(card(101)).getByRole('checkbox'));
    fireEvent.click(within(card(102)).getByRole('checkbox'));
    expect(screen.getByRole('button', { name: /Clear selection/ })).toBeInTheDocument();

    fireEvent.click(within(card(101)).getByRole('button', { name: /^Approve$/ }));
    await waitFor(() => expect(screen.queryByText('Document 101')).not.toBeInTheDocument());

    // r-2 stays selected and is the only visible item, so select-all reads
    // "Clear selection"; the batch sends only the still-pending id.
    expect(screen.getByRole('button', { name: /Clear selection/ })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /Approve selected/ }));
    const dialog = await screen.findByRole('alertdialog', { name: 'Approve 1 review item(s)?' });
    fireEvent.click(within(dialog).getByRole('button', { name: /Approve selected/ }));
    await waitFor(() => expect(api.batchReview).toHaveBeenCalledWith(['r-2'], 'approve'));
  });

  it('keeps the select-all label consistent after a reload removes selected items (#418)', async () => {
    reviewsMock.mockResolvedValueOnce(page([r1, r2])).mockResolvedValue(page([r2, r3]));
    await renderReviews();
    await screen.findByText('Document 101');

    fireEvent.click(screen.getByRole('button', { name: /Select all/ }));
    fireEvent.click(within(card(101)).getByRole('button', { name: /^Reject$/ }));
    await screen.findByText('Document 103');

    // Only r-2 is still selected out of two visible items.
    expect(screen.getByRole('button', { name: /Select all/ })).toBeInTheDocument();
    expect(screen.getByText('1 selected')).toBeInTheDocument();
  });

  it('ignores an older reload that resolves after a newer one (#419)', async () => {
    const slowLoadMore = deferred<ReviewsPage>();
    reviewsMock
      .mockResolvedValueOnce(page([r1, r2], 150, true))
      .mockReturnValueOnce(slowLoadMore.promise)
      .mockResolvedValueOnce(page([r2], 149, false));
    await renderReviews();
    await screen.findByText('Document 101');

    // "Load more" starts a slow request, then approving r-1 starts a newer one.
    fireEvent.click(screen.getByRole('button', { name: /Load more/ }));
    await waitFor(() => expect(reviewsMock).toHaveBeenCalledTimes(2));
    fireEvent.click(within(card(101)).getByRole('button', { name: /^Approve$/ }));
    await waitFor(() => expect(screen.queryByText('Document 101')).not.toBeInTheDocument());

    // The stale snapshot (still containing r-1) arrives last and is dropped.
    await act(async () => slowLoadMore.resolve(page([r1, r2], 150, true)));
    expect(screen.queryByText('Document 101')).not.toBeInTheDocument();
    expect(screen.getByText('Document 102')).toBeInTheDocument();
  });

  it('confirms auto-fix with the same count it then processes (#421)', async () => {
    reviewsMock.mockResolvedValue(page([r1, r2, r3]));
    vi.mocked(api.autoFixReviewPreview).mockResolvedValue({
      total_pending: 3,
      would_apply: 2,
      would_reject: 1,
      sample: []
    });
    const { setSuccess } = await renderReviews();
    await screen.findByText('Document 101');

    fireEvent.click(screen.getByRole('button', { name: /Auto-Fix all/ }));
    const dialog = await screen.findByRole('alertdialog', { name: 'Auto-fix 3 review item(s)?' });
    expect(dialog).toHaveAccessibleDescription(/3 pending review items/);
    expect(within(dialog).getByText(/2 will be applied, 1 will be rejected/)).toBeInTheDocument();
    expect(await axe(dialog)).toHaveNoViolations();

    fireEvent.click(within(dialog).getByRole('button', { name: /Auto-Fix all/ }));
    await waitFor(() => expect(api.autoFixReviewBulk).toHaveBeenCalledWith(3));
    await waitFor(() => expect(setSuccess).toHaveBeenCalledWith(expect.stringMatching(/2 applied, 1 rejected/)));
  });

  it('does not run auto-fix or a batch when the confirmation is cancelled (#417)', async () => {
    reviewsMock.mockResolvedValue(page([r1, r2]));
    vi.mocked(api.autoFixReviewPreview).mockResolvedValue({
      total_pending: 2,
      would_apply: 2,
      would_reject: 0,
      sample: []
    });
    await renderReviews();
    await screen.findByText('Document 101');

    fireEvent.click(screen.getByRole('button', { name: /Auto-Fix all/ }));
    fireEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(api.autoFixReviewBulk).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: /Select all/ }));
    fireEvent.click(screen.getByRole('button', { name: /Reject selected/ }));
    const dialog = await screen.findByRole('alertdialog', { name: 'Reject 2 review item(s)?' });
    fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(dialog).not.toBeInTheDocument();
    expect(api.batchReview).not.toHaveBeenCalled();
  });

  it('distinguishes loading, empty and error states (#429)', async () => {
    const first = deferred<ReviewsPage>();
    reviewsMock.mockReturnValueOnce(first.promise);
    await renderReviews();

    const loading = await screen.findByRole('status', { busy: true });
    expect(loading).toHaveTextContent('Loading review queue');
    await act(async () => first.reject(new Error('backend down')));

    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('Could not load the review queue');
    expect(screen.queryByText('No reviews are waiting.')).not.toBeInTheDocument();

    reviewsMock.mockResolvedValueOnce(page([]));
    fireEvent.click(within(alert).getByRole('button', { name: 'Retry' }));
    expect(await screen.findByText('No reviews are waiting.')).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });
});
