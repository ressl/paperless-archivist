import { memo, useMemo, useState } from 'react';
import { DashboardStats } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { formatCost, formatMs, stageLabel } from '../lib/format';
import { ChartPanel, Sparkline } from './Primitives';

type ProviderTableSortKey = 'provider' | 'model' | 'stage' | 'request_count' | 'avg_duration_ms' | 'p95_duration_ms' | 'tokens' | 'cost' | 'feedback' | 'acceptance';

export const ProviderTable = memo(function ProviderTable({ usage }: { usage: DashboardStats['provider_usage'] }) {
  const { t, formatPercent: formatPercentValue } = useI18n();
  const [sortKey, setSortKey] = useState<ProviderTableSortKey>('request_count');
  const [sortDir, setSortDir] = useState<'asc' | 'desc'>('desc');
  const [stageFilter, setStageFilter] = useState<string>('');
  const [providerFilter, setProviderFilter] = useState<string>('');

  const stageOptions = useMemo(() => {
    const set = new Set<string>();
    for (const item of usage) set.add(item.stage);
    return Array.from(set).sort();
  }, [usage]);
  const providerOptions = useMemo(() => {
    const set = new Set<string>();
    for (const item of usage) set.add(item.provider);
    return Array.from(set).sort();
  }, [usage]);

  const filtered = useMemo(() => {
    return usage.filter((item) => {
      if (stageFilter && item.stage !== stageFilter) return false;
      if (providerFilter && item.provider !== providerFilter) return false;
      return true;
    });
  }, [usage, stageFilter, providerFilter]);

  const sorted = useMemo(() => {
    const arr = [...filtered];
    const dir = sortDir === 'asc' ? 1 : -1;
    arr.sort((a, b) => {
      const aTokens = a.input_tokens + a.output_tokens;
      const bTokens = b.input_tokens + b.output_tokens;
      const aFeedback = a.positive_feedback - a.negative_feedback;
      const bFeedback = b.positive_feedback - b.negative_feedback;
      let cmp = 0;
      switch (sortKey) {
        case 'provider': cmp = a.provider.localeCompare(b.provider); break;
        case 'model': cmp = a.model.localeCompare(b.model); break;
        case 'stage': cmp = a.stage.localeCompare(b.stage); break;
        case 'request_count': cmp = a.request_count - b.request_count; break;
        case 'avg_duration_ms': cmp = a.avg_duration_ms - b.avg_duration_ms; break;
        case 'p95_duration_ms': cmp = a.p95_duration_ms - b.p95_duration_ms; break;
        case 'tokens': cmp = aTokens - bTokens; break;
        case 'cost': cmp = (a.estimated_cost_usd ?? -1) - (b.estimated_cost_usd ?? -1); break;
        case 'feedback': cmp = aFeedback - bFeedback; break;
        case 'acceptance': cmp = (a.acceptance_rate ?? -1) - (b.acceptance_rate ?? -1); break;
      }
      return cmp * dir;
    });
    return arr;
  }, [filtered, sortKey, sortDir]);

  const handleSort = (key: ProviderTableSortKey) => {
    if (sortKey === key) {
      setSortDir((dir) => (dir === 'asc' ? 'desc' : 'asc'));
    } else {
      setSortKey(key);
      setSortDir('desc');
    }
  };

  const arrow = (key: ProviderTableSortKey) => (sortKey === key ? (sortDir === 'asc' ? ' ▲' : ' ▼') : '');

  return (
    <ChartPanel title={t('dashboard.chart.provider_usage')} wide>
      <div className="provider-filter-bar">
        <label>
          <span>{t('dashboard.provider.filter_stage')}</span>
          <select value={stageFilter} onChange={(event) => setStageFilter(event.target.value)}>
            <option value="">{t('dashboard.provider.filter_all')}</option>
            {stageOptions.map((stage) => (
              <option key={stage} value={stage}>{stageLabel(stage, t)}</option>
            ))}
          </select>
        </label>
        <label>
          <span>{t('dashboard.provider.filter_provider')}</span>
          <select value={providerFilter} onChange={(event) => setProviderFilter(event.target.value)}>
            <option value="">{t('dashboard.provider.filter_all')}</option>
            {providerOptions.map((provider) => (
              <option key={provider} value={provider}>{provider}</option>
            ))}
          </select>
        </label>
      </div>
      <div className="table-wrap compact-table provider-usage-table">
        <table>
          <thead>
            <tr>
              <th><button type="button" onClick={() => handleSort('provider')}>{t('dashboard.provider.provider')}{arrow('provider')}</button></th>
              <th><button type="button" onClick={() => handleSort('model')}>{t('dashboard.provider.model')}{arrow('model')}</button></th>
              <th><button type="button" onClick={() => handleSort('stage')}>{t('dashboard.provider.stage')}{arrow('stage')}</button></th>
              <th><button type="button" onClick={() => handleSort('request_count')}>{t('dashboard.provider.requests')}{arrow('request_count')}</button></th>
              <th><button type="button" onClick={() => handleSort('avg_duration_ms')}>{t('dashboard.provider.avg')}{arrow('avg_duration_ms')}</button></th>
              <th><button type="button" onClick={() => handleSort('p95_duration_ms')}>{t('dashboard.provider.p95')}{arrow('p95_duration_ms')}</button></th>
              <th>{t('dashboard.provider.latency_trend')}</th>
              <th><button type="button" onClick={() => handleSort('tokens')}>{t('dashboard.provider.tokens')}{arrow('tokens')}</button></th>
              <th><button type="button" onClick={() => handleSort('cost')}>{t('dashboard.provider.cost')}{arrow('cost')}</button></th>
              <th><button type="button" onClick={() => handleSort('feedback')}>{t('dashboard.provider.feedback')}{arrow('feedback')}</button></th>
              <th><button type="button" onClick={() => handleSort('acceptance')}>{t('dashboard.provider.acceptance')}{arrow('acceptance')}</button></th>
            </tr>
          </thead>
          <tbody>
            {sorted.length === 0 && (
              <tr><td colSpan={11}>{t('dashboard.provider.no_usage')}</td></tr>
            )}
            {sorted.map((item) => (
              <tr key={`${item.provider}-${item.model}-${item.stage}`}>
                <td>{item.provider}</td>
                <td>{item.model}</td>
                <td>{stageLabel(item.stage, t)}</td>
                <td>{item.request_count}</td>
                <td>{formatMs(item.avg_duration_ms)}</td>
                <td>{formatMs(item.p95_duration_ms)}</td>
                <td>
                  <Sparkline
                    data={item.latency_history ?? []}
                    label={t('dashboard.provider.latency_trend')}
                    format={(value) => formatMs(value)}
                  />
                </td>
                <td>{item.input_tokens + item.output_tokens}</td>
                <td>{formatCost(item.estimated_cost_usd)}</td>
                <td>{item.positive_feedback}/{item.negative_feedback}</td>
                <td>{item.acceptance_rate == null ? '-' : formatPercentValue(item.acceptance_rate)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </ChartPanel>
  );
});
