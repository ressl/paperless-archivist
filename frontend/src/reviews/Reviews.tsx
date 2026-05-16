import { useEffect, useState } from 'react';
import { AlertTriangle, Check, ListChecks, Save, X } from 'lucide-react';
import { api, ReviewItem, Stage } from '../api/client';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { PageHeader, localizedErrorMessage, run } from '../lib/ui';
import { stageLabel } from '../lib/format';
import { DebugContextDetails } from '../lib/DebugContextDetails';

type ReviewPatchRecord = Record<string, unknown> & {
  standard_metadata?: Record<string, unknown>;
};

type ReviewEditState = {
  title?: string;
  correspondent?: string;
  document_type?: string;
  created?: string;
};

export function Reviews({ setError }: { setError: (error: string | null) => void }) {
  const { t } = useI18n();
  const [items, setItems] = useState<ReviewItem[]>([]);
  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const load = () => api.reviews().then((data) => setItems(data.items)).catch((err) => setError(localizedErrorMessage(err, t)));

  useEffect(() => {
    void load();
  }, []);

  const toggleSelected = (id: string) => {
    setSelected((current) => current.includes(id) ? current.filter((item) => item !== id) : [...current, id]);
  };
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
      </div>
      <div className="review-list">
        {items.map((item) => (
          <ReviewCard
            key={item.id}
            item={item}
            selected={selected.includes(item.id)}
            onSelect={() => toggleSelected(item.id)}
            onReload={load}
            setError={setError}
            t={t}
          />
        ))}
      </div>
    </section>
  );
}

function ReviewCard({
  item,
  selected,
  onSelect,
  onReload,
  setError,
  t
}: {
  item: ReviewItem;
  selected: boolean;
  onSelect: () => void;
  onReload: () => void;
  setError: (error: string | null) => void;
  t: TFunction;
}) {
  const patch = asReviewPatch(item.suggested_patch);
  const metadata = asReviewPatch(patch?.standard_metadata);
  const [edit, setEdit] = useState<ReviewEditState>(() => reviewEditStateFromPatch(patch));
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setEdit(reviewEditStateFromPatch(patch));
  }, [item.id]);

  const applyEdited = async () => {
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
  };

  const warnings = reviewWarnings(item.validation_warnings);
  const rows = standardMetadataRows(item.stage, patch, metadata, t);

  return (
    <article className="review-item">
      <header>
        <label className="inline">
          <input type="checkbox" checked={selected} onChange={onSelect} />
          <strong>{t('review.document', { id: item.paperless_document_id })}</strong>
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
        <button title={t('review.approve')} disabled={busy} onClick={() => api.approveReview(item.id).then(onReload).catch((err) => setError(localizedErrorMessage(err, t)))}>
          <Check size={16} /> {t('review.approve')}
        </button>
        {patch && Object.keys(edit).length > 0 && (
          <button title={t('review.apply_edited')} disabled={busy} onClick={() => void applyEdited()}>
            <Save size={16} /> {t('review.apply_edited')}
          </button>
        )}
        <button title={t('review.reject')} disabled={busy} onClick={() => api.rejectReview(item.id).then(onReload).catch((err) => setError(localizedErrorMessage(err, t)))}>
          <X size={16} /> {t('review.reject')}
        </button>
      </footer>
    </article>
  );
}

function asReviewPatch(value: unknown): ReviewPatchRecord | null {
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    return value as ReviewPatchRecord;
  }
  return null;
}

function reviewEditStateFromPatch(patch: ReviewPatchRecord | null): ReviewEditState {
  if (!patch) return {};
  const state: ReviewEditState = {};
  if (Object.prototype.hasOwnProperty.call(patch, 'title')) state.title = String(patch.title ?? '');
  if (Object.prototype.hasOwnProperty.call(patch, 'correspondent')) state.correspondent = String(patch.correspondent ?? '');
  if (Object.prototype.hasOwnProperty.call(patch, 'document_type')) state.document_type = String(patch.document_type ?? '');
  if (Object.prototype.hasOwnProperty.call(patch, 'created')) state.created = String(patch.created ?? '');
  return state;
}

function buildEditedReviewPatch(patch: ReviewPatchRecord, edit: ReviewEditState): Record<string, unknown> | null {
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
  if (stage === 'correspondent' && Object.prototype.hasOwnProperty.call(patch, 'correspondent')) {
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
  if (stage === 'document_type' && Object.prototype.hasOwnProperty.call(patch, 'document_type')) {
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
  if (stage === 'document_date' && Object.prototype.hasOwnProperty.call(patch, 'created')) {
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
