import { describe, expect, it } from 'vitest';
import { computeHealthScore } from './Dashboard';
import type { DashboardLiveStatus, DashboardStats } from '../api/client';

// Minimal DashboardStats fixture covering only the fields computeHealthScore
// inspects. Anything else stays as `null` / `0` so tests are precise about
// inputs that drive the score.
function statsFixture(overrides: Partial<DashboardStats> = {}): DashboardStats {
  const base: DashboardStats = {
    generated_at: '2026-05-16T10:00:00Z',
    selected_range: '24h',
    available_ranges: [],
    kpis: {
      completion_rate: 0.8,
      open_backlog: 100,
      failure_rate: 0,
      review_load: 0,
      running_jobs: 0,
      throughput: 0,
      cost_in_range_usd: null,
      mttc_seconds: null,
      p95_stage_duration_ms: null
    },
    comparison: {
      jobs_created_delta: 0,
      jobs_succeeded_delta: 0,
      jobs_failed_delta: 0,
      open_backlog_delta: 0
    },
    stage_status: [],
    throughput_series: [],
    backlog_series: [],
    job_status: [],
    run_status: [],
    review_status: [],
    provider_usage: [],
    quality: {
      review_decisions: 0,
      review_approved: 0,
      review_edited: 0,
      review_rejected: 0,
      acceptance_rate: null,
      uncertainty_reviews: 0,
      validation_warning_reviews: 0
    },
    cost_series: [],
    cost_breakdown_by_provider: []
  };
  return { ...base, ...overrides };
}

function liveFixture(critical: number, warning: number): DashboardLiveStatus {
  const items = [
    ...Array.from({ length: critical }, (_, idx) => ({
      kind: 'stuck_runs',
      severity: 'critical',
      title: `crit ${idx}`,
      description: '',
      action_key: null,
      count: null
    })),
    ...Array.from({ length: warning }, (_, idx) => ({
      kind: 'stale_leases',
      severity: 'warning',
      title: `warn ${idx}`,
      description: '',
      action_key: null,
      count: null
    }))
  ];
  return {
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
    selector: { state: 'idle', title: '', description: '', last_event_at: null },
    next_selector_scan_at: null,
    llm: { state: 'idle', title: '', description: '', last_event_at: null },
    paperless: { state: 'idle', title: '', description: '', last_event_at: null },
    active_runs: [],
    active_jobs: [],
    recent_llm_events: [],
    recent_failures: [],
    needs_attention: items
  };
}

describe('computeHealthScore', () => {
  it('returns null when stats are absent', () => {
    expect(computeHealthScore(null, null)).toBeNull();
  });

  it('reports a perfect score when everything is calm', () => {
    expect(computeHealthScore(statsFixture(), liveFixture(0, 0))).toBe(100);
  });

  it('clamps the failure-rate penalty at 50 points', () => {
    const stats = statsFixture({
      kpis: { ...statsFixture().kpis, failure_rate: 1 }
    });
    expect(computeHealthScore(stats, null)).toBe(50);
  });

  it('subtracts 15 points per critical needs-attention item', () => {
    const stats = statsFixture();
    expect(computeHealthScore(stats, liveFixture(1, 0))).toBe(85);
    expect(computeHealthScore(stats, liveFixture(2, 0))).toBe(70);
  });

  it('penalises a growing backlog proportionally to its size', () => {
    // open_backlog = 100, delta = +20 -> pct = 20 -> penalty = min(25, 20*0.5) = 10
    const stats = statsFixture({
      kpis: { ...statsFixture().kpis, open_backlog: 100 },
      comparison: {
        jobs_created_delta: 0,
        jobs_succeeded_delta: 0,
        jobs_failed_delta: 0,
        open_backlog_delta: 20
      }
    });
    expect(computeHealthScore(stats, null)).toBe(90);
  });

  it('does not penalise a shrinking backlog', () => {
    const stats = statsFixture({
      comparison: {
        jobs_created_delta: 0,
        jobs_succeeded_delta: 0,
        jobs_failed_delta: 0,
        open_backlog_delta: -10
      }
    });
    expect(computeHealthScore(stats, null)).toBe(100);
  });

  it('clamps the final score to zero', () => {
    const stats = statsFixture({
      kpis: { ...statsFixture().kpis, failure_rate: 1, open_backlog: 100 }
    });
    expect(computeHealthScore(stats, liveFixture(10, 0))).toBe(0);
  });
});
