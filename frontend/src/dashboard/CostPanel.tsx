import { memo, useMemo } from 'react';
import { DashboardRange, DashboardStats } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { formatCost } from '../lib/format';
import { Sparkline } from './Primitives';

export const CostPanel = memo(function CostPanel({ stats, range }: { stats: DashboardStats | null; range: DashboardRange }) {
  const { t, formatNumber } = useI18n();
  const total = stats?.kpis.cost_in_range_usd ?? null;
  const breakdown = stats?.cost_breakdown_by_provider ?? [];
  const sortedBreakdown = useMemo(() => {
    return [...breakdown]
      .filter((item) => item.cost_usd != null && item.cost_usd > 0)
      .sort((a, b) => (b.cost_usd ?? 0) - (a.cost_usd ?? 0))
      .slice(0, 8);
  }, [breakdown]);
  const series = stats?.cost_series ?? [];
  const maxBucket = useMemo(() => series.reduce((max, b) => Math.max(max, b.cost_usd ?? 0), 0), [series]);
  const maxBreakdownCost = sortedBreakdown[0]?.cost_usd ?? 0;
  const requestCount = series.reduce((sum, b) => sum + b.request_count, 0);
  return (
    <section className="card cost-panel chart-panel wide">
      <h3>{t('dashboard.cost.title', { range })}</h3>
      <p className="chart-panel-subtitle">{t('dashboard.cost.subtitle')}</p>
      {total == null || total <= 0 ? (
        <p className="empty-state compact">{t('dashboard.cost.no_data')}</p>
      ) : (
        <div className="cost-panel-grid">
          <div className="cost-total-card">
            <span>{t('dashboard.cost.total')}</span>
            <strong>{formatCost(total)}</strong>
            {/* TODO(i18n): replace literal with t('dashboard.cost.request_count', { count }) once the key exists. */}
            <em>{formatNumber(requestCount)} requests</em>
            <div className="cost-sparkline" aria-hidden="true">
              {series.map((bucket, idx) => {
                const heightPct = maxBucket > 0 ? Math.max(2, ((bucket.cost_usd ?? 0) / maxBucket) * 100) : 0;
                return <span key={`${bucket.bucket}-${idx}`} style={{ height: `${heightPct}%` }} />;
              })}
            </div>
          </div>
          <div className="cost-breakdown">
            <strong>{t('dashboard.cost.breakdown')}</strong>
            <ul>
              {sortedBreakdown.map((item) => {
                const pct = maxBreakdownCost > 0 ? Math.round(((item.cost_usd ?? 0) / maxBreakdownCost) * 100) : 0;
                return (
                  <li key={`${item.provider}-${item.model}`}>
                    <div className="cost-breakdown-row">
                      <span className="cost-breakdown-label">{item.provider} / {item.model}</span>
                      <Sparkline data={item.sparkline} label={`${item.provider} / ${item.model}`} format={formatCost} />
                      <strong>{formatCost(item.cost_usd)}</strong>
                    </div>
                    <div className="cost-breakdown-track">
                      <div className="cost-breakdown-fill" style={{ width: `${pct}%` }} />
                    </div>
                  </li>
                );
              })}
            </ul>
          </div>
        </div>
      )}
    </section>
  );
});
