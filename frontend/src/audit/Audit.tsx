import { useCallback, useRef, useState, type FormEvent } from 'react';
import { Archive, Check, ChevronDown, FileText, Filter, Shield, X } from 'lucide-react';
import { api, AuditEvent, AuditIntegrityReport, AuditQueryParams, RetentionResult } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { ActionButton, Button, FormField, PageHeader, Status, localizedErrorMessage, run } from '../lib/ui';
import { useConfirm } from '../lib/ConfirmDialog';
import { replaceLocation } from '../lib/router';
import { EmptyState } from '../lib/states';
import { useResource } from '../lib/useResource';
import { AuditDiffDrawer } from './AuditDiffDrawer';

/** Page size of the audit log (#448); older events load via the keyset cursor. */
const AUDIT_PAGE_SIZE = 100;

/** Filter fields mirrored into the URL so a filtered view survives reloads (#448). */
const AUDIT_FILTER_KEYS = ['actor', 'event_type', 'document_id', 'outcome', 'from', 'to'] as const;
type AuditFilterKey = (typeof AUDIT_FILTER_KEYS)[number];
type AuditFilters = Partial<Record<AuditFilterKey, string>>;

const OUTCOMES = [
  'success',
  'failed',
  'retry',
  'warning',
  'applied',
  'dropped',
  'rejected',
  'review',
  'skipped',
  'validation_failed',
  'partial_failure'
] as const;

function filtersFromSearch(search: string): AuditFilters {
  const sp = new URLSearchParams(search);
  const filters: AuditFilters = {};
  for (const key of AUDIT_FILTER_KEYS) {
    const value = sp.get(key)?.trim();
    if (value) filters[key] = value;
  }
  return filters;
}

function filtersToSearch(filters: AuditFilters): string {
  const sp = new URLSearchParams();
  for (const key of AUDIT_FILTER_KEYS) {
    const value = filters[key]?.trim();
    if (value) sp.set(key, value);
  }
  const qs = sp.toString();
  return qs ? `?${qs}` : '';
}

export function Audit({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime, formatNumber } = useI18n();
  const [retentionResult, setRetentionResult] = useState<RetentionResult | null>(null);
  const [busy, setBusy] = useState(false);
  const { confirm, dialog: confirmDialog } = useConfirm();
  const onError = (err: unknown) => setError(localizedErrorMessage(err, t));
  // Applied filters drive the query; the draft is what the form shows (#448).
  const [filters, setFilters] = useState<AuditFilters>(() => filtersFromSearch(window.location.search));
  const [draft, setDraft] = useState<AuditFilters>(filters);
  const filterKey = filtersToSearch(filters);
  const [olderItems, setOlderItems] = useState<AuditEvent[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [diffEventId, setDiffEventId] = useState<string | null>(null);
  // Identifies the filter context a load-more response belongs to.
  const contextRef = useRef(filterKey);
  contextRef.current = filterKey;

  const query = (cursor?: string): AuditQueryParams => ({ ...filters, limit: AUDIT_PAGE_SIZE, cursor });
  // Two resources so "Verify chain" re-checks integrity without refetching the log (#444).
  const events = useResource((signal) => api.auditSearch(query(), { signal }), [filterKey], {
    onError,
    onSuccess: (page) => {
      setOlderItems([]);
      setNextCursor(page.next_cursor ?? null);
    }
  });
  const integrityResource = useResource((signal) => api.auditIntegrity({ signal }), [], { onError });
  const items: AuditEvent[] = [...(events.data?.items ?? []), ...olderItems];
  const integrity: AuditIntegrityReport | null = integrityResource.data ?? null;
  const refreshIntegrity = integrityResource.reload;

  const applyFilters = (next: AuditFilters) => {
    setOlderItems([]);
    setNextCursor(null);
    setFilters(next);
    replaceLocation(`${window.location.pathname}${filtersToSearch(next)}`);
  };

  const submitFilters = (event: FormEvent) => {
    event.preventDefault();
    applyFilters(draft);
  };

  const clearFilters = () => {
    setDraft({});
    applyFilters({});
  };

  const loadMore = async () => {
    if (!nextCursor || loadingMore) return;
    const context = contextRef.current;
    setLoadingMore(true);
    try {
      const page = await api.auditSearch(query(nextCursor));
      // A filter change in the meantime started a new result set.
      if (context !== contextRef.current) return;
      setOlderItems((current) => [...current, ...page.items]);
      setNextCursor(page.next_cursor ?? null);
    } catch (err) {
      onError(err);
    } finally {
      setLoadingMore(false);
    }
  };

  const closeDiff = useCallback(() => setDiffEventId(null), []);

  // #417: retention permanently deletes audit events, AI artifacts and cached
  // OCR pages. Confirm with the configured retention windows when the viewer
  // may read settings; otherwise state the scope without numbers.
  const applyRetention = async () => {
    const security = await api.settings().then((settings) => settings.security).catch(() => null);
    const confirmed = await confirm({
      title: t('audit.retention_confirm.title'),
      description: t('audit.retention_confirm.description'),
      confirmLabel: t('audit.apply_retention'),
      details: security
        ? t('audit.retention_confirm.scope', {
            audit_days: formatNumber(security.audit_retention_days),
            artifact_days: formatNumber(security.ai_artifact_retention_days)
          })
        : t('audit.retention_confirm.scope_unknown')
    });
    if (!confirmed) return;
    await run(setBusy, setError, () => api.applyAuditRetention().then((result) => {
      setRetentionResult(result);
      return Promise.all([events.reload(), integrityResource.reload()]);
    }), t);
  };

  const setDraftValue = (key: AuditFilterKey, value: string) =>
    setDraft((current) => ({ ...current, [key]: value || undefined }));
  const eventTypeOptions = Array.from(new Set(items.map((item) => item.event_type))).sort();
  const filtersActive = filterKey !== '';

  return (
    <section className="page">
      <PageHeader title={t('audit.title')} />
      <div className="toolbar">
        <a className="button-link" href="/api/audit/export.csv">
          <FileText size={16} /> {t('audit.export_csv')}
        </a>
        <Button variant="secondary" icon={<Shield size={16} />} onClick={refreshIntegrity}>
          {t('audit.verify_chain')}
        </Button>
        <ActionButton
          icon={<Archive />}
          label={t('audit.apply_retention')}
          busy={busy}
          onClick={applyRetention}
        />
      </div>
      <form className="advanced-filter-panel audit-filters" role="search" aria-label={t('audit.filter.label')} onSubmit={submitFilters}>
        <FormField label={t('audit.filter.actor')}>
          <input type="text" value={draft.actor ?? ''} onChange={(event) => setDraftValue('actor', event.target.value)} />
        </FormField>
        <FormField label={t('audit.filter.event_type')}>
          <input
            type="text"
            list="audit-event-types"
            value={draft.event_type ?? ''}
            onChange={(event) => setDraftValue('event_type', event.target.value)}
          />
          <datalist id="audit-event-types">
            {eventTypeOptions.map((type) => <option key={type} value={type} />)}
          </datalist>
        </FormField>
        <FormField label={t('audit.filter.document')}>
          <input
            type="number"
            min={1}
            value={draft.document_id ?? ''}
            onChange={(event) => setDraftValue('document_id', event.target.value)}
          />
        </FormField>
        <FormField label={t('audit.filter.outcome')}>
          <select value={draft.outcome ?? ''} onChange={(event) => setDraftValue('outcome', event.target.value)}>
            <option value="">{t('inventory.filter.any')}</option>
            {OUTCOMES.map((outcome) => <option key={outcome} value={outcome}>{outcome}</option>)}
          </select>
        </FormField>
        <FormField label={t('audit.filter.from')}>
          <input type="date" value={draft.from ?? ''} onChange={(event) => setDraftValue('from', event.target.value)} />
        </FormField>
        <FormField label={t('audit.filter.to')}>
          <input type="date" value={draft.to ?? ''} onChange={(event) => setDraftValue('to', event.target.value)} />
        </FormField>
        <div className="toolbar">
          <button type="submit" className="primary-button">
            <Filter size={16} aria-hidden="true" /> {t('audit.filter.apply')}
          </button>
          {filtersActive && (
            <button type="button" className="chip-button" onClick={clearFilters}>
              <X size={14} aria-hidden="true" /> {t('inventory.clear_filters')}
            </button>
          )}
        </div>
      </form>
      {integrity && (
        <div
          className={`connection-feedback ${integrity.ok ? 'success' : 'error'}`}
          role={integrity.ok ? 'status' : 'alert'}
          aria-live={integrity.ok ? 'polite' : 'assertive'}
        >
          <header>
            {integrity.ok ? <Check size={16} /> : <X size={16} />}
            <strong>{integrity.ok ? t('audit.chain_verified') : t('audit.chain_problem')}</strong>
          </header>
          <p>
            {t('audit.checked_events', { count: formatNumber(integrity.checked_events) })}
            {` ${t('audit.hash_coverage', {
              v1: formatNumber(integrity.v1_events),
              v2: formatNumber(integrity.v2_events)
            })}`}
            {integrity.legacy_events > 0 ? ` ${t('audit.legacy_events', { count: formatNumber(integrity.legacy_events) })}` : ''}
            {integrity.legacy_precision_events > 0 ? ` ${t('audit.legacy_precision_events', { count: formatNumber(integrity.legacy_precision_events) })}` : ''}
            {integrity.broken_reason ? ` ${integrity.broken_reason}` : ''}
          </p>
        </div>
      )}
      {retentionResult && (
        <div className="connection-feedback success" role="status" aria-live="polite">
          <header><Check size={16} /><strong>{t('audit.retention_applied')}</strong></header>
          <p>
            {t('audit.retention_summary', {
              artifacts: formatNumber(retentionResult.ai_artifacts_deleted),
              events: formatNumber(retentionResult.audit_events_deleted),
              ocr_pages: formatNumber(retentionResult.ocr_page_cache_deleted)
            })}
          </p>
        </div>
      )}
      <div className="table-wrap">
        <table aria-busy={events.loading} aria-label={t('audit.title')}>
          <thead>
            <tr>
              <th>{t('audit.col_time')}</th>
              <th>{t('audit.col_event')}</th>
              <th>{t('audit.col_actor')}</th>
              <th>{t('audit.col_document')}</th>
              <th>{t('audit.col_outcome')}</th>
              <th>{t('audit.col_changes')}</th>
              <th>{t('audit.col_hash')}</th>
            </tr>
          </thead>
          <tbody>
            {items.map((item) => (
              <tr key={item.id}>
                <td>{formatDateTime(item.created_at)}</td>
                <td>{item.event_type}</td>
                <td>
                  {item.actor_username ?? item.actor_id ?? '-'}
                  <small className="inventory-row-meta">{item.actor_type}</small>
                </td>
                <td>{item.paperless_document_id || '-'}</td>
                <td><Status value={item.outcome} /></td>
                <td>
                  {item.has_changes ? (
                    <button
                      type="button"
                      className="chip-button"
                      aria-label={t('audit.show_changes_for', { event: item.event_type, time: formatDateTime(item.created_at) })}
                      onClick={() => setDiffEventId(item.id)}
                    >
                      {t('audit.show_changes')}
                    </button>
                  ) : '-'}
                </td>
                <td>{item.event_hash ? `v${item.hash_version ?? 1}:${item.event_hash.slice(0, 12)}...` : t('audit.hash_legacy')}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!events.loading && events.error == null && items.length === 0 && (
        <EmptyState message={t('audit.empty')} />
      )}
      {nextCursor && (
        <div className="toolbar">
          <ActionButton icon={<ChevronDown />} label={t('audit.load_more')} busy={loadingMore} onClick={loadMore} />
        </div>
      )}
      {diffEventId && <AuditDiffDrawer eventId={diffEventId} onClose={closeDiff} />}
      {confirmDialog}
    </section>
  );
}
