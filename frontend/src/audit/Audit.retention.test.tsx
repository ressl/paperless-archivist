import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type RuntimeSettings } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

expect.extend(toHaveNoViolations);

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      audit: vi.fn(async () => ({ items: [] })),
      auditIntegrity: vi.fn(async () => ({
        ok: true,
        checked_events: 0,
        v1_events: 0,
        v2_events: 0,
        legacy_events: 0,
        legacy_precision_events: 0
      })),
      settings: vi.fn(async () => ({ security: { audit_retention_days: 365, ai_artifact_retention_days: 30 } }) as unknown as RuntimeSettings),
      applyAuditRetention: vi.fn(async () => ({ ai_artifacts_deleted: 2, audit_events_deleted: 5, ocr_page_cache_deleted: 1 }))
    }
  };
});

async function renderAudit() {
  const { Audit } = await import('./Audit');
  render(
    <I18nProvider>
      <Audit setError={() => undefined} />
    </I18nProvider>
  );
  await waitFor(() => expect(api.auditIntegrity).toHaveBeenCalled());
}

describe('<Audit> retention confirmation (#417)', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('states the retention scope and does nothing when cancelled', async () => {
    await renderAudit();
    fireEvent.click(screen.getByRole('button', { name: 'Apply retention' }));
    const dialog = await screen.findByRole('alertdialog', { name: 'Apply retention now?' });
    expect(dialog).toHaveTextContent('older than 365 days');
    expect(dialog).toHaveTextContent('older than 30 days');
    expect(await axe(dialog)).toHaveNoViolations();

    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(api.applyAuditRetention).not.toHaveBeenCalled();
  });

  it('applies retention after confirmation', async () => {
    await renderAudit();
    fireEvent.click(screen.getByRole('button', { name: 'Apply retention' }));
    const dialog = await screen.findByRole('alertdialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Apply retention' }));
    await waitFor(() => expect(api.applyAuditRetention).toHaveBeenCalledTimes(1));
    expect(await screen.findByText('Retention applied')).toBeInTheDocument();
  });
});
