import {
  useEffect,
  useMemo,
  useRef,
  type ButtonHTMLAttributes,
  type ReactNode,
  type RefObject
} from 'react';
import { AlertTriangle, Check, CircleDashed, Eye, Info, X } from 'lucide-react';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { deltaTone, statusLabel } from './format';

// --- Shared form / surface primitives (UI redesign) -------------------------
// One set of building blocks so forms, sections, buttons and metric tiles look
// and size the same on every page instead of each feature re-rolling markup.

type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'link';

/** Button with a design-system variant mapped to the shared button classes. */
export function Button({
  variant = 'primary',
  icon,
  children,
  className,
  ...rest
}: { variant?: ButtonVariant; icon?: ReactNode } & ButtonHTMLAttributes<HTMLButtonElement>) {
  const base = `${variant}-button`;
  return (
    <button className={className ? `${base} ${className}` : base} {...rest}>
      {icon}
      {children}
    </button>
  );
}

/** A titled card section (fieldset semantics) used to group related controls. */
export function Section({ title, children, className }: { title: string; children: ReactNode; className?: string }) {
  return (
    <fieldset className={className ? `card ${className}` : 'card'}>
      <legend>{title}</legend>
      {children}
    </fieldset>
  );
}

/** A labelled form control with optional help text. Wrap an input/select/etc. */
export function FormField({ label, help, htmlFor, children }: { label: string; help?: string; htmlFor?: string; children: ReactNode }) {
  return (
    <label className="form-field" htmlFor={htmlFor}>
      <span className="form-field-label">{label}</span>
      {children}
      {help && <span className="form-field-help">{help}</span>}
    </label>
  );
}

/** A single KPI / metric tile with an optional good/bad-aware delta. */
export function KpiCard({
  label,
  value,
  delta,
  higherIsBetter = true,
  tone = 'neutral'
}: {
  label: string;
  value: ReactNode;
  delta?: { value: number; formatted: string } | null;
  higherIsBetter?: boolean;
  tone?: 'success' | 'warning' | 'danger' | 'neutral';
}) {
  return (
    <div className={`card metric metric--${tone}`}>
      <span className="metric-label">{label}</span>
      <strong>{value}</strong>
      {delta && <em className={deltaTone(delta.value, higherIsBetter)}>{delta.formatted}</em>}
    </div>
  );
}

export function PageHeader({ title }: { title: string }) {
  return <header className="page-header"><h2>{title}</h2></header>;
}

export type BannerTone = 'error' | 'success' | 'info';

/**
 * Inline status banner. `tone` drives the colour so positive outcomes render in
 * a green/neutral banner instead of the red error box (see #228). Errors keep
 * the assertive live-region; success/info use a polite one.
 */
export function Banner({ tone, message, onDismiss }: { tone: BannerTone; message: string; onDismiss: () => void }) {
  const { t } = useI18n();
  return (
    <div className={`banner ${tone}`} role={tone === 'error' ? 'alert' : 'status'} aria-live={tone === 'error' ? 'assertive' : 'polite'}>
      <span>{message}</span>
      <button title={t('generic.dismiss')} onClick={onDismiss}>
        <X size={16} />
      </button>
    </div>
  );
}

/**
 * Focus management for an overlay (drawer / modal): on open, move focus into
 * the container and trap Tab / Shift+Tab inside it; on close, restore focus to
 * the element that was focused before opening (see #242). Pass the same `active`
 * flag that controls the overlay's visibility.
 */
export function useFocusTrap(active: boolean, containerRef: RefObject<HTMLElement | null>) {
  const previouslyFocused = useRef<HTMLElement | null>(null);
  useEffect(() => {
    if (!active) return;
    const container = containerRef.current;
    if (!container) return;
    previouslyFocused.current = document.activeElement as HTMLElement | null;

    const focusable = () =>
      Array.from(
        container.querySelectorAll<HTMLElement>(
          'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])'
        )
      ).filter((el) => el.offsetParent !== null);

    const first = focusable()[0] ?? container;
    first.focus();

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Tab') return;
      const items = focusable();
      if (items.length === 0) {
        event.preventDefault();
        return;
      }
      const firstEl = items[0];
      const lastEl = items[items.length - 1];
      if (event.shiftKey && document.activeElement === firstEl) {
        event.preventDefault();
        lastEl.focus();
      } else if (!event.shiftKey && document.activeElement === lastEl) {
        event.preventDefault();
        firstEl.focus();
      }
    };

    container.addEventListener('keydown', onKeyDown);
    return () => {
      container.removeEventListener('keydown', onKeyDown);
      previouslyFocused.current?.focus?.();
    };
  }, [active, containerRef]);
}

type StatusTone = 'success' | 'danger' | 'info' | 'review' | 'neutral';

const STATUS_ICONS: Record<StatusTone, ReactNode> = {
  success: <Check size={12} aria-hidden="true" />,
  danger: <AlertTriangle size={12} aria-hidden="true" />,
  info: <Info size={12} aria-hidden="true" />,
  review: <Eye size={12} aria-hidden="true" />,
  neutral: <CircleDashed size={12} aria-hidden="true" />
};

export function Status({ value }: { value: string }) {
  const { t } = useI18n();
  const tone = useMemo<StatusTone>(() => {
    if (['succeeded', 'success', 'complete'].includes(value)) return 'success';
    if (['failed', 'error'].includes(value)) return 'danger';
    if (['running', 'queued', 'applying', 'retry_scheduled', 'retry_ready'].includes(value)) return 'info';
    if (['waiting_review', 'review'].includes(value)) return 'review';
    return 'neutral';
  }, [value]);
  const label = statusLabel(value, t);
  return (
    <span className={`status ${tone}`} role="status" aria-label={label}>
      {STATUS_ICONS[tone]}
      <span className="status-label">{label}</span>
    </span>
  );
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
