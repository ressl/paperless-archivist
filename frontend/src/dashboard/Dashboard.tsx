import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { useDashboardLive, useDashboardStats, useFreshness } from './hooks';
import {
  Activity,
  AlertTriangle,
  Check,
  Database,
  FileText,
  GitCompare,
  History,
  ListChecks,
  Play,
  Power,
  RefreshCw,
  RotateCcw,
  Settings,
  Tags,
  X
} from 'lucide-react';
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
import {
  api,
  CompletionTagReconcileResult,
  DashboardLiveFailure,
  DashboardLiveStatus,
  DashboardRange,
  DashboardStats,
  DashboardStatusCount,
  NeedsAttentionItem,
  PaperlessConsistencyResult,
  ProcessingMode,
  RecoveryCandidate
} from '../api/client';
import { localizedMessage, useI18n, type TFunction } from '../i18n/I18nProvider';
import { ActionButton, PageHeader, Status, localizedErrorMessage, run } from '../lib/ui';
import {
  deltaTone,
  formatCost,
  formatDelta,
  formatMs,
  formatMttc,
  formatPercent as formatPercentStandalone,
  shortId,
  stageLabel,
  statusLabel
} from '../lib/format';
import { workflowModeDescription, workflowModeLabel, workflowModeOptions } from '../App';

const defaultDashboardRanges: Array<{ key: DashboardRange; label: string }> = [
  { key: '24h', label: '24h' },
  { key: '7d', label: '7d' },
  { key: '30d', label: '30d' },
  { key: '90d', label: '90d' },
  { key: '12m', label: '12m' },
  { key: 'all', label: 'All' }
];

const defaultStageStatus = ['ocr', 'title', 'document_type', 'correspondent', 'document_date', 'tags', 'fields'].map((stage) => ({
  stage,
  complete: 0,
  pending: 0,
  failed: 0,
  waiting_review: 0,
  running: 0
}));

const defaultJobStatus: DashboardStatusCount[] = [
  { status: 'queued', count: 0 },
  { status: 'running', count: 0 },
  { status: 'succeeded', count: 0 },
  { status: 'failed', count: 0 }
];

function statusChartData(items: DashboardStatusCount[], t: TFunction) {
  return items.map((item) => ({
    ...item,
    label: statusLabel(item.status, t)
  }));
}

function liveFailureKind(failure: DashboardLiveFailure) {
  return failure.failure_kind || (failure.status === 'failed' ? 'failed' : 'retry_scheduled');
}

function liveFailureTiming(
  failure: DashboardLiveFailure,
  kind: string,
  t: TFunction,
  formatRelative: (value?: string | null) => string
) {
  if (kind === 'retry_ready') return t('dashboard.live.retry_ready');
  if (failure.next_attempt_at) return t('dashboard.live.next_retry', { time: formatRelative(failure.next_attempt_at) });
  return t('dashboard.live.updated', { time: formatRelative(failure.updated_at) });
}

function computeHealthScore(stats: DashboardStats | null, live: DashboardLiveStatus | null) {
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

function healthTone(score: number | null): 'success' | 'warning' | 'danger' | 'neutral' {
  if (score == null) return 'neutral';
  if (score >= 90) return 'success';
  if (score >= 70) return 'warning';
  return 'danger';
}

type StageMatrixRow = {
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

type StageMatrixSortKey = 'stage' | 'queued' | 'running' | 'failed' | 'avg_ms' | 'p95_ms' | 'throughput_per_hour';

function buildStageMatrix(stats: DashboardStats | null): StageMatrixRow[] {
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

type TimelineEntry = {
  id: string;
  at: string;
  kind: 'llm' | 'failure';
  stage: string;
  primary: string;
  secondary?: string;
};

function ChartPanel({ title, wide, children }: { title: string; wide?: boolean; children: ReactNode }) {
  return (
    <section className={`chart-panel${wide ? ' wide' : ''}`}>
      <h3>{title}</h3>
      {children}
    </section>
  );
}

function AlertsBar({
  items,
  onAction
}: {
  items: NeedsAttentionItem[];
  onAction: (item: NeedsAttentionItem) => void;
}) {
  const { t, formatNumber } = useI18n();
  if (items.length === 0) return null;
  return (
    <section className="alerts-bar" role="region" aria-label={t('dashboard.alerts.title')}>
      <header>
        <AlertTriangle size={16} />
        <strong>{t('dashboard.alerts.title')}</strong>
      </header>
      <ul>
        {items.map((item, idx) => (
          <li key={`${item.kind}-${idx}`} className={`alert-item severity-${item.severity}`}>
            <div className="alert-text">
              <strong>{item.title}</strong>
              <span>{item.description}</span>
            </div>
            {item.count != null && (
              <span className="alert-count" aria-label="count">{formatNumber(item.count)}</span>
            )}
            {item.action_key && (
              <button type="button" onClick={() => onAction(item)} className="alert-action">
                {localizedMessage(item.action_key, t, item.action_key)}
              </button>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

function HealthBadge({
  score,
  generatedAt,
  nextRefreshIn,
  pulse
}: {
  score: number | null;
  generatedAt: string | null;
  nextRefreshIn: number;
  pulse: boolean;
}) {
  const { t, formatRelativeTime } = useI18n();
  const tone = healthTone(score);
  const label = score == null
    ? t('dashboard.health.score_label')
    : tone === 'success'
      ? t('dashboard.health.healthy')
      : tone === 'warning'
        ? t('dashboard.health.degraded')
        : t('dashboard.health.unhealthy');
  return (
    <div className={`health-badge ${tone}`} role="status">
      <span className="health-score">{score ?? '-'}</span>
      <div>
        <strong>{label}</strong>
        <em>{generatedAt ? formatRelativeTime(generatedAt) : '-'}</em>
      </div>
      <div className="freshness-indicator" aria-live="polite">
        <span className={`pulse-dot ${pulse ? 'is-pulsing' : ''}`} aria-hidden="true" />
        <em>{t('dashboard.freshness.next', { seconds: nextRefreshIn })}</em>
      </div>
    </div>
  );
}

function QuotaBar({
  label,
  remaining,
  limit
}: {
  label: string;
  remaining: number | null | undefined;
  limit: number | null | undefined;
}) {
  const { t, formatNumber } = useI18n();
  if (!limit || limit <= 0) {
    return (
      <div className="quota-bar unlimited">
        <span>{label}</span>
        <em>{t('dashboard.workflow.quota_unlimited')}</em>
      </div>
    );
  }
  const safeRemaining = remaining ?? limit;
  const used = Math.max(0, limit - safeRemaining);
  const pct = Math.min(100, Math.round((used / limit) * 100));
  const tone = pct >= 90 ? 'danger' : pct >= 70 ? 'warning' : 'success';
  return (
    <div className={`quota-bar ${tone}`}>
      <header>
        <span>{label}</span>
        <em>{t('dashboard.workflow.quota_used', { used: formatNumber(used), limit: formatNumber(limit) })}</em>
      </header>
      <div className="quota-track" role="progressbar" aria-valuenow={used} aria-valuemin={0} aria-valuemax={limit}>
        <div className="quota-fill" style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}

function AutoProcessingCard({
  enabled,
  mode,
  safety,
  nextSelectorScanAt,
  busy,
  canToggle,
  onModeChange,
  onPauseChange
}: {
  enabled: boolean;
  mode: ProcessingMode;
  safety?: DashboardLiveStatus['workflow_safety'] | null;
  nextSelectorScanAt?: string | null;
  busy: boolean;
  canToggle: boolean;
  onModeChange: (mode: ProcessingMode) => void;
  onPauseChange: (paused: boolean) => void;
}) {
  const { t, formatRelativeTime: formatRelative } = useI18n();
  const paused = safety?.paused ?? false;
  const dryRun = safety?.dry_run ?? false;
  const hasQuota = !!(safety?.hourly_document_limit || safety?.daily_document_limit);
  return (
    <section className={`autopilot-card ${enabled ? 'enabled' : 'disabled'}`}>
      <div className="autopilot-body">
        <span>{t('dashboard.workflow.title')}</span>
        <strong>{workflowModeLabel(mode, t)}</strong>
        <p>{workflowModeDescription(mode, t)}</p>
        {dryRun && (
          <div className="dry-run-banner" role="alert">
            <AlertTriangle size={16} />
            <div>
              <strong>{t('dashboard.workflow.dry_run_banner_title')}</strong>
              <span>{t('dashboard.workflow.dry_run_banner_body')}</span>
            </div>
          </div>
        )}
        {hasQuota && (
          <div className="quota-bars">
            <QuotaBar
              label={t('dashboard.workflow.hourly_quota')}
              remaining={safety?.hourly_remaining}
              limit={safety?.hourly_document_limit}
            />
            <QuotaBar
              label={t('dashboard.workflow.daily_quota')}
              remaining={safety?.daily_remaining}
              limit={safety?.daily_document_limit}
            />
          </div>
        )}
        {nextSelectorScanAt && (
          <small>{t('dashboard.auto.next_scan', { time: formatRelative(nextSelectorScanAt) })}</small>
        )}
      </div>
      <div className="mode-button-group" role="group" aria-label={t('dashboard.auto.processing_mode')}>
        <button
          type="button"
          disabled={busy || !canToggle}
          aria-pressed={paused}
          onClick={() => onPauseChange(!paused)}
        >
          <Power size={16} /> {paused ? t('dashboard.auto.resume') : t('dashboard.auto.pause')}
        </button>
        {workflowModeOptions.map((option) => (
          <button
            key={option.value}
            className={mode === option.value ? 'active' : ''}
            type="button"
            disabled={busy || !canToggle || mode === option.value}
            aria-pressed={mode === option.value}
            onClick={() => onModeChange(option.value)}
            title={t(option.descriptionKey)}
          >
            {option.value === 'manual_review' ? <Power size={16} /> : <Play size={16} />}
            {mode === option.value && busy ? t('dashboard.auto.updating') : t(option.labelKey)}
          </button>
        ))}
        {!canToggle && <small>{t('generic.admin_only')}</small>}
      </div>
    </section>
  );
}

function ServiceStatusCard({
  label,
  icon,
  status
}: {
  label: string;
  icon: ReactNode;
  status?: DashboardLiveStatus['llm'] | null;
}) {
  const { t, formatRelativeTime: formatRelative } = useI18n();
  const state = status?.state ?? 'idle';
  return (
    <section className={`service-card ${state}`}>
      <header>
        <span>{icon}</span>
        <strong>{label}</strong>
        <Status value={state} />
      </header>
      <p>{status?.title ?? t('dashboard.service.loading')}</p>
      <small>{status?.description ?? t('dashboard.service.waiting')}</small>
      <em>{status?.last_event_at ? formatRelative(status.last_event_at) : t('dashboard.service.no_activity')}</em>
    </section>
  );
}

function RecoveryPanel({
  recovery,
  busy,
  onRefresh,
  onRecoverLeases,
  onRecoverRuns
}: {
  recovery: { older_than_seconds: number; items: RecoveryCandidate[] } | null;
  busy: boolean;
  onRefresh: () => void;
  onRecoverLeases: () => void;
  onRecoverRuns: () => void;
}) {
  const { t, formatNumber } = useI18n();
  const items = recovery?.items ?? [];
  const staleLeases = items.filter((item) => item.reason === 'stale_lease').length;
  const stuckRuns = items.filter((item) => item.reason === 'stuck_run_without_active_jobs').length;
  return (
    <section className="recovery-panel">
      <div>
        <strong>{t('dashboard.recovery.title')}</strong>
        <p>{t('dashboard.recovery.description')}</p>
      </div>
      <div className="recovery-counts">
        <span>{t('dashboard.recovery.stale_leases')}: {formatNumber(staleLeases)}</span>
        <span>{t('dashboard.recovery.stuck_runs')}: {formatNumber(stuckRuns)}</span>
      </div>
      <div className="toolbar">
        <button type="button" disabled={busy} onClick={onRefresh}>
          <RefreshCw size={16} /> {t('generic.refresh')}
        </button>
        <button type="button" disabled={busy || staleLeases === 0} onClick={onRecoverLeases}>
          <RotateCcw size={16} /> {t('dashboard.recovery.requeue_leases')}
        </button>
        <button type="button" disabled={busy || stuckRuns === 0} onClick={onRecoverRuns}>
          <AlertTriangle size={16} /> {t('dashboard.recovery.recover_runs')}
        </button>
      </div>
    </section>
  );
}

function PaperlessMaintenancePanel({
  consistency,
  reconcile,
  busy,
  onCheckConsistency,
  onDryRunReconcile,
  onApplyReconcile
}: {
  consistency: PaperlessConsistencyResult | null;
  reconcile: CompletionTagReconcileResult | null;
  busy: boolean;
  onCheckConsistency: () => void;
  onDryRunReconcile: () => void;
  onApplyReconcile: () => void;
}) {
  const { t, formatNumber } = useI18n();
  const mismatchCount = consistency?.mismatches.length ?? 0;
  const missingCount = consistency?.missing_local.length ?? 0;
  const staleCount = consistency?.stale_local.length ?? 0;
  const plannedCount = reconcile?.planned.length ?? 0;
  const appliedCount = reconcile?.applied.length ?? 0;
  return (
    <section className="recovery-panel paperless-maintenance-panel">
      <div>
        <strong>{t('dashboard.paperless_tools.title')}</strong>
        <p>{t('dashboard.paperless_tools.description')}</p>
      </div>
      <div className="recovery-counts">
        {consistency ? (
          <>
            <span>{t('dashboard.paperless_tools.checked')}: {formatNumber(consistency.documents_checked)}</span>
            <span>{t('dashboard.paperless_tools.missing')}: {formatNumber(missingCount)}</span>
            <span>{t('dashboard.paperless_tools.stale')}: {formatNumber(staleCount)}</span>
            <span>{t('dashboard.paperless_tools.mismatches')}: {formatNumber(mismatchCount)}</span>
          </>
        ) : (
          <span>{t('dashboard.paperless_tools.not_checked')}</span>
        )}
        {reconcile && (
          <span>
            {reconcile.dry_run
              ? t('dashboard.paperless_tools.planned')
              : t('dashboard.paperless_tools.applied')}: {formatNumber(reconcile.dry_run ? plannedCount : appliedCount)}
          </span>
        )}
      </div>
      <div className="toolbar">
        <button type="button" disabled={busy} onClick={onCheckConsistency}>
          <GitCompare size={16} /> {t('dashboard.paperless_tools.check')}
        </button>
        <button type="button" disabled={busy} onClick={onDryRunReconcile}>
          <Tags size={16} /> {t('dashboard.paperless_tools.dry_run')}
        </button>
        <button type="button" disabled={busy || !reconcile?.dry_run || plannedCount === 0} onClick={onApplyReconcile}>
          <Check size={16} /> {t('dashboard.paperless_tools.apply')}
        </button>
      </div>
    </section>
  );
}

function MaintenanceDrawer({
  open,
  onClose,
  recovery,
  consistency,
  reconcile,
  recoveryBusy,
  maintenanceBusy,
  onRefreshRecovery,
  onRecoverLeases,
  onRecoverRuns,
  onCheckConsistency,
  onDryRunReconcile,
  onApplyReconcile,
  queueBusy,
  onQueueSync,
  onQueueOcr,
  onQueueTags,
  onQueueFull
}: {
  open: boolean;
  onClose: () => void;
  recovery: { older_than_seconds: number; items: RecoveryCandidate[] } | null;
  consistency: PaperlessConsistencyResult | null;
  reconcile: CompletionTagReconcileResult | null;
  recoveryBusy: boolean;
  maintenanceBusy: boolean;
  onRefreshRecovery: () => void;
  onRecoverLeases: () => void;
  onRecoverRuns: () => void;
  onCheckConsistency: () => void;
  onDryRunReconcile: () => void;
  onApplyReconcile: () => void;
  queueBusy: boolean;
  onQueueSync: () => void;
  onQueueOcr: () => void;
  onQueueTags: () => void;
  onQueueFull: () => void;
}) {
  const { t } = useI18n();
  useEffect(() => {
    if (!open) return;
    const handler = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <div className="drawer-root" role="dialog" aria-modal="true" aria-label={t('dashboard.maintenance.title')}>
      <div className="drawer-backdrop" onClick={onClose} />
      <aside className="drawer">
        <header>
          <strong>{t('dashboard.maintenance.title')}</strong>
          <button type="button" className="drawer-close" onClick={onClose} aria-label={t('dashboard.maintenance.close')}>
            <X size={18} />
          </button>
        </header>
        <RecoveryPanel
          recovery={recovery}
          busy={recoveryBusy}
          onRefresh={onRefreshRecovery}
          onRecoverLeases={onRecoverLeases}
          onRecoverRuns={onRecoverRuns}
        />
        <PaperlessMaintenancePanel
          consistency={consistency}
          reconcile={reconcile}
          busy={maintenanceBusy}
          onCheckConsistency={onCheckConsistency}
          onDryRunReconcile={onDryRunReconcile}
          onApplyReconcile={onApplyReconcile}
        />
        <section className="drawer-section">
          <strong>{t('dashboard.maintenance.queue_section')}</strong>
          <div className="toolbar">
            <ActionButton icon={<RefreshCw />} label={t('dashboard.action.sync')} busy={queueBusy} onClick={onQueueSync} />
            <ActionButton icon={<FileText />} label={t('dashboard.action.queue_ocr')} busy={queueBusy} onClick={onQueueOcr} />
            <ActionButton icon={<Tags />} label={t('dashboard.action.queue_tags')} busy={queueBusy} onClick={onQueueTags} />
            <ActionButton icon={<Play />} label={t('dashboard.action.queue_full')} busy={queueBusy} onClick={onQueueFull} />
          </div>
        </section>
      </aside>
    </div>
  );
}

function StageMatrix({ stats, onStageSelect }: { stats: DashboardStats | null; onStageSelect: (stage: string) => void }) {
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
              <th>{' '}</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((row) => (
              <tr key={row.stage} className={bottleneckStage === row.stage ? 'is-bottleneck' : ''}>
                <td>
                  <button className="link-button" type="button" onClick={() => onStageSelect(row.stage)}>
                    {stageLabel(row.stage, t)}
                  </button>
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
}

function ActivityTimeline({ live }: { live: DashboardLiveStatus | null }) {
  const { t, formatRelativeTime } = useI18n();
  const entries = useMemo<TimelineEntry[]>(() => {
    const llm = (live?.recent_llm_events ?? []).map<TimelineEntry>((event) => ({
      id: `llm-${event.id}`,
      at: event.created_at,
      kind: 'llm',
      stage: event.stage,
      primary: t('dashboard.timeline.llm_event', { provider: event.provider, model: event.model, stage: stageLabel(event.stage, t) }),
      secondary: event.duration_ms ? formatMs(event.duration_ms) : undefined
    }));
    const failures = (live?.recent_failures ?? []).map<TimelineEntry>((failure) => ({
      id: `failure-${failure.id}`,
      at: failure.updated_at,
      kind: 'failure',
      stage: failure.stage,
      primary: t('dashboard.timeline.failure', { document: failure.paperless_document_id, stage: stageLabel(failure.stage, t) }),
      secondary: failure.error_message
    }));
    return [...llm, ...failures].sort((a, b) => new Date(b.at).getTime() - new Date(a.at).getTime()).slice(0, 30);
  }, [live, t]);
  return (
    <section className="activity-timeline">
      <header>
        <History size={16} />
        <strong>{t('dashboard.timeline.title')}</strong>
      </header>
      {entries.length === 0 ? (
        <p className="empty-state compact">{t('dashboard.timeline.empty')}</p>
      ) : (
        <ol>
          {entries.map((entry) => (
            <li key={entry.id} className={`timeline-entry kind-${entry.kind}`}>
              <time>{formatRelativeTime(entry.at)}</time>
              <div>
                <strong>{entry.primary}</strong>
                {entry.secondary && <span>{entry.secondary}</span>}
              </div>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}

type ProviderTableSortKey = 'provider' | 'model' | 'stage' | 'request_count' | 'avg_duration_ms' | 'p95_duration_ms' | 'tokens' | 'cost' | 'feedback' | 'acceptance';

function ProviderTable({ usage }: { usage: DashboardStats['provider_usage'] }) {
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
              <th><button type="button" onClick={() => handleSort('tokens')}>{t('dashboard.provider.tokens')}{arrow('tokens')}</button></th>
              <th><button type="button" onClick={() => handleSort('cost')}>{t('dashboard.provider.cost')}{arrow('cost')}</button></th>
              <th><button type="button" onClick={() => handleSort('feedback')}>{t('dashboard.provider.feedback')}{arrow('feedback')}</button></th>
              <th><button type="button" onClick={() => handleSort('acceptance')}>{t('dashboard.provider.acceptance')}{arrow('acceptance')}</button></th>
            </tr>
          </thead>
          <tbody>
            {sorted.length === 0 && (
              <tr><td colSpan={10}>{t('dashboard.provider.no_usage')}</td></tr>
            )}
            {sorted.map((item) => (
              <tr key={`${item.provider}-${item.model}-${item.stage}`}>
                <td>{item.provider}</td>
                <td>{item.model}</td>
                <td>{stageLabel(item.stage, t)}</td>
                <td>{item.request_count}</td>
                <td>{formatMs(item.avg_duration_ms)}</td>
                <td>{formatMs(item.p95_duration_ms)}</td>
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
}

function CostPanel({ stats, range }: { stats: DashboardStats | null; range: DashboardRange }) {
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
  return (
    <section className="cost-panel chart-panel wide">
      <h3>{t('dashboard.cost.title', { range })}</h3>
      <p className="chart-panel-subtitle">{t('dashboard.cost.subtitle')}</p>
      {total == null || total <= 0 ? (
        <p className="empty-state compact">{t('dashboard.cost.no_data')}</p>
      ) : (
        <div className="cost-panel-grid">
          <div className="cost-total-card">
            <span>{t('dashboard.cost.total')}</span>
            <strong>{formatCost(total)}</strong>
            <em>{formatNumber(series.reduce((sum, b) => sum + b.request_count, 0))} requests</em>
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
}

function LiveProcessingPanel({ live }: { live: DashboardLiveStatus | null }) {
  const { t, formatNumber, formatRelativeTime: formatRelative } = useI18n();
  const activeJobs = live?.active_jobs ?? [];
  const activeRuns = live?.active_runs ?? [];
  const recentEvents = live?.recent_llm_events ?? [];
  const recentFailures = live?.recent_failures ?? [];
  const hardFailures = recentFailures.filter((failure) => liveFailureKind(failure) === 'failed').length;

  return (
    <aside className="live-processing-panel">
      <header>
        <div>
          <strong>{t('dashboard.live.title')}</strong>
          <span>{t('dashboard.live.subtitle')}</span>
        </div>
        <Status value={live?.workflow_mode ?? 'loading'} />
      </header>
      <div className="live-summary">
        <div>
          <span>{t('dashboard.live.runs')}</span>
          <strong>{formatNumber(activeRuns.length)}</strong>
        </div>
        <div>
          <span>{t('dashboard.live.jobs')}</span>
          <strong>{formatNumber(activeJobs.length)}</strong>
        </div>
        <div>
          <span>{t('dashboard.live.issues')}</span>
          <strong>{formatNumber(hardFailures || recentFailures.length)}</strong>
        </div>
      </div>

      <section className="live-debug-section">
        <h3>{t('dashboard.live.active_jobs')}</h3>
        {activeJobs.length === 0 && <p className="empty-state compact">{t('dashboard.live.no_active_jobs')}</p>}
        {activeJobs.slice(0, 8).map((job) => {
          const elapsedMs = Math.max(0, Date.now() - new Date(job.updated_at).getTime());
          return (
            <article className="live-job" key={job.id}>
              <div>
                <strong>{t('review.document', { id: job.paperless_document_id })}</strong>
                <span>
                  {stageLabel(job.stage, t)} · {t('dashboard.live.attempt', { attempts: job.attempts, max: job.max_attempts })}
                  {' · '}
                  {t('dashboard.live.elapsed', { time: formatMs(elapsedMs) })}
                </span>
              </div>
              <Status value={job.status} />
              <small>
                {job.lease_owner ? t('dashboard.live.worker', { worker: job.lease_owner }) : formatRelative(job.updated_at)}
                {' · '}
                {t('dashboard.live.trace', { trace: shortId(job.trace_id) })}
              </small>
            </article>
          );
        })}
      </section>

      <section className="live-debug-section">
        <h3>{t('dashboard.live.latest_llm')}</h3>
        {recentEvents.length === 0 && <p className="empty-state compact">{t('dashboard.live.no_llm')}</p>}
        {recentEvents.slice(0, 5).map((event) => (
          <article className="live-event" key={event.id}>
            <strong>{event.provider} / {event.model}</strong>
            <span>{stageLabel(event.stage, t)} · {formatMs(event.duration_ms ?? 0)} · {formatRelative(event.created_at)}</span>
          </article>
        ))}
      </section>

      <section className="live-debug-section">
        <h3>{t('dashboard.live.recent_failures')}</h3>
        {recentFailures.length === 0 && <p className="empty-state compact">{t('dashboard.live.no_failures')}</p>}
        {recentFailures.slice(0, 5).map((failure) => {
          const kind = liveFailureKind(failure);
          return (
            <article className={`live-failure ${kind !== 'failed' ? 'retry' : ''}`} key={failure.id}>
              <div className="failure-heading">
                <strong>{t('dashboard.live.document_stage', { document: failure.paperless_document_id, stage: stageLabel(failure.stage, t) })}</strong>
                <Status value={kind} />
              </div>
              <span>{failure.error_message}</span>
              <small>{liveFailureTiming(failure, kind, t, formatRelative)}</small>
            </article>
          );
        })}
      </section>
    </aside>
  );
}

export function Dashboard({ setError, canManageSettings }: { setError: (error: string | null) => void; canManageSettings: boolean }) {
  const { t, formatNumber, formatPercent, formatRelativeTime: formatRelative } = useI18n();
  const [range, setRange] = useState<DashboardRange>(() => {
    if (typeof window === 'undefined') return '24h';
    const stored = window.localStorage.getItem('dashboard.range');
    const valid: DashboardRange[] = ['24h', '7d', '30d', '90d', '12m', 'all'];
    if (stored && (valid as string[]).includes(stored)) return stored as DashboardRange;
    return '24h';
  });
  useEffect(() => {
    if (typeof window === 'undefined') return;
    window.localStorage.setItem('dashboard.range', range);
  }, [range]);
  const [busy, setBusy] = useState(false);
  const [modeBusy, setModeBusy] = useState(false);
  const [recoveryBusy, setRecoveryBusy] = useState(false);
  const [paperlessToolsBusy, setPaperlessToolsBusy] = useState(false);
  const [consistency, setConsistency] = useState<PaperlessConsistencyResult | null>(null);
  const [reconcile, setReconcile] = useState<CompletionTagReconcileResult | null>(null);
  const [drawerOpen, setDrawerOpen] = useState(() => {
    if (typeof window === 'undefined') return false;
    return window.localStorage.getItem('dashboard.drawer_open') === 'true';
  });
  useEffect(() => {
    if (typeof window === 'undefined') return;
    window.localStorage.setItem('dashboard.drawer_open', String(drawerOpen));
  }, [drawerOpen]);

  const { stats, counts, lastLoadedAt, reload: load } = useDashboardStats(range, setError);
  const { live, recovery, reload: loadLive, reloadRecovery: loadRecovery, setLive } = useDashboardLive(canManageSettings, setError);
  const { nextRefreshIn, pulse } = useFreshness(30_000, lastLoadedAt);

  const updateDashboardWorkflowMode = async (nextMode: ProcessingMode) => {
    const settings = await api.updateWorkflowMode(nextMode);
    setLive((current) =>
      current
        ? {
            ...current,
            workflow_mode: settings.workflow.mode,
            autopilot_enabled: settings.workflow.mode !== 'manual_review' && !settings.workflow.paused,
            workflow_safety: {
              ...current.workflow_safety,
              paused: settings.workflow.paused,
              dry_run: settings.workflow.dry_run,
              hourly_document_limit: settings.workflow.hourly_document_limit,
              daily_document_limit: settings.workflow.daily_document_limit
            }
          }
        : current
    );
    await loadLive();
  };
  const updateDashboardPause = async (paused: boolean) => {
    const settings = await api.updateWorkflowControls({ paused });
    setLive((current) =>
      current
        ? {
            ...current,
            autopilot_enabled: settings.workflow.mode !== 'manual_review' && !settings.workflow.paused,
            workflow_safety: {
              ...current.workflow_safety,
              paused: settings.workflow.paused,
              dry_run: settings.workflow.dry_run,
              hourly_document_limit: settings.workflow.hourly_document_limit,
              daily_document_limit: settings.workflow.daily_document_limit
            }
          }
        : current
    );
    await loadLive();
  };
  const recoverStaleLeases = async () => {
    await api.recoverStaleLeases(recovery?.older_than_seconds ?? 600);
    await Promise.all([loadLive(), loadRecovery()]);
  };
  const recoverStuckRuns = async () => {
    await api.recoverStuckRuns(recovery?.older_than_seconds ?? 600);
    await Promise.all([load(), loadLive(), loadRecovery()]);
  };
  const checkPaperlessConsistency = async () => {
    const result = await api.paperlessConsistency();
    setConsistency(result);
  };
  const reconcileCompletionTags = async (dryRun: boolean) => {
    const result = await api.reconcileCompletionTags({ dry_run: dryRun });
    setReconcile(result);
    await Promise.all([load(), loadLive()]);
  };

  const openBacklog = counts.total_documents - counts.complete;
  const jobStatusData = statusChartData(stats?.job_status.length ? stats.job_status : defaultJobStatus, t);
  const comparison = stats?.comparison;
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
  const runningJobs = stats?.kpis.running_jobs ?? counts.running;

  const healthScore = computeHealthScore(stats, live);
  const heroMetric = {
    label: t('dashboard.metric.open_backlog'),
    value: stats?.kpis.open_backlog ?? openBacklog,
    tone: 'warning' as const,
    delta: comparison?.open_backlog_delta
  };
  const secondaryMetrics = [
    { label: t('dashboard.metric.throughput'), value: stats?.kpis.throughput ?? 0, tone: 'success', delta: comparison?.jobs_succeeded_delta },
    { label: t('dashboard.metric.completion'), value: formatPercent(stats?.kpis.completion_rate ?? 0), tone: 'neutral', delta: null },
    { label: t('dashboard.metric.mttc'), value: formatMttc(stats?.kpis.mttc_seconds), tone: 'neutral', delta: null },
    { label: t('dashboard.metric.cost', { range }), value: formatCost(stats?.kpis.cost_in_range_usd), tone: 'neutral', delta: null }
  ];
  const tertiaryMetrics = [
    { label: t('dashboard.metric.running_now'), value: runningJobs, tone: 'info', delta: null },
    { label: t('dashboard.metric.review_queue'), value: counts.waiting_review, tone: 'review', delta: null },
    { label: t('dashboard.metric.failed'), value: counts.failed, tone: 'danger', delta: comparison?.jobs_failed_delta },
    { label: t('dashboard.metric.p95_latency'), value: formatMs(stats?.kpis.p95_stage_duration_ms ?? 0), tone: 'neutral', delta: null }
  ];

  const onAlertAction = (item: NeedsAttentionItem) => {
    if (!canManageSettings) return;
    switch (item.kind) {
      case 'stuck_runs':
        void run(setRecoveryBusy, setError, recoverStuckRuns, t);
        break;
      case 'stale_leases':
        void run(setRecoveryBusy, setError, recoverStaleLeases, t);
        break;
      default:
        break;
    }
  };

  // formatPercent imported as formatPercentStandalone is unused here; the I18n hook supplies the runtime variant.
  void formatPercentStandalone;

  return (
    <section className="page dashboard-page">
      <div className="dashboard-heading">
        <div className="dashboard-heading-main">
          <PageHeader title={t('dashboard.title')} />
          <p>
            {t('dashboard.last_refresh', { time: lastLoadedAt ? formatRelative(lastLoadedAt) : '-' })}
          </p>
          <HealthBadge
            score={healthScore}
            generatedAt={stats?.generated_at ?? null}
            nextRefreshIn={nextRefreshIn}
            pulse={pulse}
          />
        </div>
        <div className="dashboard-heading-actions">
          <div className="range-tabs" aria-label={t('dashboard.range_label')}>
            {(stats?.available_ranges ?? defaultDashboardRanges).map((option) => (
              <button
                key={option.key}
                className={range === option.key ? 'active' : ''}
                onClick={() => setRange(option.key)}
              >
                {option.label}
              </button>
            ))}
          </div>
          <button
            className="primary-button"
            disabled={busy}
            onClick={() => void run(setBusy, setError, async () => Promise.all([load(), loadLive()]))}
          >
            <RefreshCw size={16} /> {busy ? t('generic.refreshing') : t('generic.refresh')}
          </button>
          {canManageSettings && (
            <button
              type="button"
              className="secondary-button drawer-toggle"
              onClick={() => setDrawerOpen((open) => !open)}
              aria-expanded={drawerOpen}
            >
              <Settings size={16} /> {t('dashboard.maintenance.toggle')}
            </button>
          )}
        </div>
      </div>

      <AlertsBar items={live?.needs_attention ?? []} onAction={onAlertAction} />

      <div className="operations-strip">
        <AutoProcessingCard
          enabled={live?.autopilot_enabled ?? false}
          mode={live?.workflow_mode ?? 'manual_review'}
          safety={live?.workflow_safety}
          nextSelectorScanAt={live?.next_selector_scan_at}
          busy={modeBusy}
          canToggle={canManageSettings}
          onModeChange={(mode) => void run(setModeBusy, setError, () => updateDashboardWorkflowMode(mode), t)}
          onPauseChange={(paused) => void run(setModeBusy, setError, () => updateDashboardPause(paused), t)}
        />
        <ServiceStatusCard label={t('dashboard.live.selector')} icon={<ListChecks size={18} />} status={live?.selector} />
        <ServiceStatusCard label="LLM" icon={<Activity size={18} />} status={live?.llm} />
        <ServiceStatusCard label="Paperless" icon={<Database size={18} />} status={live?.paperless} />
      </div>

      {canManageSettings && (
        <MaintenanceDrawer
          open={drawerOpen}
          onClose={() => setDrawerOpen(false)}
          recovery={recovery}
          consistency={consistency}
          reconcile={reconcile}
          recoveryBusy={recoveryBusy}
          maintenanceBusy={paperlessToolsBusy}
          onRefreshRecovery={() => void run(setRecoveryBusy, setError, loadRecovery, t)}
          onRecoverLeases={() => void run(setRecoveryBusy, setError, recoverStaleLeases, t)}
          onRecoverRuns={() => void run(setRecoveryBusy, setError, recoverStuckRuns, t)}
          onCheckConsistency={() => void run(setPaperlessToolsBusy, setError, checkPaperlessConsistency, t)}
          onDryRunReconcile={() => void run(setPaperlessToolsBusy, setError, () => reconcileCompletionTags(true), t)}
          onApplyReconcile={() => void run(setPaperlessToolsBusy, setError, () => reconcileCompletionTags(false), t)}
          queueBusy={busy}
          onQueueSync={() => void run(setBusy, setError, api.syncPaperless, t).then(load)}
          onQueueOcr={() => void run(setBusy, setError, api.queueOcr, t).then(load)}
          onQueueTags={() => void run(setBusy, setError, api.queueTags, t).then(load)}
          onQueueFull={() => void run(setBusy, setError, api.queueFull, t).then(load)}
        />
      )}

      <div className="kpi-grid">
        <div className={`metric hero ${heroMetric.tone}`}>
          <span>{heroMetric.label}</span>
          <strong>{typeof heroMetric.value === 'number' ? formatNumber(heroMetric.value) : heroMetric.value}</strong>
          {typeof heroMetric.delta === 'number' && (
            <em className={deltaTone(heroMetric.delta)}>{formatDelta(heroMetric.delta, t, formatNumber)}</em>
          )}
        </div>
        <div className="kpi-secondary">
          {secondaryMetrics.map(({ label, value, tone, delta }) => (
            <div className={`metric ${tone}`} key={label}>
              <span>{label}</span>
              <strong>{typeof value === 'number' ? formatNumber(value) : value}</strong>
              {typeof delta === 'number' && <em className={deltaTone(delta)}>{formatDelta(delta, t, formatNumber)}</em>}
            </div>
          ))}
        </div>
        <div className="kpi-tertiary">
          {tertiaryMetrics.map(({ label, value, tone, delta }) => (
            <div className={`metric ${tone}`} key={label}>
              <span>{label}</span>
              <strong>{typeof value === 'number' ? formatNumber(value) : value}</strong>
              {typeof delta === 'number' && <em className={deltaTone(delta)}>{formatDelta(delta, t, formatNumber)}</em>}
            </div>
          ))}
        </div>
      </div>

      <div className="dashboard-ops-grid">
        <div className="dashboard-analytics">
          <ChartPanel title={t('dashboard.chart.throughput', { range })} wide>
            <ResponsiveContainer width="100%" height={280}>
              <ComposedChart data={throughputWithRate}>
                <CartesianGrid strokeDasharray="3 3" vertical={false} />
                <XAxis dataKey="label" />
                <YAxis yAxisId="count" allowDecimals={false} />
                <YAxis yAxisId="rate" orientation="right" domain={[0, 100]} unit="%" />
                <Tooltip />
                <Legend />
                <Area yAxisId="count" type="monotone" dataKey="jobs_created" name={t('dashboard.chart.created')} stroke="#28649b" fill="#dbe9f5" />
                <Area yAxisId="count" type="monotone" dataKey="jobs_succeeded" name={t('dashboard.chart.succeeded')} stroke="#147f7a" fill="#d9eeee" />
                <Area yAxisId="count" type="monotone" dataKey="jobs_failed" name={t('dashboard.chart.failed')} stroke="#a6403a" fill="#f5dddd" />
                <Line yAxisId="rate" type="monotone" dataKey="success_rate" name={t('dashboard.chart.success_rate')} stroke="#0f5f5b" strokeWidth={2} dot={false} />
              </ComposedChart>
            </ResponsiveContainer>
          </ChartPanel>
          <StageMatrix stats={stats} onStageSelect={() => undefined} />
          <div className="dashboard-grid compact">
            <ChartPanel title={t('dashboard.chart.backlog_trend')}>
              <ResponsiveContainer width="100%" height={240}>
                <ComposedChart data={backlogWithRate}>
                  <CartesianGrid strokeDasharray="3 3" vertical={false} />
                  <XAxis dataKey="label" />
                  <YAxis yAxisId="count" allowDecimals={false} />
                  <YAxis yAxisId="rate" orientation="right" domain={[0, 100]} unit="%" />
                  <Tooltip />
                  <Legend />
                  <Area yAxisId="count" type="monotone" dataKey="open_backlog" name={t('dashboard.chart.open')} stroke="#a9782b" fill="#f1e5d0" />
                  <Line yAxisId="rate" type="monotone" dataKey="completion_rate" name={t('dashboard.chart.completion_rate')} stroke="#147f7a" strokeWidth={2} dot={false} />
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
                  <Bar dataKey="count" fill="#28649b" radius={[0, 4, 4, 0]} />
                </BarChart>
              </ResponsiveContainer>
            </ChartPanel>
          </div>
        </div>
        <LiveProcessingPanel live={live} />
      </div>

      <ActivityTimeline live={live} />

      <div className="quality-strip">
        <div>
          <span>{t('dashboard.quality.review_decisions')}</span>
          <strong>{formatNumber(stats?.quality.review_decisions ?? 0)}</strong>
        </div>
        <div>
          <span>{t('dashboard.quality.acceptance')}</span>
          <strong>{stats?.quality.acceptance_rate == null ? '-' : formatPercent(stats.quality.acceptance_rate)}</strong>
        </div>
        <div>
          <span>{t('dashboard.quality.edited')}</span>
          <strong>{formatNumber(stats?.quality.review_edited ?? 0)}</strong>
        </div>
        <div>
          <span>{t('dashboard.quality.rejected')}</span>
          <strong>{formatNumber(stats?.quality.review_rejected ?? 0)}</strong>
        </div>
        <div>
          <span>{t('dashboard.quality.uncertainty')}</span>
          <strong>{formatNumber(stats?.quality.uncertainty_reviews ?? 0)}</strong>
        </div>
      </div>

      <CostPanel stats={stats} range={range} />

      <ProviderTable usage={stats?.provider_usage ?? []} />
    </section>
  );
}
