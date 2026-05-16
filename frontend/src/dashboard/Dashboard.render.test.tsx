import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, cleanup, waitFor } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { I18nProvider } from '../i18n/I18nProvider';
import type { Counts, DashboardLiveStatus, DashboardStats, Permissions } from '../api/client';

expect.extend(toHaveNoViolations);

// Mock the api module so the Dashboard hooks resolve immediately with our
// fixtures. Each mock returns a resolved Promise — the dashboard's polling
// `useVisibleInterval` fires the tick once on mount and then every N seconds,
// so the first render will see the fixture data once the microtask flushes.
const dashboardFixture: { counts: Counts; stats: DashboardStats } = {
  counts: {
    total_documents: 1200,
    complete: 1000,
    missing_ocr: 10,
    missing_tagging: 5,
    missing_title: 8,
    missing_correspondent: 12,
    missing_document_type: 11,
    missing_document_date: 7,
    missing_fields: 4,
    waiting_review: 6,
    failed: 3,
    running: 2,
    never_processed: 80
  },
  stats: {
    generated_at: '2026-05-16T10:00:00Z',
    selected_range: '24h',
    available_ranges: [
      { key: '24h', label: 'Last 24h' },
      { key: '7d', label: 'Last 7 days' }
    ],
    kpis: {
      completion_rate: 0.83,
      open_backlog: 200,
      failure_rate: 0.03,
      review_load: 6,
      running_jobs: 2,
      throughput: 25,
      cost_in_range_usd: 0.42,
      mttc_seconds: 45,
      p95_stage_duration_ms: 1800
    },
    comparison: {
      jobs_created_delta: 4,
      jobs_succeeded_delta: 2,
      jobs_failed_delta: 0,
      open_backlog_delta: -10
    },
    stage_status: [],
    throughput_series: [],
    backlog_series: [],
    job_status: [{ status: 'running', count: 2 }, { status: 'succeeded', count: 100 }],
    run_status: [{ status: 'succeeded', count: 80 }],
    review_status: [{ status: 'pending', count: 6 }],
    provider_usage: [],
    quality: {
      review_decisions: 10,
      review_approved: 8,
      review_edited: 2,
      review_rejected: 0,
      acceptance_rate: 0.9,
      uncertainty_reviews: 1,
      validation_warning_reviews: 0
    },
    cost_series: [],
    cost_breakdown_by_provider: []
  }
};

const liveFixture: DashboardLiveStatus = {
  generated_at: '2026-05-16T10:00:00Z',
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
};

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      dashboard: vi.fn(async () => dashboardFixture),
      dashboardLive: vi.fn(async () => liveFixture),
      recoveryStatus: vi.fn(async () => ({ older_than_seconds: 600, items: [] }))
    }
  };
});

const allPermissions: Permissions = {
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
};

async function renderDashboard() {
  // Lazy-imported AFTER vi.mock has been applied above.
  const { Dashboard } = await import('./Dashboard');
  return render(
    <I18nProvider>
      <Dashboard
        setError={() => undefined}
        canManageSettings={true}
        permissions={allPermissions}
      />
    </I18nProvider>
  );
}

describe('<Dashboard> render smoke', () => {
  beforeEach(() => {
    cleanup();
  });

  it('renders the page heading after the stats fetch resolves', async () => {
    const { findByText } = await renderDashboard();
    // The localized title appears via PageHeader once stats arrive (or even before,
    // since the heading is static).
    const heading = await findByText(/Operations|Dashboard/i, undefined, { timeout: 5000 });
    expect(heading).toBeTruthy();
  });

  it('has no axe violations once the fixture has loaded', async () => {
    const { container } = await renderDashboard();
    // Wait for the open-backlog number to settle so axe scans a stable tree.
    await waitFor(
      () => {
        expect(container.querySelector('.dashboard-page')).not.toBeNull();
      },
      { timeout: 5000 }
    );
    const results = await axe(container, {
      rules: {
        // The dashboard contains many landmarks/cards; we only want to flag the
        // critical interaction issues (labels, names, contrast handled in CSS test).
        region: { enabled: false },
        'color-contrast': { enabled: false }
      }
    });
    expect(results).toHaveNoViolations();
  });
});
