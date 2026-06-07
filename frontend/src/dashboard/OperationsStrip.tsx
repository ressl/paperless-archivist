import type { ReactNode } from 'react';
import { Activity, AlertTriangle, Database, ListChecks, Play, Power } from 'lucide-react';
import { DashboardLiveStatus, ProcessingMode } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { Status } from '../lib/ui';
import { workflowModeDescription, workflowModeLabel, workflowModeOptions } from '../lib/workflow';

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

export function OperationsStrip({
  live,
  modeBusy,
  canManageSettings,
  onModeChange,
  onPauseChange
}: {
  live: DashboardLiveStatus | null;
  modeBusy: boolean;
  canManageSettings: boolean;
  onModeChange: (mode: ProcessingMode) => void;
  onPauseChange: (paused: boolean) => void;
}) {
  const { t } = useI18n();
  return (
    <div className="operations-strip">
      <AutoProcessingCard
        enabled={live?.autopilot_enabled ?? false}
        mode={live?.workflow_mode ?? 'manual_review'}
        safety={live?.workflow_safety}
        nextSelectorScanAt={live?.next_selector_scan_at}
        busy={modeBusy}
        canToggle={canManageSettings}
        onModeChange={onModeChange}
        onPauseChange={onPauseChange}
      />
      <ServiceStatusCard label={t('dashboard.live.selector')} icon={<ListChecks size={18} />} status={live?.selector} />
      <ServiceStatusCard label="LLM" icon={<Activity size={18} />} status={live?.llm} />
      <ServiceStatusCard label="Paperless" icon={<Database size={18} />} status={live?.paperless} />
    </div>
  );
}
