import { memo, useMemo, useState } from 'react';
import { DashboardStats } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { formatMs, stageLabel } from '../lib/format';
import { ChartPanel } from './Primitives';
import { StageMatrixRow, buildStageMatrix } from './helpers';

type StageMatrixSortKey = 'stage' | 'queued' | 'running' | 'failed' | 'avg_ms' | 'p95_ms' | 'throughput_per_hour';

export const StageMatrix = memo(function StageMatrix({
  stats,
  onStageSelect
}: {
  stats: DashboardStats | null;
  onStageSelect?: (stage: string) => void;
}) {
  const { t, formatNumber } = useI18n();
  const [sortKey, setSortKey] = useState<StageMatrixSortKey>('queued');
  const [sortDir, setSortDir] = useState<'asc' | 'desc'>('desc');
  const rows = useMemo(() => buildStageMatrix(stats), [stats]);
  const sorted = useMemo(() => {
    const arr = [...rows];
    arr.sort((a, b) => {
      const va = a[sortKey];
      const vb = b[sortKey];
      if (typeof va === 'string' && typeof vb === 'string') {
        return sortDir === 'asc' ? va.localeCompare(vb) : vb.localeCompare(va);
      }
      const diff = (va as number) - (vb as number);
      return sortDir === 'asc' ? diff : -diff;
    });
    return arr;
  }, [rows, sortKey, sortDir]);
  const bottleneckStage = useMemo(() => {
    let pick: StageMatrixRow | null = null;
    for (const row of rows) {
      if (row.bottleneck_score > 0 && (!pick || row.bottleneck_score > pick.bottleneck_score)) {
        pick = row;
      }
    }
    return pick?.queued && pick.bottleneck_score > 1 ? pick.stage : null;
  }, [rows]);

  const handleSort = (key: StageMatrixSortKey) => {
    if (sortKey === key) {
      setSortDir((dir) => (dir === 'asc' ? 'desc' : 'asc'));
    } else {
      setSortKey(key);
      setSortDir('desc');
    }
  };

  const arrow = (key: StageMatrixSortKey) => (sortKey === key ? (sortDir === 'asc' ? ' ▲' : ' ▼') : '');

  return (
    <ChartPanel title={t('dashboard.stage_matrix.title')} wide>
      <p className="chart-panel-subtitle">{t('dashboard.stage_matrix.subtitle')}</p>
      <div className="table-wrap compact-table stage-matrix-table">
        <table>
          <thead>
            <tr>
              <th><button type="button" onClick={() => handleSort('stage')}>{t('dashboard.stage_matrix.stage')}{arrow('stage')}</button></th>
              <th><button type="button" onClick={() => handleSort('queued')}>{t('dashboard.stage_matrix.queued')}{arrow('queued')}</button></th>
              <th><button type="button" onClick={() => handleSort('running')}>{t('dashboard.stage_matrix.running')}{arrow('running')}</button></th>
              <th><button type="button" onClick={() => handleSort('failed')}>{t('dashboard.stage_matrix.failed')}{arrow('failed')}</button></th>
              <th><button type="button" onClick={() => handleSort('avg_ms')}>{t('dashboard.stage_matrix.avg')}{arrow('avg_ms')}</button></th>
              <th><button type="button" onClick={() => handleSort('p95_ms')}>{t('dashboard.stage_matrix.p95')}{arrow('p95_ms')}</button></th>
              <th><button type="button" onClick={() => handleSort('throughput_per_hour')}>{t('dashboard.stage_matrix.throughput_per_hour')}{arrow('throughput_per_hour')}</button></th>
              <th>{t('dashboard.stage_matrix.complete')}</th>
              <th scope="col">{t('dashboard.stage_matrix.bottleneck')}</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((row) => (
              <tr key={row.stage} className={bottleneckStage === row.stage ? 'is-bottleneck' : ''}>
                <td>
                  {onStageSelect ? (
                    <button className="link-button" type="button" onClick={() => onStageSelect(row.stage)}>
                      {stageLabel(row.stage, t)}
                    </button>
                  ) : (
                    stageLabel(row.stage, t)
                  )}
                </td>
                <td>{formatNumber(row.queued)}</td>
                <td>{formatNumber(row.running)}</td>
                <td className={row.failed > 0 ? 'cell-danger' : ''}>{formatNumber(row.failed)}</td>
                <td>{formatMs(row.avg_ms)}</td>
                <td>{formatMs(row.p95_ms)}</td>
                <td>{row.throughput_per_hour > 0 ? formatNumber(Math.round(row.throughput_per_hour)) : '-'}</td>
                <td>{formatNumber(row.complete)}</td>
                <td>{bottleneckStage === row.stage && <span className="bottleneck-badge">{t('dashboard.stage_matrix.bottleneck')}</span>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </ChartPanel>
  );
});
