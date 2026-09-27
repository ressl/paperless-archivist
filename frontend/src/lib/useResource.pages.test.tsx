import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ApiError, api, type AuditIntegrityReport } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

const integrity: AuditIntegrityReport = {
  ok: true,
  checked_events: 3,
  legacy_events: 0,
  v1_events: 0,
  v2_events: 3,
  legacy_precision_events: 0
};

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      users: vi.fn(),
      sessions: vi.fn(async () => ({ items: [] })),
      apiTokens: vi.fn(async () => ({ items: [] })),
      auditSearch: vi.fn(async () => ({ items: [], next_cursor: null })),
      auditIntegrity: vi.fn(async () => integrity)
    }
  };
});

beforeEach(() => {
  vi.mocked(api.users).mockReset();
  vi.mocked(api.auditSearch).mockClear();
  vi.mocked(api.auditIntegrity).mockClear();
});
afterEach(() => cleanup());

describe('pages built on useResource (#444)', () => {
  it('Users loads through cancellable requests and reports errors once', async () => {
    vi.mocked(api.users).mockRejectedValue(new ApiError('forbidden', 403));
    const setError = vi.fn();
    const { Users } = await import('../users/Users');
    render(
      <I18nProvider>
        <Users setError={setError} />
      </I18nProvider>
    );
    await waitFor(() => expect(setError).toHaveBeenCalledTimes(1));
    expect(setError.mock.calls[0][0]).toContain('forbidden');
    expect(vi.mocked(api.users).mock.calls[0][0]?.signal).toBeInstanceOf(AbortSignal);
  });

  it('Audit re-verifies the chain without refetching the event log', async () => {
    const { Audit } = await import('../audit/Audit');
    render(
      <I18nProvider>
        <Audit setError={() => undefined} />
      </I18nProvider>
    );
    expect(await screen.findByText('Audit chain verified')).toBeInTheDocument();
    expect(api.auditSearch).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole('button', { name: 'Verify chain' }));
    await waitFor(() => expect(api.auditIntegrity).toHaveBeenCalledTimes(2));
    expect(api.auditSearch).toHaveBeenCalledTimes(1);
  });
});
