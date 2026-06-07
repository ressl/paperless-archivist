import { useEffect, useMemo, useRef } from 'react';
import { X } from 'lucide-react';
import { type MetadataFieldOutcome, type MetadataTrace } from '../api/client';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { Status, useFocusTrap } from '../lib/ui';

type FieldKey = MetadataFieldOutcome['field'];
type OutcomeKey = MetadataFieldOutcome['outcome'];

const DIAGNOSE_FIELD_ORDER: FieldKey[] = [
  'title',
  'correspondent',
  'document_type',
  'document_date',
  'tags',
  'fields',
];

const KNOWN_REASONS = new Set([
  'below_threshold',
  'unknown_choice',
  'no_proposal',
  'overwrite_disabled',
  'entity_not_found',
  'parse_failure',
  'anchor_missing',
  'over_max_tags',
  'rejected_by_operator',
]);

function outcomeTone(outcome: OutcomeKey): 'success' | 'review' | 'neutral' | 'danger' {
  switch (outcome) {
    case 'applied':
      return 'success';
    case 'review':
      return 'review';
    case 'rejected':
      return 'danger';
    case 'skipped':
    case 'dropped':
    default:
      return 'neutral';
  }
}

function describeFieldValue(field: FieldKey, value: unknown): string | null {
  if (value == null) return null;
  if (field === 'tags') {
    if (Array.isArray(value)) {
      return value.length ? value.map((tag) => String(tag)).join(', ') : null;
    }
    return null;
  }
  if (field === 'fields') {
    if (Array.isArray(value)) {
      const parts = value
        .map((entry) => {
          if (entry && typeof entry === 'object' && 'name' in entry && 'value' in entry) {
            const rec = entry as { name: unknown; value: unknown };
            return `${String(rec.name)}: ${String(rec.value)}`;
          }
          return null;
        })
        .filter((s): s is string => s != null);
      return parts.length ? parts.join(', ') : null;
    }
    return null;
  }
  if (typeof value === 'string') return value;
  return String(value);
}

function describeWarning(t: TFunction, warning: Record<string, unknown>): string {
  const kind = typeof warning.kind === 'string' ? warning.kind : null;
  if (kind === 'LowConfidence' && typeof warning.got === 'number' && typeof warning.threshold === 'number') {
    return `${kind} ${Math.round((warning.got as number) * 100)}%/${Math.round((warning.threshold as number) * 100)}%`;
  }
  return kind ?? JSON.stringify(warning);
}

type DiagnoseDrawerProps = {
  documentId: number;
  trace: MetadataTrace | null;
  busy: boolean;
  missing: boolean;
  onClose: () => void;
};

export function DiagnoseDrawer({ documentId, trace, busy, missing, onClose }: DiagnoseDrawerProps) {
  const { t, formatRelativeTime } = useI18n();
  const drawerRef = useRef<HTMLElement>(null);
  const titleId = `diagnose-drawer-title-${documentId}`;
  useFocusTrap(true, drawerRef);
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [onClose]);

  const outcomesByField = useMemo(() => {
    const map = new Map<FieldKey, MetadataFieldOutcome>();
    trace?.latest_run.per_field_outcomes.forEach((o) => map.set(o.field, o));
    return map;
  }, [trace]);

  return (
    <div className="drawer-root" role="dialog" aria-modal="true" aria-labelledby={titleId}>
      <div className="drawer-backdrop" onClick={onClose} />
      <aside ref={drawerRef} className="drawer diagnose-drawer" aria-busy={busy}>
        <header>
          <strong id={titleId}>{t('inventory.diagnose.title', { id: documentId })}</strong>
          <button
            type="button"
            className="drawer-close"
            onClick={onClose}
            aria-label={t('inventory.diagnose.close')}
          >
            <X size={18} />
          </button>
        </header>
        <div className="diagnose-body">
          {busy && !trace && !missing && (
            <p className="field-hint">{t('generic.loading')}</p>
          )}
          {missing && (
            <p className="field-hint">{t('inventory.diagnose.no_run')}</p>
          )}
          {trace && (
            <>
              <section className="drawer-section">
                <strong>{t('inventory.diagnose.current_state')}</strong>
                <dl className="diagnose-state">
                  <div>
                    <dt>{t('inventory.diagnose.field.title')}</dt>
                    <dd>{trace.current_state.title ?? '—'}</dd>
                  </div>
                  <div>
                    <dt>{t('inventory.diagnose.field.correspondent')}</dt>
                    <dd>{trace.current_state.correspondent ?? '—'}</dd>
                  </div>
                  <div>
                    <dt>{t('inventory.diagnose.field.document_type')}</dt>
                    <dd>{trace.current_state.document_type ?? '—'}</dd>
                  </div>
                  <div>
                    <dt>{t('inventory.diagnose.field.document_date')}</dt>
                    <dd>{trace.current_state.document_date ?? '—'}</dd>
                  </div>
                  <div className="diagnose-state-tags">
                    <dt>{t('inventory.diagnose.field.tags')}</dt>
                    <dd>
                      {trace.current_state.tags.length === 0 ? (
                        <span>—</span>
                      ) : (
                        <div className="diagnose-chip-row">
                          {trace.current_state.tags.map((tag) => (
                            <span key={tag} className="diagnose-tag-chip">{tag}</span>
                          ))}
                        </div>
                      )}
                    </dd>
                  </div>
                </dl>
              </section>

              <section className="drawer-section">
                <strong>{t('inventory.diagnose.latest_run')}</strong>
                <dl className="diagnose-state">
                  <div>
                    <dt>{t('inventory.diagnose.model')}</dt>
                    <dd>{trace.latest_run.model ?? '—'}</dd>
                  </div>
                  <div>
                    <dt>{t('inventory.diagnose.provider')}</dt>
                    <dd>{trace.latest_run.provider ?? '—'}</dd>
                  </div>
                  <div>
                    <dt>{t('inventory.diagnose.status')}</dt>
                    <dd><Status value={trace.latest_run.status} /></dd>
                  </div>
                  <div>
                    <dt>{t('inventory.diagnose.run_id')}</dt>
                    <dd><code>{trace.latest_run.run_id}</code></dd>
                  </div>
                  <div className="diagnose-state-span">
                    <dd>{t('inventory.diagnose.created_at', { time: formatRelativeTime(trace.latest_run.created_at) })}</dd>
                  </div>
                  <div className="diagnose-state-span">
                    <dd>
                      {trace.latest_run.applied_at
                        ? t('inventory.diagnose.applied_at', { time: formatRelativeTime(trace.latest_run.applied_at) })
                        : t('inventory.diagnose.not_applied')}
                    </dd>
                  </div>
                </dl>
              </section>

              <section className="drawer-section">
                <div className="diagnose-field-grid">
                  {DIAGNOSE_FIELD_ORDER.map((field) => {
                    const outcome = outcomesByField.get(field);
                    return (
                      <FieldOutcomeCard key={field} field={field} outcome={outcome} t={t} />
                    );
                  })}
                </div>
              </section>

              <details className="diagnose-raw">
                <summary>{t('inventory.diagnose.raw_suggestion')}</summary>
                <pre>{JSON.stringify(trace.latest_run.llm_suggestion ?? null, null, 2)}</pre>
              </details>
            </>
          )}
        </div>
      </aside>
    </div>
  );
}

function FieldOutcomeCard({ field, outcome, t }: { field: FieldKey; outcome?: MetadataFieldOutcome; t: TFunction }) {
  const label = t(`inventory.diagnose.field.${field}` as Parameters<TFunction>[0]);
  if (!outcome) {
    return (
      <article className="diagnose-field-card">
        <header>
          <span className="diagnose-field-label">{label}</span>
          <span className="status neutral">{t('inventory.diagnose.no_proposal_short')}</span>
        </header>
      </article>
    );
  }
  const tone = outcomeTone(outcome.outcome);
  const value = describeFieldValue(field, outcome.value);
  const confidencePct =
    typeof outcome.confidence === 'number' ? Math.round(outcome.confidence * 100) : null;
  const reasonText =
    outcome.reason && KNOWN_REASONS.has(outcome.reason)
      ? t(`inventory.diagnose.reason.${outcome.reason}` as Parameters<TFunction>[0])
      : outcome.reason;
  const warnings = Array.isArray(outcome.warnings) ? outcome.warnings : [];
  return (
    <article className={`diagnose-field-card tone-${tone}`}>
      <header>
        <span className="diagnose-field-label">{label}</span>
        <span className={`status ${tone}`}>
          {t(`inventory.diagnose.outcome.${outcome.outcome}` as Parameters<TFunction>[0])}
        </span>
      </header>
      {value != null && <p className="diagnose-field-value">{value}</p>}
      {confidencePct != null && (
        <p className="diagnose-field-confidence">
          {t('inventory.diagnose.confidence')}: {confidencePct}%
        </p>
      )}
      {reasonText && <p className="diagnose-field-reason">{reasonText}</p>}
      {warnings.length > 0 && (
        <div className="diagnose-chip-row">
          {warnings.map((warning, idx) => (
            <span key={idx} className="diagnose-warning-chip">
              {describeWarning(t, warning as Record<string, unknown>)}
            </span>
          ))}
        </div>
      )}
    </article>
  );
}
