import { memo, useCallback, useEffect, useState } from 'react';
import { AlertTriangle, Check, ChevronDown, ListChecks, Save, Wrench, X } from 'lucide-react';
import { api, ReviewItem, Stage } from '../api/client';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { PageHeader, localizedErrorMessage, run } from '../lib/ui';
import { stageLabel } from '../lib/format';
import { DebugContextDetails } from '../lib/DebugContextDetails';

export type ReviewPatchRecord = Record<string, unknown> & {
  standard_metadata?: Record<string, unknown>;
};

export type ReviewEditState = {
  title?: string;
  correspondent?: string;
  document_type?: string;
  created?: string;
};

// One Load-More step. The backend clamps `limit` to 500, so beyond that we cannot
// reach further pending items without a paginated (offset) endpoint.
const REVIEW_PAGE_SIZE = 100;
const REVIEW_MAX_LIMIT = 500;

export function Reviews({ setError, setSuccess }: { setError: (error: string | null) => void; setSuccess: (message: string | null) => void }) {
  const { t } = useI18n();
  const [items, setItems] = useState<ReviewItem[]>([]);
  const [total, setTotal] = useState(0);
  const [serverHasMore, setServerHasMore] = useState(false);
  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [limit, setLimit] = useState(REVIEW_PAGE_SIZE);
  const load = useCallback(
    () =>
      api
        .reviews(limit)
        .then((data) => {
          setItems(data.items);
          setTotal(data.total);
          setServerHasMore(data.has_more);
        })
        .catch((err) => setError(localizedErrorMessage(err, t))),
    [limit, setError, t]
  );

  useEffect(() => {
    void load();
  }, [load]);

  // The server reports whether more pending items exist beyond this page; we can
  // only keep loading until we hit the backend's hard cap on `limit`.
  const hasMore = serverHasMore && limit < REVIEW_MAX_LIMIT;
  const loadMore = useCallback(() => {
    setLimit((current) => Math.min(current + REVIEW_PAGE_SIZE, REVIEW_MAX_LIMIT));
  }, []);

  const toggleSelected = useCallback((id: string) => {
    setSelected((current) => current.includes(id) ? current.filter((item) => item !== id) : [...current, id]);
  }, []);
  const batch = async (decision: 'approve' | 'reject') => {
    await run(setBusy, setError, async () => {
      const result = await api.batchReview(selected, decision);
      if (result.failed.length > 0) {
        setError(t('review.failed_batch', { count: result.failed.length, error: result.failed[0].error }));
      }
      setSelected([]);
      await load();
    }, t);
  };

  const autoFixAll = async () => {
    if (items.length === 0) return;
    const preview = await api.autoFixReviewPreview(items.length);
    const confirmed = window.confirm(
      t('review.auto_fix_confirm', {
        count: preview.total_pending,
      })
    );
    if (!confirmed) return;
    await run(setBusy, setError, async () => {
      const result = await api.autoFixReviewBulk(items.length);
      setSelected([]);
      await load();
      // Positive outcome → success banner, not the red error box (#228).
      setSuccess(
        t('review.auto_fix_result', {
          applied: result.applied,
          rejected: result.rejected,
          errors: result.errors.length,
        })
      );
    }, t);
  };

  const autoFixOne = async (id: string) => {
    await run(setBusy, setError, async () => {
      const result = await api.autoFixReviewSingle(id);
      await load();
      // Positive outcome → success banner, not the red error box (#228).
      setSuccess(
        t('review.auto_fix_result', {
          applied: result.action === 'applied' ? 1 : 0,
          rejected: result.action === 'rejected' ? 1 : 0,
          errors: 0,
        })
      );
    }, t);
  };

  return (
    <section className="page">
      <PageHeader title={t('review.title')} />
      <div className="toolbar">
        <button disabled={items.length === 0} onClick={() => setSelected(selected.length === items.length ? [] : items.map((item) => item.id))}>
          <ListChecks size={16} /> {selected.length === items.length ? t('review.clear_selection') : t('review.select_all')}
        </button>
        <button disabled={busy || selected.length === 0} onClick={() => void batch('approve')}>
          <Check size={16} /> {t('review.approve_selected')}
        </button>
        <button disabled={busy || selected.length === 0} onClick={() => void batch('reject')}>
          <X size={16} /> {t('review.reject_selected')}
        </button>
        <button disabled={busy || items.length === 0} onClick={() => void autoFixAll()} title={t('review.auto_fix_all')}>
          <Wrench size={16} /> {t('review.auto_fix_all')}
        </button>
        <small className="field-hint">{t('reviews.count', { shown: items.length, total })}</small>
      </div>
      <div className="review-list">
        {items.map((item) => (
          <ReviewCardMemo
            key={item.id}
            item={item}
            selected={selected.includes(item.id)}
            onSelect={toggleSelected}
            onReload={load}
            onAutoFix={autoFixOne}
            setError={setError}
            t={t}
          />
        ))}
      </div>
      {hasMore && (
        <div className="toolbar">
          <button disabled={busy} onClick={loadMore}>
            <ChevronDown size={16} /> {t('inventory.load_more')}
          </button>
        </div>
      )}
    </section>
  );
}

type ReviewCardProps = {
  item: ReviewItem;
  selected: boolean;
  onSelect: (id: string) => void;
  onReload: () => void;
  onAutoFix: (id: string) => void;
  setError: (error: string | null) => void;
  t: TFunction;
};

function ReviewCard({ item, selected, onSelect, onReload, onAutoFix, setError, t }: ReviewCardProps) {
  const patch = asReviewPatch(item.suggested_patch);
  const metadata = asReviewPatch(patch?.standard_metadata);
  const [edit, setEdit] = useState<ReviewEditState>(() => reviewEditStateFromPatch(patch));
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setEdit(reviewEditStateFromPatch(patch));
  }, [item.id]);

  const applyEdited = useCallback(async () => {
    if (!patch) {
      setError(t('review.patch_not_editable'));
      return;
    }
    const editedPatch = buildEditedReviewPatch(patch, edit);
    if (editedPatch === null) {
      setError(t('review.invalid_numeric_id'));
      return;
    }
    await run(setBusy, setError, async () => {
      await api.editReview(item.id, editedPatch);
      onReload();
    }, t);
  }, [patch, edit, item.id, onReload, setError, t]);

  const approve = useCallback(
    () => api.approveReview(item.id).then(onReload).catch((err) => setError(localizedErrorMessage(err, t))),
    [item.id, onReload, setError, t]
  );

  const reject = useCallback(
    () => api.rejectReview(item.id).then(onReload).catch((err) => setError(localizedErrorMessage(err, t))),
    [item.id, onReload, setError, t]
  );

  const handleSelect = useCallback(() => onSelect(item.id), [onSelect, item.id]);

  const warnings = reviewWarnings(item.validation_warnings);
  const rows = standardMetadataRows(item.stage, patch, metadata, t);

  return (
    <article className="review-item">
      <header>
        <label className="inline">
          <input type="checkbox" checked={selected} onChange={handleSelect} />
          <strong>{item.paperless_title ?? t('review.document', { id: item.paperless_document_id })}</strong>
          {item.paperless_title && <small className="field-hint">#{item.paperless_document_id}</small>}
        </label>
        <span>{stageLabel(item.stage as Stage, t) ?? item.stage}</span>
      </header>

      {rows.length > 0 && (
        <div className="metadata-review">
          {rows.map((row) => (
            <div className={`metadata-review-row ${row.lowConfidence ? 'low-confidence' : ''}`} key={row.field}>
              <div>
                <strong>{row.label}</strong>
                <small>{t('review.current', { value: row.current ?? t('generic.empty') })}</small>
              </div>
              <div>
                <span>{t('review.suggestion', { value: row.suggested ?? t('generic.empty') })}</span>
                {row.confidence !== null && <small>{t('review.confidence', { value: `${(row.confidence * 100).toFixed(0)}%` })}</small>}
                {row.evidence && <small>{t('review.evidence', { value: row.evidence })}</small>}
              </div>
              {row.editableKey && (
                <label>
                  {t('review.edit')}
                  <input
                    type={row.editableKey === 'created' ? 'date' : 'text'}
                    value={edit[row.editableKey] ?? ''}
                    onChange={(event) => setEdit((current) => ({ ...current, [row.editableKey!]: event.target.value }))}
                    placeholder={row.placeholder}
                  />
                </label>
              )}
            </div>
          ))}
        </div>
      )}

      {warnings.length > 0 && (
        <div className="review-warnings">
          {warnings.map((warning) => (
            <span key={warning}><AlertTriangle size={14} /> {warning}</span>
          ))}
        </div>
      )}

      <DebugContextDetails context={item.debug_context} />

      <details>
        <summary>{t('review.raw_patch')}</summary>
        <pre>{JSON.stringify(item.suggested_patch, null, 2)}</pre>
      </details>

      <footer>
        <button title={t('review.approve')} disabled={busy} onClick={approve}>
          <Check size={16} /> {t('review.approve')}
        </button>
        {patch && Object.keys(edit).length > 0 && (
          <button title={t('review.apply_edited')} disabled={busy} onClick={() => void applyEdited()}>
            <Save size={16} /> {t('review.apply_edited')}
          </button>
        )}
        <button title={t('review.auto_fix_one')} disabled={busy} onClick={() => onAutoFix(item.id)}>
          <Wrench size={16} /> {t('review.auto_fix_one')}
        </button>
        <button title={t('review.reject')} disabled={busy} onClick={reject}>
          <X size={16} /> {t('review.reject')}
        </button>
      </footer>
    </article>
  );
}

const ReviewCardMemo = memo(
  ReviewCard,
  (prev, next) => {
    if (prev.t !== next.t) return false;
    if (prev.selected !== next.selected) return false;
    if (prev.onSelect !== next.onSelect) return false;
    if (prev.onReload !== next.onReload) return false;
    if (prev.onAutoFix !== next.onAutoFix) return false;
    if (prev.setError !== next.setError) return false;
    const a = prev.item;
    const b = next.item;
    return (
      a.id === b.id &&
      a.status === b.status &&
      a.stage === b.stage &&
      a.created_at === b.created_at &&
      a.paperless_document_id === b.paperless_document_id &&
      a.suggested_patch === b.suggested_patch &&
      a.edited_patch === b.edited_patch &&
      a.validation_warnings === b.validation_warnings &&
      a.debug_context === b.debug_context
    );
  }
);

function asReviewPatch(value: unknown): ReviewPatchRecord | null {
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    return value as ReviewPatchRecord;
  }
  return null;
}

export function reviewEditStateFromPatch(patch: ReviewPatchRecord | null): ReviewEditState {
  if (!patch) return {};
  const state: ReviewEditState = {};
  if (Object.prototype.hasOwnProperty.call(patch, 'title')) state.title = String(patch.title ?? '');
  if (Object.prototype.hasOwnProperty.call(patch, 'correspondent')) state.correspondent = String(patch.correspondent ?? '');
  if (Object.prototype.hasOwnProperty.call(patch, 'document_type')) state.document_type = String(patch.document_type ?? '');
  if (Object.prototype.hasOwnProperty.call(patch, 'created')) state.created = String(patch.created ?? '');
  return state;
}

export function buildEditedReviewPatch(patch: ReviewPatchRecord, edit: ReviewEditState): Record<string, unknown> | null {
  const edited: Record<string, unknown> = { ...patch };
  delete edited.standard_metadata;
  if (edit.title !== undefined) edited.title = edit.title.trim();
  if (edit.created !== undefined) edited.created = edit.created.trim();
  for (const key of ['correspondent', 'document_type'] as const) {
    if (edit[key] === undefined) continue;
    const trimmed = edit[key]!.trim();
    if (trimmed === '') {
      edited[key] = null;
      continue;
    }
    const numeric = Number(trimmed);
    if (!Number.isInteger(numeric) || numeric <= 0) return null;
    edited[key] = numeric;
  }
  return edited;
}

function reviewWarnings(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.map((warning) => typeof warning === 'string' ? warning : JSON.stringify(warning)).slice(0, 10);
}

function standardMetadataRows(stage: string, patch: ReviewPatchRecord | null, metadata: ReviewPatchRecord | null, t: TFunction) {
  if (!patch) return [];
  const rows: Array<{
    field: string;
    label: string;
    current?: string;
    suggested?: string;
    confidence: number | null;
    evidence?: string;
    lowConfidence: boolean;
    editableKey?: keyof ReviewEditState;
    placeholder?: string;
  }> = [];
  const confidence = numericMetadata(metadata?.confidence);
  // v1.4.0 consolidated metadata stage: each review item carries a `field` discriminator in
  // standard_metadata so the reviewer UX can show "Metadata · Correspondent" etc. cleanly.
  // For backward compatibility we still match on stage === 'correspondent' / 'document_type'
  // / 'document_date' for in-flight runs queued before v1.4.0.
  const metadataField = stage === 'metadata' ? metadataValue(metadata?.field) : undefined;
  const isMetaCorrespondent = stage === 'metadata' && metadataField === 'correspondent';
  const isMetaDocumentType = stage === 'metadata' && metadataField === 'document_type';
  const isMetaDocumentDate = stage === 'metadata' && metadataField === 'document_date';
  if ((stage === 'correspondent' || isMetaCorrespondent) && Object.prototype.hasOwnProperty.call(patch, 'correspondent')) {
    rows.push({
      field: 'correspondent',
      label: t('stage.correspondent'),
      current: metadataValue(metadata?.current_correspondent),
      suggested: metadataValue(metadata?.suggested_name) ?? metadataValue(patch.correspondent),
      confidence,
      evidence: metadataValue(metadata?.evidence),
      lowConfidence: confidence !== null && confidence < 0.7,
      editableKey: 'correspondent',
      placeholder: t('review.placeholder.correspondent')
    });
  }
  if ((stage === 'document_type' || isMetaDocumentType) && Object.prototype.hasOwnProperty.call(patch, 'document_type')) {
    rows.push({
      field: 'document_type',
      label: t('stage.document_type'),
      current: metadataValue(metadata?.current_document_type),
      suggested: metadataValue(metadata?.suggested_name) ?? metadataValue(patch.document_type),
      confidence,
      evidence: metadataValue(metadata?.evidence),
      lowConfidence: confidence !== null && confidence < 0.7,
      editableKey: 'document_type',
      placeholder: t('review.placeholder.document_type')
    });
  }
  if ((stage === 'document_date' || isMetaDocumentDate) && Object.prototype.hasOwnProperty.call(patch, 'created')) {
    rows.push({
      field: 'document_date',
      label: t('stage.document_date'),
      current: metadataValue(metadata?.current_date),
      suggested: metadataValue(metadata?.suggested_date) ?? metadataValue(patch.created),
      confidence,
      evidence: metadataValue(metadata?.evidence),
      lowConfidence: confidence !== null && confidence < 0.7,
      editableKey: 'created',
      placeholder: t('review.placeholder.document_date')
    });
  }
  return rows;
}

function numericMetadata(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function metadataValue(value: unknown): string | undefined {
  if (value === null || value === undefined || value === '') return undefined;
  if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') return String(value);
  return JSON.stringify(value);
}
