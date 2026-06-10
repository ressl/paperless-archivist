import { useEffect, useRef } from 'react';
import {
  AlertTriangle,
  Check,
  FileText,
  GitCompare,
  Play,
  RefreshCw,
  RotateCcw,
  Tags,
  X,
  Zap
} from 'lucide-react';
import {
  CompletionTagReconcileResult,
  PaperlessConsistencyResult,
  RecoveryCandidate
} from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { ActionButton, useFocusTrap } from '../lib/ui';

function RecoveryPanel({
  recovery,
  busy,
  onRefresh,
  onRecoverLeases,
  onRecoverRuns,
  onReleaseScheduled
}: {
  recovery: { older_than_seconds: number; items: RecoveryCandidate[] } | null;
  busy: boolean;
  onRefresh: () => void;
  onRecoverLeases: () => void;
  onRecoverRuns: () => void;
  onReleaseScheduled: () => void;
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
        <button type="button" disabled={busy} onClick={onReleaseScheduled}>
          <Zap size={16} /> {t('dashboard.recovery.release_scheduled')}
        </button>
      </div>
      <p className="field-hint">{t('dashboard.recovery.release_scheduled_hint')}</p>
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

export function MaintenanceDrawer({
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
  onReleaseScheduled,
  onCheckConsistency,
  onDryRunReconcile,
  onApplyReconcile,
  queueBusy,
  onQueueSync,
  onQueueOcr,
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
  onReleaseScheduled: () => void;
  onCheckConsistency: () => void;
  onDryRunReconcile: () => void;
  onApplyReconcile: () => void;
  queueBusy: boolean;
  onQueueSync: () => void;
  onQueueOcr: () => void;
  onQueueFull: () => void;
}) {
  const { t } = useI18n();
  const drawerRef = useRef<HTMLElement>(null);
  useFocusTrap(open, drawerRef);
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
      <aside className="drawer" ref={drawerRef}>
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
          onReleaseScheduled={onReleaseScheduled}
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
            <ActionButton icon={<Play />} label={t('dashboard.action.queue_full')} busy={queueBusy} onClick={onQueueFull} />
          </div>
        </section>
      </aside>
    </div>
  );
}
