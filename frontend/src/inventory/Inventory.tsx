import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Copy, ExternalLink, FileText, Filter, RefreshCw, RotateCcw, Search, Sparkles, Stethoscope, Tags, X } from 'lucide-react';
import {
  api,
  type DuplicateGroup,
  type InventoryItem,
  type InventoryQueryParams,
  type MetadataFieldOutcome,
  type MetadataTrace,
  type Stage,
} from '../api/client';
import { languageOptions } from '../data/worldLanguages';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { ActionButton, PageHeader, Status, localizedErrorMessage, run } from '../lib/ui';
import { DebugContextDetails } from '../lib/DebugContextDetails';

const PAGE_SIZE = 500;
// Default stage set for a bulk re-run: the full business pipeline. Re-running a
// "succeeded-but-wrong" document should regenerate OCR + metadata from scratch.
const RERUN_STAGES: Stage[] = ['ocr', 'metadata'];
const STATUS_OPTIONS = ['queued', 'running', 'succeeded', 'failed', 'waiting_review', 'unknown'] as const;
const RUN_STATUS_OPTIONS = ['queued', 'running', 'waiting_review', 'applying', 'succeeded', 'failed'] as const;

type Filters = {
  id?: number;
  q?: string;
  ocr_status: string[];
  metadata_status: string[];
  run_status: string[];
  tags_include: string[];
  tags_exclude: string[];
  language?: string;
  date_from?: string;
  date_to?: string;
  has_error?: boolean;
  needs_review?: boolean;
};

const EMPTY_FILTERS: Filters = {
  ocr_status: [],
  metadata_status: [],
  run_status: [],
  tags_include: [],
  tags_exclude: [],
};

function parseFiltersFromUrl(): Filters {
  const sp = new URLSearchParams(window.location.search);
  const csv = (key: string) => {
    const value = sp.get(key);
    if (!value) return [];
    return value.split(',').map((s) => s.trim()).filter(Boolean);
  };
  const idValue = sp.get('id');
  const hasError = sp.get('has_error');
  const needsReview = sp.get('needs_review');
  return {
    id: idValue ? Number(idValue) || undefined : undefined,
    q: sp.get('q') ?? undefined,
    ocr_status: csv('ocr_status'),
    metadata_status: csv('metadata_status'),
    run_status: csv('run_status'),
    tags_include: csv('tag'),
    tags_exclude: csv('not_tag'),
    language: sp.get('lang') ?? undefined,
    date_from: sp.get('date_from') ?? undefined,
    date_to: sp.get('date_to') ?? undefined,
    has_error: hasError === 'true' ? true : hasError === 'false' ? false : undefined,
    needs_review: needsReview === 'true' ? true : needsReview === 'false' ? false : undefined,
  };
}

function filtersToUrl(filters: Filters): string {
  const sp = new URLSearchParams();
  if (filters.id != null) sp.set('id', String(filters.id));
  if (filters.q) sp.set('q', filters.q);
  if (filters.ocr_status.length) sp.set('ocr_status', filters.ocr_status.join(','));
  if (filters.metadata_status.length) sp.set('metadata_status', filters.metadata_status.join(','));
  if (filters.run_status.length) sp.set('run_status', filters.run_status.join(','));
  if (filters.tags_include.length) sp.set('tag', filters.tags_include.join(','));
  if (filters.tags_exclude.length) sp.set('not_tag', filters.tags_exclude.join(','));
  if (filters.language) sp.set('lang', filters.language);
  if (filters.date_from) sp.set('date_from', filters.date_from);
  if (filters.date_to) sp.set('date_to', filters.date_to);
  if (filters.has_error != null) sp.set('has_error', String(filters.has_error));
  if (filters.needs_review != null) sp.set('needs_review', String(filters.needs_review));
  const qs = sp.toString();
  return qs ? `?${qs}` : '';
}

function filtersToParams(filters: Filters): InventoryQueryParams {
  return {
    id: filters.id,
    q: filters.q,
    ocr_status: filters.ocr_status.length ? filters.ocr_status : undefined,
    metadata_status: filters.metadata_status.length ? filters.metadata_status : undefined,
    run_status: filters.run_status.length ? filters.run_status : undefined,
    tag: filters.tags_include.length ? filters.tags_include : undefined,
    not_tag: filters.tags_exclude.length ? filters.tags_exclude : undefined,
    lang: filters.language,
    date_from: filters.date_from,
    date_to: filters.date_to,
    has_error: filters.has_error,
    needs_review: filters.needs_review,
  };
}

function isFiltersEmpty(f: Filters): boolean {
  return (
    f.id == null &&
    !f.q &&
    !f.ocr_status.length &&
    !f.metadata_status.length &&
    !f.run_status.length &&
    !f.tags_include.length &&
    !f.tags_exclude.length &&
    !f.language &&
    !f.date_from &&
    !f.date_to &&
    f.has_error == null &&
    f.needs_review == null
  );
}

function formatLanguageDetection(item: InventoryItem, languages: ReturnType<typeof languageOptions>) {
  const tag = item.detected_language;
  if (!tag) return '-';
  const option = languages.find((language) => language.tag === tag);
  const label = option ? option.uiName : tag;
  const confidence = item.detected_language_confidence;
  if (confidence == null) return label;
  return `${label} ${Math.round(confidence * 100)}%`;
}

export function Inventory({ setError }: { setError: (error: string | null) => void }) {
  const { t, locale } = useI18n();
  const [items, setItems] = useState<InventoryItem[]>([]);
  const [total, setTotal] = useState<number>(0);
  const [busy, setBusy] = useState(false);
  const [filters, setFilters] = useState<Filters>(() => parseFiltersFromUrl());
  const [searchText, setSearchText] = useState<string>(() => {
    const f = parseFiltersFromUrl();
    return f.id != null ? String(f.id) : (f.q ?? '');
  });
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [duplicatesOpen, setDuplicatesOpen] = useState(false);
  const [diagnoseDocumentId, setDiagnoseDocumentId] = useState<number | null>(null);
  const [diagnoseTrace, setDiagnoseTrace] = useState<MetadataTrace | null>(null);
  const [diagnoseBusy, setDiagnoseBusy] = useState(false);
  const [diagnoseMissing, setDiagnoseMissing] = useState(false);
  const [selected, setSelected] = useState<Set<number>>(() => new Set());
  const [notice, setNotice] = useState<string | null>(null);
  const languages = useMemo(() => languageOptions(locale), [locale]);

  // Sync filters → URL whenever filters change.
  const urlSyncRef = useRef(true);
  useEffect(() => {
    if (!urlSyncRef.current) return;
    const next = filtersToUrl(filters);
    const current = window.location.search;
    if (next !== current) {
      window.history.replaceState(null, '', `${window.location.pathname}${next}${window.location.hash}`);
    }
  }, [filters]);

  // Monotonic request id: a slower earlier response (e.g. after a fast filter
  // change) must never overwrite the result of a newer in-flight request.
  const requestIdRef = useRef(0);

  const loadFirst = useCallback(() => {
    const requestId = ++requestIdRef.current;
    return api
      .inventory({ ...filtersToParams(filters), offset: 0, limit: PAGE_SIZE })
      .then((data) => {
        if (requestId !== requestIdRef.current) return;
        setItems(data.items);
        setTotal(data.total);
      })
      .catch((err) => {
        if (requestId !== requestIdRef.current) return;
        setError(localizedErrorMessage(err, t));
      });
  }, [filters, setError, t]);

  const loadMore = useCallback(() => {
    const requestId = ++requestIdRef.current;
    return api
      .inventory({ ...filtersToParams(filters), offset: items.length, limit: PAGE_SIZE })
      .then((data) => {
        if (requestId !== requestIdRef.current) return;
        setItems((prev) => [...prev, ...data.items]);
        setTotal(data.total);
      })
      .catch((err) => {
        if (requestId !== requestIdRef.current) return;
        setError(localizedErrorMessage(err, t));
      });
  }, [filters, items.length, setError, t]);

  useEffect(() => {
    void loadFirst();
  }, [loadFirst]);

  const commitSearch = useCallback(() => {
    const trimmed = searchText.trim();
    if (!trimmed) {
      setFilters((f) => ({ ...f, id: undefined, q: undefined }));
      return;
    }
    if (/^\d+$/.test(trimmed)) {
      setFilters((f) => ({ ...f, id: Number(trimmed), q: undefined }));
    } else {
      setFilters((f) => ({ ...f, q: trimmed, id: undefined }));
    }
  }, [searchText]);

  const toggleChip = useCallback((patch: Partial<Filters>, active: boolean) => {
    setFilters((f) => {
      if (active) {
        // remove chip's effect — match on the patch keys
        const next = { ...f };
        if (patch.ocr_status) next.ocr_status = [];
        if (patch.metadata_status) next.metadata_status = [];
        if (patch.run_status) next.run_status = [];
        if (patch.has_error != null) next.has_error = undefined;
        if (patch.needs_review != null) next.needs_review = undefined;
        return next;
      }
      return { ...f, ...patch };
    });
  }, []);

  const clearAll = useCallback(() => {
    setSearchText('');
    setFilters(EMPTY_FILTERS);
  }, []);

  const triggerOcr = useCallback(
    (documentId: number) =>
      api
        .triggerDocument(documentId, ['ocr'], 'manual_review')
        .then(loadFirst)
        .catch((err) => setError(localizedErrorMessage(err, t))),
    [loadFirst, setError, t]
  );

  const triggerMetadata = useCallback(
    (documentId: number) =>
      api
        .triggerDocument(documentId, ['metadata'], 'manual_review')
        .then(loadFirst)
        .catch((err) => setError(localizedErrorMessage(err, t))),
    [loadFirst, setError, t]
  );

  const triggerPipeline = useCallback(
    (documentId: number) =>
      api
        .triggerDocument(documentId, ['ocr', 'metadata'], 'manual_review')
        .then(loadFirst)
        .catch((err) => setError(localizedErrorMessage(err, t))),
    [loadFirst, setError, t]
  );

  const toggleSelect = useCallback((documentId: number) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(documentId)) {
        next.delete(documentId);
      } else {
        next.add(documentId);
      }
      return next;
    });
  }, []);

  const allOnPageSelected = items.length > 0 && items.every((item) => selected.has(item.paperless_document_id));

  const toggleSelectAll = useCallback(() => {
    setSelected((prev) => {
      const everySelected = items.length > 0 && items.every((item) => prev.has(item.paperless_document_id));
      if (everySelected) {
        const next = new Set(prev);
        items.forEach((item) => next.delete(item.paperless_document_id));
        return next;
      }
      const next = new Set(prev);
      items.forEach((item) => next.add(item.paperless_document_id));
      return next;
    });
  }, [items]);

  const rerunSelected = useCallback(() => {
    const ids = Array.from(selected);
    if (ids.length === 0) return Promise.resolve();
    setNotice(null);
    return api
      .bulkRerun(ids, RERUN_STAGES)
      .then((result) => {
        setSelected(new Set());
        setNotice(t('inventory.rerun_done', { count: result.queued }));
        return loadFirst();
      })
      .catch((err) => setError(localizedErrorMessage(err, t)));
  }, [selected, loadFirst, setError, t]);

  const openDiagnose = useCallback(
    async (documentId: number) => {
      setDiagnoseDocumentId(documentId);
      setDiagnoseTrace(null);
      setDiagnoseMissing(false);
      setDiagnoseBusy(true);
      try {
        const trace = await api.inventoryMetadataTrace(documentId);
        setDiagnoseTrace(trace);
      } catch (err) {
        const message = err instanceof Error ? err.message : '';
        if (message.toLowerCase().includes('no metadata run')) {
          setDiagnoseMissing(true);
        } else {
          setError(localizedErrorMessage(err, t));
          setDiagnoseDocumentId(null);
        }
      } finally {
        setDiagnoseBusy(false);
      }
    },
    [setError, t]
  );

  const closeDiagnose = useCallback(() => {
    setDiagnoseDocumentId(null);
    setDiagnoseTrace(null);
    setDiagnoseMissing(false);
  }, []);

  const hasMore = items.length < total;
  const filtersActive = !isFiltersEmpty(filters);

  const chipFailedOcrActive = filters.ocr_status.includes('failed');
  const chipWaitingReviewActive = filters.run_status.includes('waiting_review');
  const chipHasErrorActive = filters.has_error === true;
  const chipMissingMetadataActive =
    filters.metadata_status.length > 0 && filters.metadata_status.every((s) => s === 'queued' || s === 'unknown');

  return (
    <section className="page">
      <PageHeader title={t('inventory.title')} />

      <div className="toolbar inventory-search-bar">
        <div className="inventory-search-input">
          <Search size={16} />
          <input
            type="text"
            placeholder={t('inventory.search_placeholder')}
            value={searchText}
            onChange={(event) => setSearchText(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') commitSearch();
            }}
          />
          <button className="chip-button" onClick={commitSearch}>{t('inventory.search_apply')}</button>
        </div>
        <ActionButton icon={<RefreshCw />} label={t('generic.reload')} busy={busy} onClick={() => run(setBusy, setError, loadFirst, t)} />
        <small className="field-hint">{t('inventory.count_label', { shown: items.length, total })}</small>
      </div>

      <div className="toolbar inventory-chips">
        <ChipButton
          label={t('inventory.chip.failed_ocr')}
          active={chipFailedOcrActive}
          onClick={() => toggleChip({ ocr_status: ['failed'] }, chipFailedOcrActive)}
        />
        <ChipButton
          label={t('inventory.chip.waiting_review')}
          active={chipWaitingReviewActive}
          onClick={() => toggleChip({ run_status: ['waiting_review'] }, chipWaitingReviewActive)}
        />
        <ChipButton
          label={t('inventory.chip.has_error')}
          active={chipHasErrorActive}
          onClick={() => toggleChip({ has_error: true }, chipHasErrorActive)}
        />
        <ChipButton
          label={t('inventory.chip.missing_metadata')}
          active={chipMissingMetadataActive}
          onClick={() => toggleChip({ metadata_status: ['queued', 'unknown'] }, chipMissingMetadataActive)}
        />
        <button className={`chip-button${advancedOpen ? ' active' : ''}`} onClick={() => setAdvancedOpen((open) => !open)}>
          <Filter size={14} /> {t('inventory.advanced_toggle')}
        </button>
        <button className={`chip-button${duplicatesOpen ? ' active' : ''}`} onClick={() => setDuplicatesOpen((open) => !open)}>
          <Copy size={14} /> {t('inventory.duplicates_toggle')}
        </button>
        {filtersActive && (
          <button className="chip-button" onClick={clearAll}>
            <X size={14} /> {t('inventory.clear_filters')}
          </button>
        )}
      </div>

      {advancedOpen && (
        <AdvancedPanel filters={filters} setFilters={setFilters} languages={languages} t={t} />
      )}

      {duplicatesOpen && <DuplicatesPanel setError={setError} t={t} />}

      <div className="toolbar inventory-selection-bar">
        <button
          className="primary-button"
          disabled={busy || selected.size === 0}
          onClick={() => run(setBusy, setError, rerunSelected, t)}
        >
          <RotateCcw size={16} /> {t('inventory.rerun_selected')}
        </button>
        <small className="field-hint">{t('inventory.selected_count', { count: selected.size })}</small>
        {selected.size > 0 && (
          <button className="chip-button" onClick={() => setSelected(new Set())}>
            <X size={14} /> {t('inventory.clear_selection')}
          </button>
        )}
        {notice && <small className="field-hint">{notice}</small>}
      </div>

      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th className="select-col">
                <input
                  type="checkbox"
                  checked={allOnPageSelected}
                  onChange={toggleSelectAll}
                  aria-label={t('inventory.select_all')}
                />
              </th>
              <th>{t('inventory.id')}</th>
              <th>{t('inventory.document_title')}</th>
              <th>{t('inventory.ocr')}</th>
              <th>{t('inventory.metadata')}</th>
              <th>{t('inventory.language')}</th>
              <th>{t('inventory.tags')}</th>
              <th>{t('inventory.date')}</th>
              <th>{t('inventory.run')}</th>
              <th>{t('inventory.debug')}</th>
              <th>{t('inventory.actions')}</th>
            </tr>
          </thead>
          <tbody>
            {items.length === 0 ? (
              <tr>
                <td colSpan={11} className="empty-row">
                  {t('inventory.no_results')}
                </td>
              </tr>
            ) : (
              items.map((item) => (
                <InventoryRow
                  key={item.paperless_document_id}
                  item={item}
                  selected={selected.has(item.paperless_document_id)}
                  onToggleSelect={toggleSelect}
                  languages={languages}
                  t={t}
                  onTriggerOcr={triggerOcr}
                  onTriggerMetadata={triggerMetadata}
                  onTriggerPipeline={triggerPipeline}
                  onDiagnose={openDiagnose}
                />
              ))
            )}
          </tbody>
        </table>
      </div>
      {hasMore && (
        <div className="toolbar">
          <ActionButton
            icon={<RefreshCw />}
            label={t('inventory.load_more')}
            busy={busy}
            onClick={() => run(setBusy, setError, loadMore, t)}
          />
        </div>
      )}
      {diagnoseDocumentId != null && (
        <DiagnoseDrawer
          documentId={diagnoseDocumentId}
          trace={diagnoseTrace}
          busy={diagnoseBusy}
          missing={diagnoseMissing}
          onClose={closeDiagnose}
        />
      )}
    </section>
  );
}

function ChipButton({ label, active, onClick }: { label: string; active: boolean; onClick: () => void }) {
  return (
    <button className={`chip-button${active ? ' active' : ''}`} onClick={onClick}>
      {label}
    </button>
  );
}

// Read-only dedup view (#216): lists groups of documents sharing the same OCR
// content hash, each member linking out to its Paperless detail page. Fetches
// its own data when first rendered so the cost is only paid when the operator
// opens the panel.
function DuplicatesPanel({ setError, t }: { setError: (error: string | null) => void; t: TFunction }) {
  const [groups, setGroups] = useState<DuplicateGroup[] | null>(null);
  const [paperlessBase, setPaperlessBase] = useState<string>('');
  const [busy, setBusy] = useState(false);

  const load = useCallback(() => {
    setBusy(true);
    return api
      .inventoryDuplicates()
      .then((data) => {
        setGroups(data.groups);
        setPaperlessBase(data.paperless_base);
      })
      .catch((err) => setError(localizedErrorMessage(err, t)))
      .finally(() => setBusy(false));
  }, [setError, t]);

  useEffect(() => {
    void load();
  }, [load]);

  const documentUrl = (id: number) =>
    paperlessBase ? `${paperlessBase}/documents/${id}/details` : null;

  return (
    <div className="advanced-panel duplicates-panel">
      <div className="toolbar">
        <strong>{t('inventory.duplicates_title')}</strong>
        <ActionButton icon={<RefreshCw />} label={t('generic.reload')} busy={busy} onClick={load} />
        {groups != null && (
          <small className="field-hint">{t('inventory.duplicates_group_count', { count: groups.length })}</small>
        )}
      </div>
      {groups != null && groups.length === 0 && (
        <p className="field-hint">{t('inventory.duplicates_empty')}</p>
      )}
      {groups?.map((group) => (
        <div key={group.hash} className="duplicates-group">
          <div className="field-hint">
            {t('inventory.duplicates_hash', { hash: group.hash.slice(0, 12) })} ·{' '}
            {t('inventory.duplicates_doc_count', { count: group.documents.length })}
          </div>
          <ul>
            {group.documents.map((doc) => {
              const url = documentUrl(doc.paperless_document_id);
              const label = `#${doc.paperless_document_id} ${doc.title ?? t('inventory.untitled')}`;
              return (
                <li key={doc.paperless_document_id}>
                  {url ? (
                    <a href={url} target="_blank" rel="noreferrer">
                      <ExternalLink size={14} /> {label}
                    </a>
                  ) : (
                    label
                  )}
                </li>
              );
            })}
          </ul>
        </div>
      ))}
    </div>
  );
}

type AdvancedPanelProps = {
  filters: Filters;
  setFilters: React.Dispatch<React.SetStateAction<Filters>>;
  languages: ReturnType<typeof languageOptions>;
  t: TFunction;
};

function AdvancedPanel({ filters, setFilters, languages, t }: AdvancedPanelProps) {
  const toggleStatus = (group: 'ocr_status' | 'metadata_status' | 'run_status', value: string) => {
    setFilters((f) => {
      const current = f[group];
      const next = current.includes(value) ? current.filter((s) => s !== value) : [...current, value];
      return { ...f, [group]: next };
    });
  };
  return (
    <div className="advanced-filter-panel">
      <fieldset>
        <legend>{t('inventory.filter.ocr_status')}</legend>
        <div className="checkbox-row">
          {STATUS_OPTIONS.map((status) => (
            <label key={`ocr-${status}`} className="inline">
              <input
                type="checkbox"
                checked={filters.ocr_status.includes(status)}
                onChange={() => toggleStatus('ocr_status', status)}
              />
              <span>{status}</span>
            </label>
          ))}
        </div>
      </fieldset>
      <fieldset>
        <legend>{t('inventory.filter.metadata_status')}</legend>
        <div className="checkbox-row">
          {STATUS_OPTIONS.map((status) => (
            <label key={`meta-${status}`} className="inline">
              <input
                type="checkbox"
                checked={filters.metadata_status.includes(status)}
                onChange={() => toggleStatus('metadata_status', status)}
              />
              <span>{status}</span>
            </label>
          ))}
        </div>
      </fieldset>
      <fieldset>
        <legend>{t('inventory.filter.run_status')}</legend>
        <div className="checkbox-row">
          {RUN_STATUS_OPTIONS.map((status) => (
            <label key={`run-${status}`} className="inline">
              <input
                type="checkbox"
                checked={filters.run_status.includes(status)}
                onChange={() => toggleStatus('run_status', status)}
              />
              <span>{status}</span>
            </label>
          ))}
        </div>
      </fieldset>
      <label>
        {t('inventory.filter.tags_include')}
        <input
          type="text"
          value={filters.tags_include.join(', ')}
          onChange={(event) =>
            setFilters((f) => ({
              ...f,
              tags_include: event.target.value.split(',').map((s) => s.trim()).filter(Boolean),
            }))
          }
        />
      </label>
      <label>
        {t('inventory.filter.tags_exclude')}
        <input
          type="text"
          value={filters.tags_exclude.join(', ')}
          onChange={(event) =>
            setFilters((f) => ({
              ...f,
              tags_exclude: event.target.value.split(',').map((s) => s.trim()).filter(Boolean),
            }))
          }
        />
      </label>
      <label>
        {t('inventory.filter.language')}
        <select
          value={filters.language ?? ''}
          onChange={(event) =>
            setFilters((f) => ({ ...f, language: event.target.value || undefined }))
          }
        >
          <option value="">{t('inventory.filter.any')}</option>
          {languages.map((lang) => (
            <option key={lang.tag} value={lang.tag}>{lang.uiName}</option>
          ))}
        </select>
      </label>
      <label>
        {t('inventory.filter.date_from')}
        <input
          type="date"
          value={filters.date_from ?? ''}
          onChange={(event) => setFilters((f) => ({ ...f, date_from: event.target.value || undefined }))}
        />
      </label>
      <label>
        {t('inventory.filter.date_to')}
        <input
          type="date"
          value={filters.date_to ?? ''}
          onChange={(event) => setFilters((f) => ({ ...f, date_to: event.target.value || undefined }))}
        />
      </label>
      <label className="inline">
        <input
          type="checkbox"
          checked={filters.has_error === true}
          onChange={(event) =>
            setFilters((f) => ({ ...f, has_error: event.target.checked ? true : undefined }))
          }
        />
        <span>{t('inventory.filter.has_error')}</span>
      </label>
      <label className="inline">
        <input
          type="checkbox"
          checked={filters.needs_review === true}
          onChange={(event) =>
            setFilters((f) => ({ ...f, needs_review: event.target.checked ? true : undefined }))
          }
        />
        <span>{t('inventory.filter.needs_review')}</span>
      </label>
    </div>
  );
}

type InventoryRowProps = {
  item: InventoryItem;
  selected: boolean;
  onToggleSelect: (documentId: number) => void;
  languages: ReturnType<typeof languageOptions>;
  t: TFunction;
  onTriggerOcr: (documentId: number) => void;
  onTriggerMetadata: (documentId: number) => void;
  onTriggerPipeline: (documentId: number) => void;
  onDiagnose: (documentId: number) => void;
};

const InventoryRow = memo(
  function InventoryRow({ item, selected, onToggleSelect, languages, t, onTriggerOcr, onTriggerMetadata, onTriggerPipeline, onDiagnose }: InventoryRowProps) {
    return (
      // `content-visibility: auto` lets the browser skip layout/paint for rows
      // outside the viewport, keeping a long (load-more) list cheap to render.
      <tr style={{ contentVisibility: 'auto', containIntrinsicSize: 'auto 41px' }}>
        <td className="select-col">
          <input
            type="checkbox"
            checked={selected}
            onChange={() => onToggleSelect(item.paperless_document_id)}
            aria-label={t('inventory.select_row', { id: item.paperless_document_id })}
          />
        </td>
        <td>{item.paperless_document_id}</td>
        <td>{item.title || item.original_file_name || t('inventory.untitled')}</td>
        <td><Status value={item.ocr_status} /></td>
        <td><Status value={item.metadata_status} /></td>
        <td>{formatLanguageDetection(item, languages)}</td>
        <td>{item.current_tags && item.current_tags.length > 0 ? item.current_tags.join(', ') : '-'}</td>
        <td>{item.document_date ?? '-'}</td>
        <td>{item.current_run_status || '-'}</td>
        <td><DebugContextDetails context={item.debug_context} compact /></td>
        <td className="row-actions">
          <button title={t('inventory.trigger_ocr')} onClick={() => onTriggerOcr(item.paperless_document_id)}>
            <FileText size={16} />
          </button>
          <button title={t('inventory.trigger_metadata')} onClick={() => onTriggerMetadata(item.paperless_document_id)}>
            <Tags size={16} />
          </button>
          <button title={t('inventory.trigger_pipeline')} onClick={() => onTriggerPipeline(item.paperless_document_id)}>
            <Sparkles size={16} />
          </button>
          <button title={t('inventory.diagnose.button')} onClick={() => onDiagnose(item.paperless_document_id)}>
            <Stethoscope size={16} />
          </button>
        </td>
      </tr>
    );
  },
  (prev, next) => {
    if (prev.t !== next.t) return false;
    if (prev.selected !== next.selected) return false;
    if (prev.onToggleSelect !== next.onToggleSelect) return false;
    if (prev.languages !== next.languages) return false;
    if (prev.onTriggerOcr !== next.onTriggerOcr) return false;
    if (prev.onTriggerMetadata !== next.onTriggerMetadata) return false;
    if (prev.onTriggerPipeline !== next.onTriggerPipeline) return false;
    if (prev.onDiagnose !== next.onDiagnose) return false;
    const a = prev.item;
    const b = next.item;
    return (
      a.paperless_document_id === b.paperless_document_id &&
      a.title === b.title &&
      a.original_file_name === b.original_file_name &&
      a.ocr_status === b.ocr_status &&
      a.metadata_status === b.metadata_status &&
      a.current_tags === b.current_tags &&
      a.document_date === b.document_date &&
      a.current_run_status === b.current_run_status &&
      a.detected_language === b.detected_language &&
      a.detected_language_confidence === b.detected_language_confidence &&
      a.debug_context === b.debug_context
    );
  }
);

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

function DiagnoseDrawer({ documentId, trace, busy, missing, onClose }: DiagnoseDrawerProps) {
  const { t, formatRelativeTime } = useI18n();
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
    <div className="drawer-root" role="dialog" aria-modal="true" aria-label={t('inventory.diagnose.title', { id: documentId })}>
      <div className="drawer-backdrop" onClick={onClose} />
      <aside className="drawer diagnose-drawer">
        <header>
          <strong>{t('inventory.diagnose.title', { id: documentId })}</strong>
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
