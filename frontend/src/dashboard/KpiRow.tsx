import type { ReactNode } from 'react';
import { Counts, DashboardRange, DashboardStats } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { deltaTone, formatCost, formatDelta, formatMs, formatMttc } from '../lib/format';

type MetricTone = 'success' | 'warning' | 'danger' | 'neutral' | 'info' | 'review';

type Metric = {
  label: string;
  value: ReactNode;
  tone: MetricTone;
  delta?: number | null;
  higherIsBetter: boolean;
};

// Bespoke metric tile (rather than the shared <KpiCard/>) because the dashboard
// needs the `info`/`review` tones and the larger `hero` variant that KpiCard's
// tone union and markup do not cover. See css_needed in the handoff.
function MetricTile({
  metric,
  hero,
  formatNumber,
  formatDelta: fmtDelta
}: {
  metric: Metric;
  hero?: boolean;
  formatNumber: (value: number) => string;
  formatDelta: (value: number) => string;
}) {
  const { label, value, tone, delta, higherIsBetter } = metric;
  return (
    <div className={`card metric${hero ? ' hero' : ''} ${tone}`}>
      <span>{label}</span>
      <strong>{typeof value === 'number' ? formatNumber(value) : value}</strong>
      {typeof delta === 'number' && <em className={deltaTone(delta, higherIsBetter)}>{fmtDelta(delta)}</em>}
    </div>
  );
}

/**
 * Primary KPI hierarchy: one prominent hero metric (open backlog), a small row
 * of headline metrics, then demoted secondary stats — instead of a flat wall of
 * competing numbers (issue #237).
 */
export function KpiRow({
  stats,
  counts,
  range
}: {
  stats: DashboardStats | null;
  counts: Counts;
  range: DashboardRange;
}) {
  const { t, formatNumber, formatPercent } = useI18n();
  const fmtDelta = (value: number) => formatDelta(value, t, formatNumber);

  const comparison = stats?.comparison;
  const openBacklog = counts.total_documents - counts.complete;
  const runningJobs = stats?.kpis.running_jobs ?? counts.running;

  // Derive the hero backlog tone from the value/trend instead of hardcoding
  // 'warning': empty or shrinking backlog reads success, growth reads
  // warning (or danger when it grew by a quarter or more) (issue #233).
  const backlogValue = stats?.kpis.open_backlog ?? openBacklog;
  const backlogDelta = comparison?.open_backlog_delta ?? 0;
  const backlogGrowthPct = backlogValue > 0 && backlogDelta > 0 ? backlogDelta / backlogValue : 0;
  const heroTone: MetricTone =
    backlogValue === 0 || backlogDelta < 0
      ? 'success'
      : backlogDelta === 0
        ? 'neutral'
        : backlogGrowthPct >= 0.25
          ? 'danger'
          : 'warning';

  const heroMetric: Metric = {
    label: t('dashboard.metric.open_backlog'),
    value: backlogValue,
    tone: heroTone,
    delta: comparison?.open_backlog_delta,
    higherIsBetter: false
  };
  const secondaryMetrics: Metric[] = [
    { label: t('dashboard.metric.throughput'), value: stats?.kpis.throughput ?? 0, tone: 'success', delta: comparison?.jobs_succeeded_delta, higherIsBetter: true },
    { label: t('dashboard.metric.completion'), value: formatPercent(stats?.kpis.completion_rate ?? 0), tone: 'neutral', delta: null, higherIsBetter: true },
    { label: t('dashboard.metric.mttc'), value: formatMttc(stats?.kpis.mttc_seconds), tone: 'neutral', delta: null, higherIsBetter: false },
    { label: t('dashboard.metric.cost', { range }), value: formatCost(stats?.kpis.cost_in_range_usd), tone: 'neutral', delta: null, higherIsBetter: false }
  ];
  const tertiaryMetrics: Metric[] = [
    { label: t('dashboard.metric.running_now'), value: runningJobs, tone: 'info', delta: null, higherIsBetter: true },
    { label: t('dashboard.metric.review_queue'), value: counts.waiting_review, tone: 'review', delta: null, higherIsBetter: false },
    { label: t('dashboard.metric.failed'), value: counts.failed, tone: 'danger', delta: comparison?.jobs_failed_delta, higherIsBetter: false },
    { label: t('dashboard.metric.p95_latency'), value: formatMs(stats?.kpis.p95_stage_duration_ms ?? 0), tone: 'neutral', delta: null, higherIsBetter: false }
  ];

  return (
    <div className="kpi-grid">
      <MetricTile metric={heroMetric} hero formatNumber={formatNumber} formatDelta={fmtDelta} />
      <div className="kpi-secondary card-grid card-grid--compact">
        {secondaryMetrics.map((metric) => (
          <MetricTile key={metric.label} metric={metric} formatNumber={formatNumber} formatDelta={fmtDelta} />
        ))}
      </div>
      <div className="kpi-tertiary card-grid card-grid--compact">
        {tertiaryMetrics.map((metric) => (
          <MetricTile key={metric.label} metric={metric} formatNumber={formatNumber} formatDelta={fmtDelta} />
        ))}
      </div>
    </div>
  );
}
