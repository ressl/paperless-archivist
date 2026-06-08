import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  Bar,
  BarChart,
  CartesianGrid,
  ComposedChart,
  Legend,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis
} from 'recharts';
import { api, StatisticsBreakdownRow, StatisticsBucket, StatisticsResponse } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import type { MessageKey } from '../i18n/messages';
import { formatCost, formatMs } from '../lib/format';
import { KpiCard, PageHeader, localizedErrorMessage } from '../lib/ui';
import { ChartPanel } from '../dashboard/Primitives';

type RangePreset = '24h' | '7d' | '30d' | '90d' | 'all';

const RANGE_PRESETS: Array<{ key: RangePreset; labelKey: MessageKey }> = [
  { key: '24h', labelKey: 'stats.range.24h' },
  { key: '7d', labelKey: 'stats.range.7d' },
  { key: '30d', labelKey: 'stats.range.30d' },
  { key: '90d', labelKey: 'stats.range.90d' },
  { key: 'all', labelKey: 'stats.range.all' }
];

const BUCKETS: StatisticsBucket[] = ['day', 'week', 'month'];

const BUCKET_LABEL_KEYS: Record<StatisticsBucket, MessageKey> = {
  hour: 'stats.bucket.hour',
  day: 'stats.bucket.day',
  week: 'stats.bucket.week',
  month: 'stats.bucket.month'
};

// All-time start. The backend clamps to the earliest record, so a far-past
// date simply means "everything".
const ALL_TIME_FROM = '2000-01-01';

function isoDay(date: Date): string {
  return date.toISOString().slice(0, 10);
}

function presetFrom(preset: RangePreset): string {
  const now = Date.now();
  switch (preset) {
    case '24h':
      return isoDay(new Date(now - 24 * 60 * 60 * 1000));
    case '7d':
      return isoDay(new Date(now - 7 * 24 * 60 * 60 * 1000));
    case '30d':
      return isoDay(new Date(now - 30 * 24 * 60 * 60 * 1000));
    case '90d':
      return isoDay(new Date(now - 90 * 24 * 60 * 60 * 1000));
    case 'all':
      return ALL_TIME_FROM;
  }
}

export function Statistics({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatNumber, formatDateTime } = useI18n();
  const [preset, setPreset] = useState<RangePreset>('30d');
  const [from, setFrom] = useState<string>(() => presetFrom('30d'));
  const [to, setTo] = useState<string>(() => isoDay(new Date()));
  const [bucket, setBucket] = useState<StatisticsBucket>('day');
  const [data, setData] = useState<StatisticsResponse | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(() => {
    setLoading(true);
    return api
      .statistics({ from, to, bucket })
      .then((result) => {
        setData(result);
        setError(null);
      })
      .catch((err) => setError(localizedErrorMessage(err, t)))
      .finally(() => setLoading(false));
  }, [from, to, bucket, setError, t]);

  useEffect(() => {
    void load();
  }, [load]);

  const applyPreset = (next: RangePreset) => {
    setPreset(next);
    setFrom(presetFrom(next));
    setTo(isoDay(new Date()));
  };

  // A free-form date change drops the preset highlight since the range is now custom.
  const onFromChange = (value: string) => {
    setPreset('30d');
    setFrom(value);
  };
  const onToChange = (value: string) => {
    setPreset('30d');
    setTo(value);
  };

  // Short, locale-aware axis labels keyed off the bucket granularity.
  const formatAxis = useCallback(
    (value: string) => {
      const date = new Date(value);
      if (!Number.isFinite(date.getTime())) return value;
      if (bucket === 'month') {
        return new Intl.DateTimeFormat(undefined, { month: 'short', year: '2-digit' }).format(date);
      }
      return new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' }).format(date);
    },
    [bucket]
  );

  const usageSeries = useMemo(
    () => (data?.time_series ?? []).map((point) => ({ ...point, label: formatAxis(point.bucket) })),
    [data?.time_series, formatAxis]
  );
  const throughputSeries = useMemo(
    () => (data?.throughput_series ?? []).map((point) => ({ ...point, label: formatAxis(point.bucket) })),
    [data?.throughput_series, formatAxis]
  );

  const summary = data?.summary;
  const isCustom = !RANGE_PRESETS.some(
    (p) => p.key === preset && presetFrom(p.key) === from && isoDay(new Date()) === to
  );

  return (
    <section className="page">
      <PageHeader title={t('stats.title')} />

      <div className="toolbar">
        <div className="range-tabs" role="group" aria-label={t('stats.range_label')}>
          {RANGE_PRESETS.map((option) => (
            <button
              key={option.key}
              type="button"
              className={!isCustom && preset === option.key ? 'active' : ''}
              aria-pressed={!isCustom && preset === option.key}
              onClick={() => applyPreset(option.key)}
            >
              {t(option.labelKey)}
            </button>
          ))}
        </div>
        <label className="form-field">
          <span className="form-field-label">{t('stats.from')}</span>
          <input type="date" value={from} max={to} onChange={(event) => onFromChange(event.target.value)} />
        </label>
        <label className="form-field">
          <span className="form-field-label">{t('stats.to')}</span>
          <input type="date" value={to} min={from} onChange={(event) => onToChange(event.target.value)} />
        </label>
        <div className="range-tabs" role="group" aria-label={t('stats.bucket_label')}>
          {BUCKETS.map((option) => (
            <button
              key={option}
              type="button"
              className={bucket === option ? 'active' : ''}
              aria-pressed={bucket === option}
              onClick={() => setBucket(option)}
            >
              {t(BUCKET_LABEL_KEYS[option])}
            </button>
          ))}
        </div>
      </div>

      {data && (
        <p className="chart-panel-subtitle" aria-live="polite">
          {t('stats.range_summary', {
            from: formatDateTime(data.from),
            to: formatDateTime(data.to),
            bucket: t(BUCKET_LABEL_KEYS[data.bucket])
          })}
        </p>
      )}

      <div className="card-grid card-grid--default">
        <KpiCard label={t('stats.kpi.requests')} value={summary ? formatNumber(summary.request_count) : '—'} />
        <KpiCard label={t('stats.kpi.output_tokens')} value={summary ? formatNumber(summary.output_tokens) : '—'} />
        <KpiCard label={t('stats.kpi.avg_latency')} value={summary ? formatMs(summary.avg_duration_ms) : '—'} />
        <KpiCard
          label={t('stats.kpi.cost')}
          value={summary && summary.estimated_cost_usd != null ? formatCost(summary.estimated_cost_usd) : '—'}
        />
        <KpiCard
          label={t('stats.kpi.succeeded')}
          value={summary ? formatNumber(summary.jobs_succeeded) : '—'}
          tone="success"
        />
        <KpiCard
          label={t('stats.kpi.failed')}
          value={summary ? formatNumber(summary.jobs_failed) : '—'}
          tone={summary && summary.jobs_failed > 0 ? 'danger' : 'neutral'}
        />
      </div>

      {loading && !data ? (
        <p className="empty-state">{t('stats.loading')}</p>
      ) : data && data.summary.request_count === 0 ? (
        <p className="empty-state">{t('stats.empty')}</p>
      ) : data ? (
        <>
          <ChartPanel title={t('stats.chart.usage')} wide>
            <ResponsiveContainer width="100%" height={280}>
              <ComposedChart data={usageSeries}>
                <CartesianGrid strokeDasharray="3 3" vertical={false} />
                <XAxis dataKey="label" />
                <YAxis yAxisId="requests" allowDecimals={false} width={56} />
                <YAxis yAxisId="tokens" orientation="right" width={64} />
                <Tooltip />
                <Legend />
                <Bar
                  yAxisId="requests"
                  dataKey="request_count"
                  name={t('stats.series.requests')}
                  fill="var(--info)"
                  radius={[3, 3, 0, 0]}
                />
                <Line
                  yAxisId="tokens"
                  type="monotone"
                  dataKey="output_tokens"
                  name={t('stats.series.output_tokens')}
                  stroke="var(--success)"
                  strokeWidth={2}
                  dot={false}
                />
              </ComposedChart>
            </ResponsiveContainer>
          </ChartPanel>

          <div className="card-grid card-grid--wide">
            <ChartPanel title={t('stats.chart.latency')}>
              <ResponsiveContainer width="100%" height={240}>
                <LineChart data={usageSeries}>
                  <CartesianGrid strokeDasharray="3 3" vertical={false} />
                  <XAxis dataKey="label" />
                  <YAxis width={56} />
                  <Tooltip formatter={(value) => formatMs(Number(value))} />
                  <Line
                    type="monotone"
                    dataKey="avg_duration_ms"
                    name={t('stats.series.avg_latency')}
                    stroke="var(--warning)"
                    strokeWidth={2}
                    dot={false}
                  />
                </LineChart>
              </ResponsiveContainer>
            </ChartPanel>

            <ChartPanel title={t('stats.chart.throughput')}>
              <ResponsiveContainer width="100%" height={240}>
                <BarChart data={throughputSeries}>
                  <CartesianGrid strokeDasharray="3 3" vertical={false} />
                  <XAxis dataKey="label" />
                  <YAxis allowDecimals={false} width={56} />
                  <Tooltip />
                  <Legend />
                  <Bar dataKey="succeeded" name={t('stats.series.succeeded')} stackId="t" fill="var(--success)" />
                  <Bar dataKey="failed" name={t('stats.series.failed')} stackId="t" fill="var(--danger)" />
                  <Bar dataKey="cancelled" name={t('stats.series.cancelled')} stackId="t" fill="var(--muted)" />
                </BarChart>
              </ResponsiveContainer>
            </ChartPanel>
          </div>

          <BreakdownTable
            title={t('stats.breakdown.by_provider')}
            nameHeader={t('stats.col.provider')}
            rows={data.by_provider}
            nameOf={(row) => row.provider ?? '—'}
          />
          <BreakdownTable
            title={t('stats.breakdown.by_model')}
            nameHeader={t('stats.col.model')}
            rows={data.by_model}
            nameOf={(row) => row.model ?? '—'}
          />
          <BreakdownTable
            title={t('stats.breakdown.by_stage')}
            nameHeader={t('stats.col.stage')}
            rows={data.by_stage}
            nameOf={(row) => row.stage ?? '—'}
          />
        </>
      ) : null}
    </section>
  );
}

function BreakdownTable({
  title,
  nameHeader,
  rows,
  nameOf
}: {
  title: string;
  nameHeader: string;
  rows: StatisticsBreakdownRow[];
  nameOf: (row: StatisticsBreakdownRow) => string;
}) {
  const { t, formatNumber } = useI18n();
  const sorted = useMemo(() => [...rows].sort((a, b) => b.request_count - a.request_count), [rows]);
  return (
    <section className="card chart-panel wide" aria-label={title}>
      <h3>{title}</h3>
      {sorted.length === 0 ? (
        <p className="empty-state compact">{t('stats.empty')}</p>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>{nameHeader}</th>
                <th>{t('stats.col.requests')}</th>
                <th>{t('stats.col.output_tokens')}</th>
                <th>{t('stats.col.avg_latency')}</th>
                <th>{t('stats.col.cost')}</th>
              </tr>
            </thead>
            <tbody>
              {sorted.map((row, idx) => (
                <tr key={`${nameOf(row)}-${idx}`}>
                  <td>{nameOf(row)}</td>
                  <td>{formatNumber(row.request_count)}</td>
                  <td>{formatNumber(row.output_tokens)}</td>
                  <td>{formatMs(row.avg_duration_ms)}</td>
                  <td>{row.estimated_cost_usd != null ? formatCost(row.estimated_cost_usd) : '—'}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
