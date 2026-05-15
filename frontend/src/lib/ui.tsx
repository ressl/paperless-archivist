import { useMemo, type ReactNode } from 'react';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { statusLabel } from './format';

export function PageHeader({ title }: { title: string }) {
  return <header className="page-header"><h2>{title}</h2></header>;
}

export function Status({ value }: { value: string }) {
  const { t } = useI18n();
  const tone = useMemo(() => {
    if (['succeeded', 'success', 'complete'].includes(value)) return 'success';
    if (['failed', 'error'].includes(value)) return 'danger';
    if (['running', 'queued', 'applying', 'retry_scheduled', 'retry_ready'].includes(value)) return 'info';
    if (['waiting_review', 'review'].includes(value)) return 'review';
    return 'neutral';
  }, [value]);
  return <span className={`status ${tone}`}>{statusLabel(value, t)}</span>;
}

export function ActionButton({ icon, label, busy, onClick }: { icon: ReactNode; label: string; busy: boolean; onClick: () => void | Promise<void> }) {
  return <button className="primary-button" title={label} disabled={busy} onClick={onClick}>{icon}{label}</button>;
}

export async function run(
  setBusy: (value: boolean) => void,
  setError: (value: string | null) => void,
  action: () => Promise<unknown> | unknown,
  t?: TFunction
) {
  setBusy(true);
  setError(null);
  try {
    await action();
  } catch (err) {
    setError(t ? localizedErrorMessage(err, t) : errorToString(err));
  } finally {
    setBusy(false);
  }
}

export function localizedErrorMessage(err: unknown, t: TFunction, fallback = t('generic.request_failed')) {
  const message = errorToString(err);
  const lower = message.toLowerCase();
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return `${t('generic.unauthorized')} ${message}`;
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return `${t('generic.timeout')} ${message}`;
  }
  if (lower.includes('failed to fetch') || lower.includes('network') || lower.includes('connect')) {
    return `${t('generic.network_error')} ${message}`;
  }
  if (!message || message === 'Request failed') return fallback;
  return message;
}

export function errorToString(err: unknown) {
  return err instanceof Error ? err.message : typeof err === 'string' ? err : 'Request failed';
}
