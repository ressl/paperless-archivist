import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { RotateCcw, X } from 'lucide-react';
import {
  api,
  type InventoryItem,
  type MetadataTrace,
} from '../api/client';
import { languageOptions } from '../data/worldLanguages';
import { useI18n } from '../i18n/I18nProvider';
import { PageHeader, localizedErrorMessage, run } from '../lib/ui';
import { AdvancedPanel } from './AdvancedPanel';
import { DiagnoseDrawer } from './DiagnoseDrawer';
import { DuplicatesPanel } from './DuplicatesPanel';
import { InventoryFiltersBar } from './InventoryFiltersBar';
import { InventoryPagination } from './InventoryPagination';
import { InventoryTable } from './InventoryTable';
import {
  PAGE_SIZE,
  RERUN_STAGES,
  filtersToParams,
  filtersToUrl,
  parseFiltersFromUrl,
  type Filters,
} from './types';

export function Inventory({ setError }: { setError: (error: string | null) => void }) {
  const { t, locale } = useI18n();
  const [items, setItems] = useState<InventoryItem[]>([]);
  const [total, setTotal] = useState<number>(0);
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(true);
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
    setLoading(true);
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
      })
      .finally(() => {
        // Only the newest in-flight request may clear the loading flag, so a
        // stale earlier response can't hide the spinner for a pending newer one.
        if (requestId === requestIdRef.current) setLoading(false);
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

  return (
    <section className="page">
      <PageHeader title={t('inventory.title')} />

      <InventoryFiltersBar
        filters={filters}
        setFilters={setFilters}
        searchText={searchText}
        setSearchText={setSearchText}
        commitSearch={commitSearch}
        busy={busy}
        setBusy={setBusy}
        setError={setError}
        reload={loadFirst}
        shown={items.length}
        total={total}
        advancedOpen={advancedOpen}
        setAdvancedOpen={setAdvancedOpen}
        duplicatesOpen={duplicatesOpen}
        setDuplicatesOpen={setDuplicatesOpen}
      />

      {advancedOpen && (
        <AdvancedPanel filters={filters} setFilters={setFilters} languages={languages} />
      )}

      {duplicatesOpen && <DuplicatesPanel setError={setError} />}

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

      <InventoryTable
        items={items}
        loading={loading}
        selected={selected}
        allOnPageSelected={allOnPageSelected}
        onToggleSelect={toggleSelect}
        onToggleSelectAll={toggleSelectAll}
        languages={languages}
        onTriggerOcr={triggerOcr}
        onTriggerMetadata={triggerMetadata}
        onTriggerPipeline={triggerPipeline}
        onDiagnose={openDiagnose}
      />

      <InventoryPagination
        hasMore={hasMore}
        busy={busy}
        setBusy={setBusy}
        setError={setError}
        loadMore={loadMore}
      />

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
