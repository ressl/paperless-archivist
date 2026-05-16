import { describe, expect, it, vi, beforeEach } from 'vitest';
import { cleanup, render, waitFor } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { I18nProvider } from '../i18n/I18nProvider';
import type { ReviewItem } from '../api/client';

expect.extend(toHaveNoViolations);

const reviewFixtures: ReviewItem[] = [
  {
    id: 'r-1',
    paperless_document_id: 101,
    stage: 'correspondent',
    status: 'pending',
    suggested_patch: { correspondent: 17, standard_metadata: { confidence: 0.92 } },
    edited_patch: null,
    validation_warnings: [],
    debug_context: null,
    created_at: '2026-05-15T09:00:00Z'
  },
  {
    id: 'r-2',
    paperless_document_id: 102,
    stage: 'tags',
    status: 'pending',
    suggested_patch: { tags: ['Invoice'], standard_metadata: { confidence: 0.4 } },
    edited_patch: null,
    validation_warnings: ['confidence_below_threshold'],
    debug_context: null,
    created_at: '2026-05-15T09:05:00Z'
  },
  {
    id: 'r-3',
    paperless_document_id: 103,
    stage: 'title',
    status: 'pending',
    suggested_patch: { title: 'Statement May 2026' },
    edited_patch: null,
    validation_warnings: [],
    debug_context: null,
    created_at: '2026-05-15T09:10:00Z'
  }
];

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      reviews: vi.fn(async () => ({ items: reviewFixtures })),
      batchReview: vi.fn(async () => ({ ok: true })),
      approveReview: vi.fn(async () => ({ ok: true })),
      rejectReview: vi.fn(async () => ({ ok: true })),
      editReview: vi.fn(async () => ({ ok: true }))
    }
  };
});

describe('<Reviews> a11y shell', () => {
  beforeEach(() => {
    cleanup();
  });

  it('renders a reviews list without axe violations on the static shell', async () => {
    const { Reviews } = await import('./Reviews');
    const { container } = render(
      <I18nProvider>
        <Reviews setError={() => undefined} />
      </I18nProvider>
    );
    await waitFor(
      () => {
        expect(container.querySelector('.page')).not.toBeNull();
      },
      { timeout: 5000 }
    );
    const results = await axe(container, {
      rules: {
        region: { enabled: false },
        'color-contrast': { enabled: false }
      }
    });
    expect(results).toHaveNoViolations();
  });
});
