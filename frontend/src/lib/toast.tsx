import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
  type ReactNode
} from 'react';
import { AlertTriangle, Check, Info, X } from 'lucide-react';
import { useI18n } from '../i18n/I18nProvider';

/**
 * Toast / notification stack (#450). Replaces the single global banner: every
 * message is queued instead of overwriting the previous one, at most
 * `MAX_VISIBLE_TOASTS` are shown at once (the rest wait and are counted), and
 * repeats of a visible message are folded into one toast with a counter.
 *
 * Accessibility: the region and both live containers are always mounted, so
 * screen readers announce toasts added later. Errors go to an assertive
 * `role="alert"` container and stay until dismissed; success/info/warning go
 * to a polite `role="status"` container and auto-dismiss after
 * `TOAST_TIMEOUT_MS`, paused while the pointer or focus is inside the stack.
 */

export type ToastTone = 'error' | 'warning' | 'success' | 'info';

export type ToastInput = {
  tone: ToastTone;
  message: string;
  /**
   * Groups toasts that one owner may clear together, e.g. the page-level
   * `setError(null)` clears only `page` errors.
   */
  scope?: string;
  /** Keep until dismissed. Errors are always sticky. */
  sticky?: boolean;
};

export type Toast = Required<Omit<ToastInput, 'scope'>> & { id: number; scope: string | null; count: number };

export const MAX_VISIBLE_TOASTS = 4;
export const TOAST_TIMEOUT_MS = 6000;
/** Oldest toasts beyond this are dropped so a runaway loop cannot grow the queue forever. */
const MAX_QUEUED_TOASTS = 50;

type Action =
  | { type: 'add'; toast: Toast }
  | { type: 'dismiss'; id: number }
  | { type: 'clear'; scope: string; tone?: ToastTone };

export function toastReducer(state: Toast[], action: Action): Toast[] {
  switch (action.type) {
    case 'add': {
      const { toast } = action;
      const duplicate = state.find(
        (item) => item.tone === toast.tone && item.message === toast.message && item.scope === toast.scope
      );
      if (duplicate) {
        return state.map((item) => (item === duplicate ? { ...item, count: item.count + 1 } : item));
      }
      const next = [...state, toast];
      return next.length > MAX_QUEUED_TOASTS ? next.slice(next.length - MAX_QUEUED_TOASTS) : next;
    }
    case 'dismiss':
      return state.filter((item) => item.id !== action.id);
    case 'clear':
      return state.filter((item) => item.scope !== action.scope || (action.tone !== undefined && item.tone !== action.tone));
  }
}

type ToastApi = {
  notify: (input: ToastInput) => void;
  dismiss: (id: number) => void;
  /** Dismiss every toast of `scope` (optionally only of one tone). */
  clear: (scope: string, tone?: ToastTone) => void;
};

const NOOP_API: ToastApi = { notify: () => {}, dismiss: () => {}, clear: () => {} };
const ToastContext = createContext<ToastApi | null>(null);

/** Toast actions; a no-op outside a provider so isolated component tests keep working. */
export function useToast(): ToastApi {
  return useContext(ToastContext) ?? NOOP_API;
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, dispatch] = useReducer(toastReducer, []);
  const nextId = useRef(1);
  const api = useMemo<ToastApi>(
    () => ({
      notify: ({ tone, message, scope, sticky }) => {
        if (!message) return;
        dispatch({
          type: 'add',
          toast: {
            id: nextId.current++,
            tone,
            message,
            scope: scope ?? null,
            sticky: tone === 'error' || Boolean(sticky),
            count: 1
          }
        });
      },
      dismiss: (id) => dispatch({ type: 'dismiss', id }),
      clear: (scope, tone) => dispatch({ type: 'clear', scope, tone })
    }),
    []
  );
  return (
    <ToastContext.Provider value={api}>
      {children}
      <ToastRegion toasts={toasts} onDismiss={api.dismiss} />
    </ToastContext.Provider>
  );
}

const ICONS: Record<ToastTone, ReactNode> = {
  error: <AlertTriangle size={16} aria-hidden="true" />,
  warning: <AlertTriangle size={16} aria-hidden="true" />,
  success: <Check size={16} aria-hidden="true" />,
  info: <Info size={16} aria-hidden="true" />
};

function ToastRegion({ toasts, onDismiss }: { toasts: Toast[]; onDismiss: (id: number) => void }) {
  const { t } = useI18n();
  const [paused, setPaused] = useState(false);
  const visible = toasts.slice(0, MAX_VISIBLE_TOASTS);
  const queued = toasts.length - visible.length;
  const renderToast = (toast: Toast) => (
    <ToastItem key={toast.id} toast={toast} paused={paused} onDismiss={onDismiss} dismissLabel={t('generic.dismiss')} />
  );
  return (
    <section
      className="toast-region"
      aria-label={t('toast.region_label')}
      onMouseEnter={() => setPaused(true)}
      onMouseLeave={() => setPaused(false)}
      onFocus={() => setPaused(true)}
      onBlur={() => setPaused(false)}
    >
      <div className="toast-stack" role="alert" aria-live="assertive" aria-relevant="additions text">
        {visible.filter((toast) => toast.tone === 'error').map(renderToast)}
      </div>
      <div className="toast-stack" role="status" aria-live="polite" aria-relevant="additions text">
        {visible.filter((toast) => toast.tone !== 'error').map(renderToast)}
      </div>
      {queued > 0 && <p className="toast-more">{t('toast.more', { count: queued })}</p>}
    </section>
  );
}

function ToastItem({
  toast,
  paused,
  onDismiss,
  dismissLabel
}: {
  toast: Toast;
  paused: boolean;
  onDismiss: (id: number) => void;
  dismissLabel: string;
}) {
  const { formatNumber } = useI18n();
  const dismiss = useCallback(() => onDismiss(toast.id), [onDismiss, toast.id]);
  // Restart the timer when a repeat is folded into this toast.
  useEffect(() => {
    if (toast.sticky || paused) return;
    const timer = window.setTimeout(dismiss, TOAST_TIMEOUT_MS);
    return () => window.clearTimeout(timer);
  }, [toast.sticky, toast.count, paused, dismiss]);
  return (
    <div className={`toast toast--${toast.tone}`}>
      {ICONS[toast.tone]}
      <span className="toast-message">{toast.message}</span>
      {toast.count > 1 && <span className="toast-count">×{formatNumber(toast.count)}</span>}
      <button type="button" className="toast-dismiss" title={dismissLabel} aria-label={dismissLabel} onClick={dismiss}>
        <X size={16} aria-hidden="true" />
      </button>
    </div>
  );
}
