import { useEffect, useMemo, useState } from 'react';
import { useDashboardLive, useDashboardStats, useMediaQuery } from './hooks';
import { RefreshCw, Settings } from 'lucide-react';
import {
  api,
  CompletionTagReconcileResult,
  DashboardRange,
  NeedsAttentionItem,
  PaperlessConsistencyResult,
  ProcessingMode
} from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { Button, PageHeader, run } from '../lib/ui';
import { ErrorBoundary } from '../lib/ErrorBoundary';
import { defaultDashboardRanges, computeHealthScore } from './helpers';
import { AlertsBar } from './AlertsBar';
import { HealthBadge } from './HealthBadge';
import { OperationsStrip } from './OperationsStrip';
import { MaintenanceDrawer } from './MaintenanceDrawer';
import { KpiRow } from './KpiRow';
import { TrendCharts } from './TrendCharts';
import { LiveProcessingPanel } from './LiveProcessingPanel';
import { ActivityTimeline } from './ActivityTimeline';
import { CostPanel } from './CostPanel';
import { ProviderTable } from './ProviderTable';

// Re-exported from the pure helpers module so the public test surface
// (Dashboard.helpers.test.ts) and any external importer keep `computeHealthScore`
// available from this entry point.
export { computeHealthScore } from './helpers';

export function Dashboard({
  setError,
  setSuccess,
  canManageSettings,
  permissions,
  onNavigate
}: {
  setError: (error: string | null) => void;
  setSuccess: (message: string | null) => void;
  canManageSettings: boolean;
  permissions: import('../api/client').Permissions;
  /**
   * Optional callback for cross-tab navigation. Used by the
   * "Fehler untersuchen" alert action so it can switch to the
   * Inventory tab with `has_error=true` pre-applied in the URL.
   * When omitted, the alert action is a no-op for that kind.
   */
  onNavigate?: (tab: string, queryString?: string) => void;
}) {
  // Recovery visibility/actions are gated on permissions, not the admin role,
  // so that any caller the server lets read/write runs gets the matching UI.
  // See issue #98.
  const canReadRuns = permissions.read_runs;
  const canWriteRuns = permissions.write_runs;
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
  const { live, recovery, reload: loadLive, reloadRecovery: loadRecovery, setLive } = useDashboardLive(canReadRuns, setError);
  const compactLayout = useMediaQuery('(max-width: 1100px)');
  const [activeTab, setActiveTab] = useState<'analytics' | 'live' | 'activity'>('analytics');

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
  const unblockBlockedJobs = async () => {
    const summary = await api.unblockJobs({ clear_provider_cooldowns: true });
    // Positive outcome: surface via the success banner, not the error banner
    // (issue #228).
    setSuccess(
      t('dashboard.alert.unblock_success', {
        predecessors: String(summary.predecessors_requeued),
        runs: String(summary.runs_unblocked),
        cooldowns: String(summary.cooldowns_cleared),
      })
    );
    await Promise.all([load(), loadLive()]);
  };
  const clearProviderCooldowns = async () => {
    const result = await api.clearProviderCooldown();
    setSuccess(
      t('dashboard.alert.cooldown_cleared', { count: String(result.cleared) })
    );
    await loadLive();
  };
  const releaseScheduledRetries = async () => {
    const result = await api.releaseScheduledRetries();
    setSuccess(
      t('dashboard.alert.scheduled_released', { count: String(result.released) })
    );
    await Promise.all([load(), loadLive()]);
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

  // Wire the stage-matrix stage names to cross-tab navigation. When no
  // `onNavigate` is provided the matrix renders plain labels instead of dead
  // links (issue #230). Map the stage to the inventory status filter the
  // Inventory page actually parses — `?stage=` was never read, so the link
  // opened an unfiltered list (#267). Pre-select the actionable statuses
  // (in-flight or stuck at that stage).
  const handleStageSelect = useMemo(
    () =>
      onNavigate
        ? (stage: string) => {
            const statusKey =
              stage === 'ocr' ? 'ocr_status' : stage === 'metadata' ? 'metadata_status' : null;
            const search = statusKey
              ? `?${statusKey}=queued,running,failed,waiting_review`
              : '';
            onNavigate('inventory', search);
          }
        : undefined,
    [onNavigate]
  );

  const healthScore = computeHealthScore(stats, live);

  const onAlertAction = (item: NeedsAttentionItem) => {
    // Navigation-only alerts: never require WriteRuns, just hop to the
    // relevant tab. Backend kinds come from needs_attention_items in
    // archivist-db (provider_error, dry_run_active, quota_low).
    if (item.kind === 'provider_error') {
      if (onNavigate) {
        onNavigate('inventory', '?has_error=true');
      } else {
        setError(t('dashboard.alert.navigate_unavailable'));
      }
      return;
    }
    if (item.kind === 'dry_run_active' || item.kind === 'quota_low') {
      if (onNavigate) {
        onNavigate('settings');
      } else {
        setError(t('dashboard.alert.navigate_unavailable'));
      }
      return;
    }
    // Recovery actions (stuck_runs, stale_leases) hit privileged write
    // endpoints — surface a clear error when the user lacks WriteRuns
    // instead of silently no-oping (v1.5.16 fix).
    if (!canWriteRuns) {
      setError(t('dashboard.alert.permission_denied'));
      return;
    }
    switch (item.kind) {
      case 'stuck_runs':
        void run(setRecoveryBusy, setError, recoverStuckRuns, t);
        break;
      case 'stale_leases':
        void run(setRecoveryBusy, setError, recoverStaleLeases, t);
        break;
      case 'blocked_jobs':
        void run(setRecoveryBusy, setError, unblockBlockedJobs, t);
        break;
      case 'provider_cooldown':
        void run(setRecoveryBusy, setError, clearProviderCooldowns, t);
        break;
      default:
        setError(t('dashboard.alert.unknown_kind', { kind: item.kind }));
        break;
    }
  };

  return (
    <section className="page dashboard-page">
      <div className="dashboard-heading">
        <div className="dashboard-heading-main">
          <PageHeader title={t('dashboard.title')} />
          <p aria-live="polite">
            {t('dashboard.last_refresh', { time: lastLoadedAt ? formatRelative(lastLoadedAt) : '-' })}
          </p>
          <HealthBadge
            score={healthScore}
            generatedAt={stats?.generated_at ?? null}
            lastLoadedAt={lastLoadedAt}
          />
        </div>
        <div className="dashboard-heading-actions">
          <div className="range-tabs" role="group" aria-label={t('dashboard.range_label')}>
            {(stats?.available_ranges ?? defaultDashboardRanges).map((option) => (
              <button
                key={option.key}
                type="button"
                className={range === option.key ? 'active' : ''}
                aria-pressed={range === option.key}
                onClick={() => setRange(option.key)}
              >
                {option.label}
              </button>
            ))}
          </div>
          <Button
            type="button"
            variant="primary"
            icon={<RefreshCw size={16} />}
            disabled={busy}
            onClick={() => void run(setBusy, setError, async () => Promise.all([load(), loadLive()]))}
          >
            {busy ? t('generic.refreshing') : t('generic.refresh')}
          </Button>
          {canManageSettings && (
            <Button
              type="button"
              variant="secondary"
              className="drawer-toggle"
              icon={<Settings size={16} />}
              onClick={() => setDrawerOpen((open) => !open)}
              aria-expanded={drawerOpen}
            >
              {t('dashboard.maintenance.toggle')}
            </Button>
          )}
        </div>
      </div>

      <AlertsBar items={live?.needs_attention ?? []} onAction={onAlertAction} />

      {/* Primary KPI hierarchy first (issue #237): hero + headline + demoted stats. */}
      <KpiRow stats={stats} counts={counts} range={range} />

      {/*
        Operations controls (autopilot + service status) live in OperationsStrip.
        The processing-mode segmented control keeps its group label there:
        aria-label={t('dashboard.auto.processing_mode')} (a11y contract).
      */}
      <OperationsStrip
        live={live}
        modeBusy={modeBusy}
        canManageSettings={canManageSettings}
        onModeChange={(mode) => void run(setModeBusy, setError, () => updateDashboardWorkflowMode(mode), t)}
        onPauseChange={(paused) => void run(setModeBusy, setError, () => updateDashboardPause(paused), t)}
      />

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
          onReleaseScheduled={() => void run(setRecoveryBusy, setError, releaseScheduledRetries, t)}
          onCheckConsistency={() => void run(setPaperlessToolsBusy, setError, checkPaperlessConsistency, t)}
          onDryRunReconcile={() => void run(setPaperlessToolsBusy, setError, () => reconcileCompletionTags(true), t)}
          onApplyReconcile={() => void run(setPaperlessToolsBusy, setError, () => reconcileCompletionTags(false), t)}
          queueBusy={busy}
          onQueueSync={() => void run(setBusy, setError, api.syncPaperless, t).then(load)}
          onQueueOcr={() => void run(setBusy, setError, api.queueOcr, t).then(load)}
          onQueueFull={() => void run(setBusy, setError, api.queueFull, t).then(load)}
          onRerunFailed={() => void run(setBusy, setError, api.rerunFailed, t).then(load)}
        />
      )}

      {compactLayout && (
        <div className="dashboard-tabs" role="tablist" aria-label={t('dashboard.title')}>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === 'analytics'}
            className={activeTab === 'analytics' ? 'active' : ''}
            onClick={() => setActiveTab('analytics')}
          >
            {t('dashboard.tab.analytics')}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === 'live'}
            className={activeTab === 'live' ? 'active' : ''}
            onClick={() => setActiveTab('live')}
          >
            {t('dashboard.tab.live')}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === 'activity'}
            className={activeTab === 'activity' ? 'active' : ''}
            onClick={() => setActiveTab('activity')}
          >
            {t('dashboard.tab.activity')}
          </button>
        </div>
      )}

      <ErrorBoundary>
      <div className={`dashboard-ops-grid${compactLayout ? ' is-compact' : ''}`}>
        <TrendCharts
          stats={stats}
          range={range}
          compactLayout={compactLayout}
          activeTab={activeTab}
          onStageSelect={handleStageSelect}
        />
        <div className={compactLayout && activeTab !== 'live' ? 'is-hidden' : ''} role={compactLayout ? 'tabpanel' : undefined}>
          <LiveProcessingPanel live={live} />
        </div>
      </div>
      </ErrorBoundary>

      <div className={compactLayout && activeTab !== 'activity' ? 'is-hidden' : ''} role={compactLayout ? 'tabpanel' : undefined}>
        <ActivityTimeline live={live} />
      </div>

      <div className="quality-strip">
        <div className="card card--compact">
          <span>{t('dashboard.quality.review_decisions')}</span>
          <strong>{formatNumber(stats?.quality.review_decisions ?? 0)}</strong>
        </div>
        <div className="card card--compact">
          <span>{t('dashboard.quality.acceptance')}</span>
          <strong>{stats?.quality.acceptance_rate == null ? '-' : formatPercent(stats.quality.acceptance_rate)}</strong>
        </div>
        <div className="card card--compact">
          <span>{t('dashboard.quality.edited')}</span>
          <strong>{formatNumber(stats?.quality.review_edited ?? 0)}</strong>
        </div>
        <div className="card card--compact">
          <span>{t('dashboard.quality.rejected')}</span>
          <strong>{formatNumber(stats?.quality.review_rejected ?? 0)}</strong>
        </div>
        <div className="card card--compact">
          <span>{t('dashboard.quality.uncertainty')}</span>
          <strong>{formatNumber(stats?.quality.uncertainty_reviews ?? 0)}</strong>
        </div>
      </div>

      <CostPanel stats={stats} range={range} />

      <ProviderTable usage={stats?.provider_usage ?? []} />
    </section>
  );
}
