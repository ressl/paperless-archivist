import { useCallback, useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { AlertTriangle, Info } from 'lucide-react';
import { useI18n } from '../i18n/I18nProvider';
import { useFocusTrap } from './ui';

export type ConfirmTone = 'danger' | 'default';

export type ConfirmOptions = {
  /** Dialog heading; also the accessible name. */
  title: string;
  /** One-sentence consequence; also the accessible description. */
  description: string;
  /** Label of the confirming (right-hand) button. */
  confirmLabel: string;
  /** Label of the cancelling button (defaults to the generic "Cancel"). */
  cancelLabel?: string;
  /** `danger` renders the confirm button in the destructive style. */
  tone?: ConfirmTone;
  /**
   * Optional scope/count details shown below the description (e.g. "12 review
   * items" or a list of affected entities). Kept out of `aria-describedby` so
   * the announced description stays a single sentence.
   */
  details?: ReactNode;
};

/**
 * Shared accessible confirmation dialog (#417). Generalised from the prompt
 * draft guard: `role="alertdialog"` + `aria-modal`, labelled and described,
 * initial focus on the safe (cancel) action, Tab trapped inside, Escape and a
 * backdrop click cancel, and focus returns to the invoking control on close.
 * Rendered into `document.body` so a dialog opened from inside a drawer is not
 * clipped by, or trapped inside, the drawer.
 */
export function ConfirmDialog({
  title,
  description,
  confirmLabel,
  cancelLabel,
  tone = 'danger',
  details,
  onConfirm,
  onCancel
}: ConfirmOptions & { onConfirm: () => void; onCancel: () => void }) {
  const { t } = useI18n();
  const dialogRef = useRef<HTMLElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const titleId = useId();
  const descriptionId = useId();
  useFocusTrap(true, dialogRef);

  useEffect(() => {
    cancelRef.current?.focus();
    // Capture phase + stopImmediatePropagation: Escape must only cancel this
    // dialog, not also close a drawer that listens for Escape on `window`.
    const cancelOnEscape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      event.stopImmediatePropagation();
      event.preventDefault();
      onCancel();
    };
    window.addEventListener('keydown', cancelOnEscape, true);
    return () => window.removeEventListener('keydown', cancelOnEscape, true);
  }, [onCancel]);

  const dialog = (
    <div className="confirm-dialog-root">
      <div className="confirm-dialog-backdrop" aria-hidden="true" onClick={onCancel} />
      <section
        className={`confirm-dialog confirm-dialog--${tone}`}
        ref={dialogRef}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={descriptionId}
        tabIndex={-1}
      >
        <header>
          {tone === 'danger' ? <AlertTriangle size={20} aria-hidden="true" /> : <Info size={20} aria-hidden="true" />}
          <h3 id={titleId}>{title}</h3>
        </header>
        <p id={descriptionId}>{description}</p>
        {details && <div className="confirm-dialog-details">{details}</div>}
        <div className="confirm-dialog-actions">
          <button ref={cancelRef} className="secondary-button" type="button" onClick={onCancel}>
            {cancelLabel ?? t('generic.cancel')}
          </button>
          <button
            className={tone === 'danger' ? 'primary-button danger-button' : 'primary-button'}
            type="button"
            onClick={onConfirm}
          >
            {confirmLabel}
          </button>
        </div>
      </section>
    </div>
  );
  return typeof document === 'undefined' ? dialog : createPortal(dialog, document.body);
}

type PendingConfirm = ConfirmOptions & { resolve: (confirmed: boolean) => void };

/**
 * Promise-based confirmation for async action handlers:
 *
 *   const { confirm, dialog } = useConfirm();
 *   if (!(await confirm({ title, description, confirmLabel }))) return;
 *   ...
 *   return <>{...}{dialog}</>;
 *
 * Replaces `window.confirm` (#417). An unmounted host resolves a pending
 * confirmation with `false`, so an action never proceeds without an answer.
 */
export function useConfirm() {
  const [pending, setPending] = useState<PendingConfirm | null>(null);
  const pendingRef = useRef<PendingConfirm | null>(null);

  const settle = useCallback((confirmed: boolean) => {
    const current = pendingRef.current;
    pendingRef.current = null;
    setPending(null);
    current?.resolve(confirmed);
  }, []);

  const confirm = useCallback(
    (options: ConfirmOptions) =>
      new Promise<boolean>((resolve) => {
        // A second request while one is open cancels the first.
        pendingRef.current?.resolve(false);
        const next = { ...options, resolve };
        pendingRef.current = next;
        setPending(next);
      }),
    []
  );

  useEffect(() => () => pendingRef.current?.resolve(false), []);

  const onConfirm = useCallback(() => settle(true), [settle]);
  const onCancel = useCallback(() => settle(false), [settle]);

  const dialog = pending ? (
    <ConfirmDialog
      title={pending.title}
      description={pending.description}
      confirmLabel={pending.confirmLabel}
      cancelLabel={pending.cancelLabel}
      tone={pending.tone}
      details={pending.details}
      onConfirm={onConfirm}
      onCancel={onCancel}
    />
  ) : null;

  return { confirm, dialog };
}
