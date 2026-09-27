import { act, cleanup, fireEvent, render, renderHook, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type Counts, type DashboardLiveStatus, type Permissions } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';
import { useDashboardLive } from './hooks';

expect.extend(toHaveNoViolations);

const counts: Counts = {
  total_documents: 120,
  complete: 100,
  missing_ocr: 10,
  waiting_review: 6,
  failed: 3,
  running: 2,
  never_processed: 8
};

const liveFixture = {
  generated_at: '2026-09-27T10:00:00Z',
  workflow_mode: 'manual_review',
  autopilot_enabled: false,
  workflow_safety: {
    paused: false,
    dry_run: false,
    hourly_document_limit: null,
    daily_document_limit: null,
    hourly_remaining: null,
    daily_remaining: null
  },
  selector: { state: 'idle', title: 'Idle', description: '', last_event_at: null },
  next_selector_scan_at: null,
  llm: { state: 'idle', title: 'Idle', description: '', last_event_at: null },
  paperless: { state: 'idle', title: 'Connected', description: '', last_event_at: null },
  active_runs: [],
  active_jobs: [],
  recent_llm_events: [],
  recent_failures: [],
  needs_attention: []
} as unknown as DashboardLiveStatus;

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      dashboard: vi.fn(),
      dashboardLive: vi.fn(),
      recoveryStatus: vi.fn(async () => ({ older_than_seconds: 600, items: [] })),
      queueOcr: vi.fn(async () => ({ queued: 10 })),
      updateWorkflowMode: vi.fn()
    }
  };
});

const permissions = {
  read_dashboard: true,
  read_runs: true,
  write_runs: true,
  read_inventory: true,
  write_batches: true,
  use_chat: true,
  read_reviews: true,
  write_reviews: true,
  read_settings: true,
  write_settings: true,
  manage_users: true,
  read_audit: true
} as Permissions;

describe('useDashboardLive polling health (#427)', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
    cleanup();
  });

  it('backs off on repeated failures and clears the stale flag after a success', async () => {
    vi.mocked(api.dashboardLive).mockRejectedValue(new Error('api down'));
    const { result } = renderHook(() => useDashboardLive(false));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.health.stale).toBe(true);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    // Without backoff a 5s poll would have fired 13 times in 60s; with
    // 5s/10s/20s/40s gaps only t=0, 5, 15 and 35s hit the API.
    expect(api.dashboardLive).toHaveBeenCalledTimes(4);
    expect(result.current.health.failures).toBe(4);

    vi.mocked(api.dashboardLive).mockResolvedValue(liveFixture);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(15_000);
    });
    expect(api.dashboardLive).toHaveBeenCalledTimes(5);
    expect(result.current.health).toEqual({ stale: false, failures: 0 });
    expect(result.current.live).toEqual(liveFixture);
  });
});

async function renderDashboard(setError = vi.fn()) {
  const { Dashboard } = await import('./Dashboard');
  render(
    <I18nProvider>
      <Dashboard setError={setError} setSuccess={() => undefined} canManageSettings permissions={permissions} />
    </I18nProvider>
  );
  return setError;
}

describe('<Dashboard> polling indicator and confirmations', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
    window.localStorage.clear();
    vi.mocked(api.dashboard).mockResolvedValue({ counts, stats: null } as never);
    vi.mocked(api.dashboardLive).mockResolvedValue(liveFixture);
  });

  it('shows an inline stale indicator instead of the global error banner (#427)', async () => {
    vi.mocked(api.dashboardLive).mockRejectedValue(new Error('api down'));
    const setError = await renderDashboard();

    expect(await screen.findByText(/Live data may be out of date/)).toBeInTheDocument();
    expect(setError).not.toHaveBeenCalledWith(expect.stringMatching(/api down/));
  });

  it('asks before queueing OCR and states the scope (#417)', async () => {
    window.localStorage.setItem('dashboard.drawer_open', 'true');
    await renderDashboard();
    const drawer = await screen.findByRole('dialog', { name: 'Operator tools' });
    await waitFor(() => expect(api.dashboard).toHaveBeenCalled());

    fireEvent.click(within(drawer).getByRole('button', { name: 'Queue OCR' }));
    const dialog = await screen.findByRole('alertdialog', { name: 'Queue OCR for all documents without OCR?' });
    await waitFor(() => expect(dialog).toHaveAccessibleDescription(/10 document/));
    expect(await axe(dialog)).toHaveNoViolations();

    // Escape closes only the confirmation, not the maintenance drawer.
    fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(screen.getByRole('dialog', { name: 'Operator tools' })).toBeInTheDocument();
    expect(api.queueOcr).not.toHaveBeenCalled();

    fireEvent.click(within(drawer).getByRole('button', { name: 'Queue OCR' }));
    fireEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Queue OCR' }));
    await waitFor(() => expect(api.queueOcr).toHaveBeenCalledTimes(1));
  });

  it('asks before handing control to the autopilot (#417)', async () => {
    await renderDashboard();
    const fullAuto = await screen.findByRole('button', { name: /Full autopilot/ });
    await waitFor(() => expect(fullAuto).toBeEnabled());

    fireEvent.click(fullAuto);
    const dialog = await screen.findByRole('alertdialog', { name: 'Switch to Full autopilot?' });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(api.updateWorkflowMode).not.toHaveBeenCalled();
  });
});
