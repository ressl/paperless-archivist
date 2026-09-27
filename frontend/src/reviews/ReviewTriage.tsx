import { useEffect, useId, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { ExternalLink, Keyboard, RefreshCw } from 'lucide-react';
import { api, type ReviewRetryOptions } from '../api/client';
import type { TFunction } from '../i18n/I18nProvider';
import { localizedErrorMessage, useFocusTrap } from '../lib/ui';
import { useResource } from '../lib/useResource';

/** Keys of the review keyboard triage (#445). */
export const REVIEW_SHORTCUTS = [
  { key: 'j', labelKey: 'review.shortcuts.next' },
  { key: 'k', labelKey: 'review.shortcuts.previous' },
  { key: 'a', labelKey: 'review.shortcuts.approve' },
  { key: 'r', labelKey: 'review.shortcuts.reject' },
  { key: 'e', labelKey: 'review.shortcuts.edit' },
  { key: '?', labelKey: 'review.shortcuts.help' }
] as const;

export type ReviewShortcutAction = 'next' | 'previous' | 'approve' | 'reject' | 'edit' | 'help';

/**
 * True when a key press belongs to the focused control (text entry, selects,
 * comboboxes, editable content) and must not trigger a shortcut (#445).
 */
export function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  if (target.closest('[role="combobox"], [role="listbox"]')) return true;
  const tag = target.tagName;
  if (tag === 'TEXTAREA' || tag === 'SELECT') return true;
  if (tag === 'INPUT') {
    const type = (target as HTMLInputElement).type;
    // Checkboxes/buttons don't take text; everything else (text, date, ...) does.
    return !['checkbox', 'radio', 'button', 'submit', 'reset'].includes(type);
  }
  return false;
}

/** Map a keydown to a triage action, or null when it must pass through. */
export function reviewShortcutFor(event: KeyboardEvent): ReviewShortcutAction | null {
  if (event.defaultPrevented || event.ctrlKey || event.metaKey || event.altKey) return null;
  if (isTypingTarget(event.target)) return null;
  // Any open modal (confirm, retry, help) owns the keyboard.
  if (typeof document !== 'undefined' && document.querySelector('[aria-modal="true"]')) return null;
  switch (event.key) {
    case 'j':
      return 'next';
    case 'k':
      return 'previous';
    case 'a':
      return 'approve';
    case 'r':
      return 'reject';
    case 'e':
      return 'edit';
    case '?':
      return 'help';
    default:
      return null;
  }
}

/** Page-level keydown listener for the review triage shortcuts (#445). */
export function useReviewShortcuts(enabled: boolean, onAction: (action: ReviewShortcutAction) => void) {
  const latest = useRef(onAction);
  latest.current = onAction;
  useEffect(() => {
    if (!enabled) return undefined;
    const onKeyDown = (event: KeyboardEvent) => {
      const action = reviewShortcutFor(event);
      if (!action) return;
      event.preventDefault();
      latest.current(action);
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [enabled]);
}

/** Shared modal frame: labelled dialog, focus trap, Escape closes. */
function ModalFrame({
  title,
  description,
  onClose,
  children
}: {
  title: string;
  description: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLElement>(null);
  const titleId = useId();
  const descriptionId = useId();
  useFocusTrap(true, ref);
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      event.stopImmediatePropagation();
      event.preventDefault();
      onClose();
    };
    window.addEventListener('keydown', onKeyDown, true);
    return () => window.removeEventListener('keydown', onKeyDown, true);
  }, [onClose]);
  const dialog = (
    <div className="confirm-dialog-root">
      <div className="confirm-dialog-backdrop" aria-hidden="true" onClick={onClose} />
      <section
        className="confirm-dialog confirm-dialog--default review-dialog"
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={descriptionId}
        tabIndex={-1}
      >
        <header>
          <h3 id={titleId}>{title}</h3>
        </header>
        <p id={descriptionId}>{description}</p>
        {children}
      </section>
    </div>
  );
  return typeof document === 'undefined' ? dialog : createPortal(dialog, document.body);
}

export function ShortcutHelpDialog({ onClose, t }: { onClose: () => void; t: TFunction }) {
  return (
    <ModalFrame title={t('review.shortcuts.title')} description={t('review.shortcuts.description')} onClose={onClose}>
      <dl className="review-shortcut-list">
        {REVIEW_SHORTCUTS.map((shortcut) => (
          <div key={shortcut.key}>
            <dt>
              <kbd>{shortcut.key}</kbd>
            </dt>
            <dd>{t(shortcut.labelKey)}</dd>
          </div>
        ))}
      </dl>
      <div className="confirm-dialog-actions">
        <button className="primary-button" type="button" onClick={onClose}>
          {t('review.shortcuts.close')}
        </button>
      </div>
    </ModalFrame>
  );
}

export function ShortcutHelpButton({ onClick, t }: { onClick: () => void; t: TFunction }) {
  return (
    <button type="button" onClick={onClick} aria-keyshortcuts="?">
      <Keyboard size={16} aria-hidden="true" /> {t('review.shortcuts.button')}
    </button>
  );
}

/**
 * Thumbnail of the review's document, served by Archivist's proxy (#445).
 * Clicking opens the full preview (PDF/image) in a new tab, also proxied.
 */
export function ReviewPreview({ reviewId, documentId, t }: { reviewId: string; documentId: number; t: TFunction }) {
  const [failed, setFailed] = useState(false);
  return (
    <a
      className="review-preview"
      href={api.reviewPreviewUrl(reviewId)}
      target="_blank"
      rel="noopener noreferrer"
      aria-label={t('review.preview.open')}
    >
      {failed ? (
        <span className="review-preview-fallback">{t('review.preview.unavailable')}</span>
      ) : (
        <img
          src={api.reviewThumbnailUrl(reviewId)}
          alt={t('review.preview.thumbnail_alt', { id: documentId })}
          loading="lazy"
          onError={() => setFailed(true)}
        />
      )}
      <span className="review-preview-link">
        <ExternalLink size={14} aria-hidden="true" /> {t('review.preview.open_short')}
      </span>
    </a>
  );
}

/**
 * "Retry with..." dialog (#445): pick provider, model and metadata prompt
 * version; submitting rejects the review and queues a new run.
 */
export function RetryReviewDialog({
  reviewId,
  onClose,
  onRetried,
  t
}: {
  reviewId: string;
  onClose: () => void;
  onRetried: (reviewId: string) => void | Promise<void>;
  t: TFunction;
}) {
  const providerId = useId();
  const modelId = useId();
  const promptId = useId();
  const [provider, setProvider] = useState('');
  const [model, setModel] = useState('');
  const [prompt, setPrompt] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const options = useResource<ReviewRetryOptions>((signal) => api.reviewRetryOptions({ signal }), [], {
    onError: (err) => setError(localizedErrorMessage(err, t))
  });
  const data = options.data;
  const effectiveProvider = provider || data?.default_provider || '';
  const defaultModel = data?.providers.find((entry) => entry.name === effectiveProvider)?.default_model ?? '';
  const activePrompt = data?.prompts.find((entry) => entry.active);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.retryReview(reviewId, {
        provider_name: provider || null,
        model: model.trim() || null,
        prompt_id: prompt || null
      });
      await onRetried(reviewId);
    } catch (err) {
      setError(localizedErrorMessage(err, t));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalFrame title={t('review.retry.title')} description={t('review.retry.description')} onClose={onClose}>
      <form className="review-retry-form" onSubmit={(event) => void submit(event)}>
        {!data && !error && <p className="field-hint">{t('review.retry.loading')}</p>}
        <label htmlFor={providerId}>{t('review.retry.provider')}</label>
        <select id={providerId} value={provider} disabled={!data || busy} onChange={(event) => setProvider(event.target.value)}>
          <option value="">{t('review.retry.current', { value: data?.default_provider ?? '-' })}</option>
          {data?.providers.map((entry) => (
            <option key={entry.name} value={entry.name}>
              {entry.name}
            </option>
          ))}
        </select>
        <label htmlFor={modelId}>{t('review.retry.model')}</label>
        <input
          id={modelId}
          value={model}
          maxLength={200}
          disabled={!data || busy}
          placeholder={defaultModel}
          aria-describedby={`${modelId}-hint`}
          onChange={(event) => setModel(event.target.value)}
        />
        <small className="field-hint" id={`${modelId}-hint`}>
          {t('review.retry.model_hint', { model: defaultModel || '-' })}
        </small>
        <label htmlFor={promptId}>{t('review.retry.prompt')}</label>
        <select id={promptId} value={prompt} disabled={!data || busy} onChange={(event) => setPrompt(event.target.value)}>
          <option value="">
            {activePrompt
              ? t('review.retry.current', { value: `${activePrompt.name} v${activePrompt.version}` })
              : t('review.retry.current_prompt')}
          </option>
          {data?.prompts.map((entry) => (
            <option key={entry.id} value={entry.id}>
              {entry.name} v{entry.version}
            </option>
          ))}
        </select>
        {error && (
          <p className="form-error" role="alert">
            {error}
          </p>
        )}
        <div className="confirm-dialog-actions">
          <button className="secondary-button" type="button" onClick={onClose}>
            {t('generic.cancel')}
          </button>
          <button className="primary-button" type="submit" disabled={!data || busy}>
            <RefreshCw size={16} aria-hidden="true" /> {t('review.retry.submit')}
          </button>
        </div>
      </form>
    </ModalFrame>
  );
}
