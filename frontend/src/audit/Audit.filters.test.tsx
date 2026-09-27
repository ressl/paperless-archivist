import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type AuditEvent, type AuditQueryParams } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

expect.extend(toHaveNoViolations);

const event = (id: string, overrides: Partial<AuditEvent> = {}): AuditEvent => ({
  id,
  event_type: 'document.patch_confirmed',
  actor_type: 'user',
  actor_id: 'u-1',
  actor_username: 'alice',
  paperless_document_id: 42,
  outcome: 'success',
  created_at: '2026-09-27T10:00:00Z',
  has_changes: true,
  ...overrides
});

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      auditSearch: vi.fn(),
      auditEvent: vi.fn(),
      auditIntegrity: vi.fn(async () => ({
        ok: true,
        checked_events: 0,
        v1_events: 0,
        v2_events: 0,
        legacy_events: 0,
        legacy_precision_events: 0
      }))
    }
  };
});

const searchMock = vi.mocked(api.auditSearch);
const detailMock = vi.mocked(api.auditEvent);

async function renderAudit() {
  const { Audit } = await import('./Audit');
  render(
    <I18nProvider>
      <Audit setError={() => undefined} />
    </I18nProvider>
  );
}

describe('<Audit> filters, paging and diff (#448)', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
    window.history.replaceState(null, '', '/audit');
    searchMock.mockImplementation(async (params: AuditQueryParams) =>
      params.cursor
        ? { items: [event('e-2', { paperless_document_id: 41, has_changes: false })], next_cursor: null }
        : { items: [event('e-1')], next_cursor: 'cursor-1' }
    );
  });

  it('sends server-side filters, mirrors them into the URL and pages with the cursor', async () => {
    await renderAudit();
    expect(await screen.findByText('alice')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Actor (username or ID)'), { target: { value: 'alice' } });
    fireEvent.change(screen.getByLabelText('Document ID'), { target: { value: '42' } });
    fireEvent.change(screen.getByLabelText('Outcome'), { target: { value: 'failed' } });
    fireEvent.click(screen.getByRole('button', { name: 'Apply filters' }));

    await waitFor(() =>
      expect(searchMock).toHaveBeenLastCalledWith(
        expect.objectContaining({ actor: 'alice', document_id: '42', outcome: 'failed', limit: 100 }),
        expect.anything()
      )
    );
    expect(window.location.search).toBe('?actor=alice&document_id=42&outcome=failed');

    fireEvent.click(await screen.findByRole('button', { name: 'Load older events' }));
    await waitFor(() =>
      expect(searchMock).toHaveBeenLastCalledWith(expect.objectContaining({ actor: 'alice', cursor: 'cursor-1' }))
    );
    expect(await screen.findByText('41')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Load older events' })).not.toBeInTheDocument();
  });

  it('restores filters from the URL on reload', async () => {
    window.history.replaceState(null, '', '/audit?event_type=review.applied');
    await renderAudit();
    await waitFor(() =>
      expect(searchMock).toHaveBeenCalledWith(expect.objectContaining({ event_type: 'review.applied' }), expect.anything())
    );
    expect(screen.getByLabelText('Event type')).toHaveValue('review.applied');
  });

  it('shows the before/after diff of an event in an accessible drawer', async () => {
    detailMock.mockResolvedValue({
      ...event('e-1'),
      before: { title: 'Old', correspondent: 7 },
      after: { title: 'New', correspondent: 7, document_type: 3 }
    });
    await renderAudit();
    fireEvent.click(await screen.findByRole('button', { name: /Show changes of document.patch_confirmed/ }));
    const dialog = await screen.findByRole('dialog', { name: 'Changes: document.patch_confirmed' });
    const table = await within(dialog).findByRole('table', { name: 'Before/after differences' });
    const rows = within(table).getAllByRole('row');
    // header + title (changed) + document_type (added); the unchanged correspondent is hidden
    expect(rows).toHaveLength(3);
    expect(within(table).getByText('Old')).toBeInTheDocument();
    expect(within(table).getByText('New')).toBeInTheDocument();
    expect(within(table).queryByText('correspondent')).not.toBeInTheDocument();
    expect(detailMock).toHaveBeenCalledWith('e-1', expect.anything());
    expect(await axe(dialog)).toHaveNoViolations();

    fireEvent.click(within(dialog).getByRole('button', { name: 'Close' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
  });
});
