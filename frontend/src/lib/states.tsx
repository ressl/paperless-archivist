import type { ReactNode } from 'react';
import { AlertTriangle, Inbox, Loader2, RotateCcw } from 'lucide-react';
import { useI18n } from '../i18n/I18nProvider';

// Shared loading / empty / error placeholders (#429). Each state is visually
// distinct (icon + tone) and distinct for assistive tech: loading is a polite
// busy status, empty is a plain status, and a failed load is an alert with a
// retry action, so "could not load" is never mistaken for "nothing here".

export function LoadingState({ label, compact = false }: { label?: string; compact?: boolean }) {
  const { t } = useI18n();
  return (
    <div className={`state-block state-block--loading${compact ? ' compact' : ''}`} role="status" aria-live="polite" aria-busy="true">
      <Loader2 size={18} aria-hidden="true" className="state-block-spinner" />
      <span>{label ?? t('generic.loading')}</span>
    </div>
  );
}

export function EmptyState({ message, compact = false, children }: { message: string; compact?: boolean; children?: ReactNode }) {
  return (
    <div className={`state-block state-block--empty${compact ? ' compact' : ''}`} role="status">
      <Inbox size={18} aria-hidden="true" />
      <span>{message}</span>
      {children}
    </div>
  );
}

export function ErrorState({
  title,
  detail,
  onRetry,
  compact = false
}: {
  title: string;
  detail?: string | null;
  onRetry?: () => void;
  compact?: boolean;
}) {
  const { t } = useI18n();
  return (
    <div className={`state-block state-block--error${compact ? ' compact' : ''}`} role="alert">
      <AlertTriangle size={18} aria-hidden="true" />
      <span>
        <strong>{title}</strong>
        {detail && <small>{detail}</small>}
      </span>
      {onRetry && (
        <button type="button" className="secondary-button" onClick={onRetry}>
          <RotateCcw size={14} aria-hidden="true" /> {t('generic.retry')}
        </button>
      )}
    </div>
  );
}
