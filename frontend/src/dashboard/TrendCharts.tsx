import { memo, useMemo } from 'react';
import {
  Area,
  Bar,
  BarChart,
  CartesianGrid,
  ComposedChart,
  Legend,
  Line,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis
} from 'recharts';
import { DashboardRange, DashboardStats } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { ChartPanel } from './Primitives';
import { StageMatrix } from './StageMatrix';
import { chartTooltipFormatter, defaultJobStatus, statusChartData } from './helpers';

// Memoized analytics column (4 Recharts + StageMatrix). It depends only on
// stats/range-derived data, so the 5s `live` poll re-render reuses the same
// element and React skips reconciling the charts entirely.
export const TrendCharts = memo(function TrendCharts({
  stats,
  range,
  compactLayout,
  activeTab,
  onStageSelect
}: {
  stats: DashboardStats | null;
  range: DashboardRange;
  compactLayout: boolean;
  activeTab: 'analytics' | 'live' | 'activity';
  onStageSelect?: (stage: string) => void;
}) {
  const { t } = useI18n();
  const jobStatusData = useMemo(
    () => statusChartData(stats?.job_status.length ? stats.job_status : defaultJobStatus, t),
    [stats?.job_status, t]
  );
  const throughputWithRate = useMemo(() => {
    const series = stats?.throughput_series ?? [];
    return series.map((bucket) => {
      const total = bucket.jobs_succeeded + bucket.jobs_failed;
      const success_rate = total > 0 ? Math.round((bucket.jobs_succeeded / total) * 100) : null;
      return { ...bucket, success_rate };
    });
  }, [stats?.throughput_series]);
  const backlogWithRate = useMemo(() => {
    const series = stats?.backlog_series ?? [];
    return series.map((point) => {
      const completion_rate = point.total_documents > 0
        ? Math.round((point.complete / point.total_documents) * 100)
        : 0;
      return { ...point, completion_rate };
    });
  }, [stats?.backlog_series]);

  return (
    <div
      id={compactLayout ? 'dashboard-panel-analytics' : undefined}
      className={`dashboard-analytics${compactLayout && activeTab !== 'analytics' ? ' is-hidden' : ''}`}
      role={compactLayout ? 'tabpanel' : undefined}
      aria-labelledby={compactLayout ? 'dashboard-tab-analytics' : undefined}
      hidden={compactLayout && activeTab !== 'analytics'}
      tabIndex={compactLayout && activeTab === 'analytics' ? 0 : undefined}
    >
      <ChartPanel title={t('dashboard.chart.throughput', { range })} wide>
        <ResponsiveContainer width="100%" height={280}>
          <ComposedChart data={throughputWithRate}>
            <defs>
              <pattern id="pat-created" patternUnits="userSpaceOnUse" width="6" height="6">
                <rect width="6" height="6" fill="var(--info-soft)" />
                <circle cx="3" cy="3" r="1.2" fill="var(--info)" />
              </pattern>
              <pattern id="pat-succeeded" patternUnits="userSpaceOnUse" width="6" height="6" patternTransform="rotate(45)">
                <rect width="6" height="6" fill="var(--teal-soft)" />
                <line x1="0" y1="0" x2="0" y2="6" stroke="var(--teal)" strokeWidth="1.2" />
              </pattern>
              <pattern id="pat-failed" patternUnits="userSpaceOnUse" width="6" height="6" patternTransform="rotate(-45)">
                <rect width="6" height="6" fill="var(--danger-soft)" />
                <line x1="0" y1="0" x2="0" y2="6" stroke="var(--danger)" strokeWidth="1.6" />
              </pattern>
            </defs>
            <CartesianGrid strokeDasharray="3 3" vertical={false} />
            <XAxis dataKey="label" />
            <YAxis yAxisId="count" allowDecimals={false} width={56} />
            <YAxis yAxisId="rate" orientation="right" domain={[0, 100]} unit="%" width={44} />
            <Tooltip formatter={chartTooltipFormatter} />
            <Legend />
            <Area yAxisId="count" type="monotone" dataKey="jobs_created" name={t('dashboard.chart.created')} stroke="var(--info)" fill="url(#pat-created)" />
            <Area yAxisId="count" type="monotone" dataKey="jobs_succeeded" name={t('dashboard.chart.succeeded')} stroke="var(--teal)" fill="url(#pat-succeeded)" />
            <Area yAxisId="count" type="monotone" dataKey="jobs_failed" name={t('dashboard.chart.failed')} stroke="var(--danger)" fill="url(#pat-failed)" />
            <Line yAxisId="rate" type="monotone" dataKey="success_rate" name={t('dashboard.chart.success_rate')} stroke="var(--teal-fg)" strokeWidth={2} dot={false} />
          </ComposedChart>
        </ResponsiveContainer>
      </ChartPanel>
      <StageMatrix stats={stats} onStageSelect={onStageSelect} />
      <div className="card-grid card-grid--wide">
        <ChartPanel title={t('dashboard.chart.backlog_trend')}>
          <ResponsiveContainer width="100%" height={240}>
            <ComposedChart data={backlogWithRate}>
              <defs>
                <pattern id="pat-backlog" patternUnits="userSpaceOnUse" width="8" height="8" patternTransform="rotate(135)">
                  <rect width="8" height="8" fill="var(--brass-soft)" />
                  <line x1="0" y1="0" x2="0" y2="8" stroke="var(--brass)" strokeWidth="1.2" />
                </pattern>
              </defs>
              <CartesianGrid strokeDasharray="3 3" vertical={false} />
              <XAxis dataKey="label" />
              <YAxis yAxisId="count" allowDecimals={false} width={56} />
              <YAxis yAxisId="rate" orientation="right" domain={[0, 100]} unit="%" width={44} />
              <Tooltip formatter={chartTooltipFormatter} />
              <Legend />
              <Area yAxisId="count" type="monotone" dataKey="open_backlog" name={t('dashboard.chart.open')} stroke="var(--brass)" fill="url(#pat-backlog)" />
              <Line yAxisId="rate" type="monotone" dataKey="completion_rate" name={t('dashboard.chart.completion_rate')} stroke="var(--teal)" strokeWidth={2} dot={false} />
            </ComposedChart>
          </ResponsiveContainer>
        </ChartPanel>
        <ChartPanel title={t('dashboard.chart.queue_state')}>
          <ResponsiveContainer width="100%" height={240}>
            <BarChart data={jobStatusData} layout="vertical" margin={{ left: 12 }}>
              <CartesianGrid strokeDasharray="3 3" horizontal={false} />
              <XAxis type="number" allowDecimals={false} />
              <YAxis type="category" dataKey="label" width={92} />
              <Tooltip />
              <Bar dataKey="count" fill="var(--info)" radius={[0, 4, 4, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </ChartPanel>
      </div>
    </div>
  );
});
