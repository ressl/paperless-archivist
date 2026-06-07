import {
  DashboardLiveFailure,
  DashboardLiveStatus,
  DashboardRange,
  DashboardStats,
  DashboardStatusCount
} from '../api/client';
import { statusLabel } from '../lib/format';
import type { TFunction } from '../i18n/I18nProvider';

export const defaultDashboardRanges: Array<{ key: DashboardRange; label: string }> = [
  { key: '24h', label: '24h' },
  { key: '7d', label: '7d' },
  { key: '30d', label: '30d' },
  { key: '90d', label: '90d' },
  { key: '12m', label: '12m' },
  { key: 'all', label: 'All' }
];

// The consolidated `metadata` stage replaces the six legacy per-field stages on the
// stage-matrix dashboard; only `ocr` and `metadata` run on current pipelines.
export const defaultStageStatus = ['ocr', 'metadata'].map((stage) => ({
  stage,
  complete: 0,
  pending: 0,
  failed: 0,
  waiting_review: 0,
  running: 0
}));

export const defaultJobStatus: DashboardStatusCount[] = [
  { status: 'queued', count: 0 },
  { status: 'running', count: 0 },
  { status: 'succeeded', count: 0 },
  { status: 'failed', count: 0 }
];

export function statusChartData(items: DashboardStatusCount[], t: TFunction) {
  return items.map((item) => ({
    ...item,
    label: statusLabel(item.status, t)
  }));
}

export function liveFailureKind(failure: DashboardLiveFailure) {
  return failure.failure_kind || (failure.status === 'failed' ? 'failed' : 'retry_scheduled');
}

export function liveFailureTiming(
  failure: DashboardLiveFailure,
  kind: string,
  t: TFunction,
  formatRelative: (value?: string | null) => string
) {
  if (kind === 'retry_ready') return t('dashboard.live.retry_ready');
  if (failure.next_attempt_at) return t('dashboard.live.next_retry', { time: formatRelative(failure.next_attempt_at) });
  return t('dashboard.live.updated', { time: formatRelative(failure.updated_at) });
}

export function computeHealthScore(stats: DashboardStats | null, live: DashboardLiveStatus | null) {
  if (!stats) return null;
  let score = 100;
  score -= Math.min(50, stats.kpis.failure_rate * 100);
  const backlogDelta = stats.comparison?.open_backlog_delta ?? 0;
  if (backlogDelta > 0 && stats.kpis.open_backlog > 0) {
    const pct = (backlogDelta / stats.kpis.open_backlog) * 100;
    score -= Math.min(25, pct * 0.5);
  }
  const critical = live?.needs_attention?.filter((item) => item.severity === 'critical').length ?? 0;
  score -= critical * 15;
  return Math.max(0, Math.round(score));
}

export function healthTone(score: number | null): 'success' | 'warning' | 'danger' | 'neutral' {
  if (score == null) return 'neutral';
  if (score >= 90) return 'success';
  if (score >= 70) return 'warning';
  return 'danger';
}

export type StageMatrixRow = {
  stage: string;
  queued: number;
  running: number;
  failed: number;
  complete: number;
  avg_ms: number;
  p95_ms: number;
  throughput_per_hour: number;
  bottleneck_score: number;
};

// Recharts default tooltip displays raw numbers. For our composed charts the
// `success_rate` / `completion_rate` series live on the right Y-axis as
// percentages — surface the % suffix in the tooltip too so 100 reads as
// 100% to match the axis label.
const PERCENT_KEYS = new Set(['success_rate', 'completion_rate']);
export const chartTooltipFormatter = (
  value: unknown,
  _name: unknown,
  payload: { dataKey?: string | number | ((obj: unknown) => unknown) }
) => {
  const key = typeof payload?.dataKey === 'string' ? payload.dataKey : '';
  if (PERCENT_KEYS.has(key) && typeof value === 'number') {
    return `${value}%`;
  }
  return value as string | number;
};

export function buildStageMatrix(stats: DashboardStats | null): StageMatrixRow[] {
  const stages = stats?.stage_status?.length ? stats.stage_status : defaultStageStatus;
  const usageByStage = new Map<string, { total_avg: number; count: number; max_p95: number }>();
  for (const usage of stats?.provider_usage ?? []) {
    const slot = usageByStage.get(usage.stage) ?? { total_avg: 0, count: 0, max_p95: 0 };
    slot.total_avg += usage.avg_duration_ms * usage.request_count;
    slot.count += usage.request_count;
    slot.max_p95 = Math.max(slot.max_p95, usage.p95_duration_ms);
    usageByStage.set(usage.stage, slot);
  }
  return stages.map((stage) => {
    const usage = usageByStage.get(stage.stage);
    const avg_ms = usage && usage.count > 0 ? usage.total_avg / usage.count : 0;
    const p95_ms = usage?.max_p95 ?? 0;
    const throughput_per_hour = avg_ms > 0 ? 3_600_000 / avg_ms : 0;
    const queued = stage.pending;
    const bottleneck_score = throughput_per_hour > 0 ? queued / throughput_per_hour : queued > 0 ? Number.POSITIVE_INFINITY : 0;
    return {
      stage: stage.stage,
      queued,
      running: stage.running,
      failed: stage.failed,
      complete: stage.complete,
      avg_ms,
      p95_ms,
      throughput_per_hour,
      bottleneck_score
    };
  });
}
