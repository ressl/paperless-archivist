import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { MessageSquare, RotateCcw, X } from 'lucide-react';
import {
  api,
  isApiError,
  type InventoryItem,
  type MetadataTrace,
  type Stage,
} from '../api/client';
import { languageOptions } from '../data/worldLanguages';
import { replaceLocation } from '../lib/router';
import { useI18n } from '../i18n/I18nProvider';
import { PageHeader, localizedErrorMessage, run } from '../lib/ui';
import { useConfirm } from '../lib/ConfirmDialog';
import { useResource } from '../lib/useResource';
import { AdvancedPanel, type InventoryFacets } from './AdvancedPanel';
import { DiagnoseDrawer } from './DiagnoseDrawer';
import { DuplicatesPanel } from './DuplicatesPanel';
import { InventoryFiltersBar } from './InventoryFiltersBar';
import { InventoryPagination } from './InventoryPagination';
import { InventoryTable } from './InventoryTable';
import { SavedViewsBar } from './SavedViewsBar';
import {
  PAGE_SIZE,
  RERUN_STAGES,
  filtersToParams,
  filtersToUrl,
  parseFiltersFromUrl,
  type Filters,
} from './types';

/** The chat accepts at most this many document ids as a filter (#449). */
const MAX_CHAT_DOCUMENTS = 50;

export function Inventory({
  setError,
  onAskInChat
}: {
  setError: (error: string | null) => void;
  /** Open the chat scoped to these documents; omitted without chat permission (#449). */
  onAskInChat?: (documentIds: number[]) => void;
}) {
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
  // Mirrors diagnoseDocumentId so the async trace fetch can detect that the
  // user opened a different document while it was in flight. (#272)
  const diagnoseDocumentIdRef = useRef<number | null>(null);
  const [diagnoseTrace, setDiagnoseTrace] = useState<MetadataTrace | null>(null);
  const [diagnoseBusy, setDiagnoseBusy] = useState(false);
  const [diagnoseMissing, setDiagnoseMissing] = useState(false);
  const [selected, setSelected] = useState<Set<number>>(() => new Set());
  const [notice, setNotice] = useState<string | null>(null);
  // #429: a failed first-page load is shown as an error state, not "no results".
  const [loadError, setLoadError] = useState<string | null>(null);
  // #426: per-row pending state; the ref blocks a double click synchronously.
  const rowActionsInFlight = useRef(new Set<number>());
  const [pendingRows, setPendingRows] = useState<ReadonlySet<number>>(() => new Set());
  const { confirm, dialog: confirmDialog } = useConfirm();
  const languages = useMemo(() => languageOptions(locale), [locale]);
  // #447: correspondent / document type vocabularies for the filter pickers,
  // from the synced Paperless mirror. Optional: on failure the pickers just
  // offer "any" / "not set".
  const facets = useResource(
    (signal): Promise<InventoryFacets> =>
      Promise.all([api.paperlessCorrespondents({ signal }), api.paperlessDocumentTypes({ signal })]).then(
        ([correspondents, documentTypes]) => ({
          correspondents: correspondents.items,
          document_types: documentTypes.items
        })
      ),
    []
  );

  // #447: apply a saved view — replace the filters and the search box text.
  const applyFilters = useCallback((next: Filters) => {
    setSearchText(next.id != null ? String(next.id) : (next.q ?? ''));
    setFilters(next);
  }, []);

  const visibleDocumentIds = useMemo(
    () => new Set(items.map((item) => item.paperless_document_id)),
    [items]
  );
  const visibleSelected = useMemo(
    () => new Set(Array.from(selected).filter((documentId) => visibleDocumentIds.has(documentId))),
    [selected, visibleDocumentIds]
  );

  // Sync filters → URL whenever filters change.
  const urlSyncRef = useRef(true);
  useEffect(() => {
    if (!urlSyncRef.current) return;
    const next = filtersToUrl(filters);
    const current = window.location.search;
    if (next !== current) {
      // Through the router so the app shell sees the new query string. (#424)
      replaceLocation(`${window.location.pathname}${next}`);
    }
  }, [filters]);

  // Monotonic request id: a slower earlier response (e.g. after a fast filter
  // change) must never overwrite the result of a newer in-flight request.
  const requestIdRef = useRef(0);

  const loadFirst = useCallback(() => {
    const requestId = ++requestIdRef.current;
    // A first-page load replaces the query result. Remove the old snapshot
    // immediately so it cannot be selected or paginated while refresh is in
    // flight.
    setSelected(new Set());
    setItems([]);
    setTotal(0);
    setLoading(true);
    setLoadError(null);
    return api
      .inventory({ ...filtersToParams(filters), offset: 0, limit: PAGE_SIZE })
      .then((data) => {
        if (requestId !== requestIdRef.current) return;
        setItems(data.items);
        setTotal(data.total);
        // The operator may have selected an old row while the refresh was in
        // flight. The replacing response defines a new selection context.
        setSelected(new Set());
      })
      .catch((err) => {
        if (requestId !== requestIdRef.current) return;
        const message = localizedErrorMessage(err, t);
        setLoadError(message);
        setError(message);
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
    // Query changes are result boundaries. Invalidate requests and remove the
    // old snapshot before the debounced first-page request starts; otherwise
    // an old load-more response could overtake it and append across contexts.
    ++requestIdRef.current;
    setSelected(new Set());
    setItems([]);
    setTotal(0);
    setLoading(true);
    // Debounce filter-driven reloads so rapid changes (date pickers, toggles)
    // collapse into one 500-row query instead of firing per change. The
    // request-id guard in loadFirst still protects against out-of-order
    // responses. (#277)
    const handle = window.setTimeout(() => {
      void loadFirst();
    }, 300);
    return () => window.clearTimeout(handle);
  }, [filters, loadFirst]);

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

  // Re-fetch just one document and patch it into the loaded list, keeping
  // every loaded page, the selection and the scroll position (#426).
  const refreshRow = useCallback(async (documentId: number) => {
    const contextId = requestIdRef.current;
    try {
      const data = await api.inventory({ id: documentId, offset: 0, limit: 1 });
      // A query change in the meantime replaced the list; do not patch it.
      if (contextId !== requestIdRef.current) return;
      const fresh = data.items.find((item) => item.paperless_document_id === documentId);
      if (!fresh) return;
      setItems((current) =>
        current.map((item) => (item.paperless_document_id === documentId ? fresh : item))
      );
    } catch {
      // The trigger itself succeeded; a failed row refresh keeps the old row.
    }
  }, []);

  const triggerRow = useCallback(
    async (documentId: number, stages: Stage[]) => {
      if (rowActionsInFlight.current.has(documentId)) return;
      rowActionsInFlight.current.add(documentId);
      setPendingRows(new Set(rowActionsInFlight.current));
      setNotice(null);
      try {
        await api.triggerDocument(documentId, stages, 'manual_review');
        setNotice(t('inventory.trigger_queued', { id: documentId }));
        await refreshRow(documentId);
      } catch (err) {
        setError(localizedErrorMessage(err, t));
      } finally {
        rowActionsInFlight.current.delete(documentId);
        setPendingRows(new Set(rowActionsInFlight.current));
      }
    },
    [refreshRow, setError, t]
  );

  const triggerOcr = useCallback((documentId: number) => void triggerRow(documentId, ['ocr']), [triggerRow]);
  const triggerMetadata = useCallback((documentId: number) => void triggerRow(documentId, ['metadata']), [triggerRow]);
  const triggerPipeline = useCallback(
    (documentId: number) => void triggerRow(documentId, ['ocr', 'metadata']),
    [triggerRow]
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

  const allOnPageSelected = items.length > 0 && items.every((item) => visibleSelected.has(item.paperless_document_id));

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

  // Refresh the rows that are already loaded in place (same range), without
  // emptying the list first, so pages and scroll position survive (#426).
  const refreshLoaded = useCallback(() => {
    const requestId = ++requestIdRef.current;
    return api
      .inventory({ ...filtersToParams(filters), offset: 0, limit: Math.max(items.length, PAGE_SIZE) })
      .then((data) => {
        if (requestId !== requestIdRef.current) return;
        setItems(data.items);
        setTotal(data.total);
      })
      .catch((err) => {
        if (requestId !== requestIdRef.current) return;
        setError(localizedErrorMessage(err, t));
      });
  }, [filters, items.length, setError, t]);

  const rerunSelected = useCallback(async () => {
    // Only IDs in the rendered result may reach the bulk endpoint, even if a
    // future state-management regression leaves a hidden ID in `selected`.
    const ids = Array.from(visibleSelected);
    if (ids.length === 0) return;
    // #417: bulk re-run queues AI work for every selected document.
    const confirmed = await confirm({
      title: t('inventory.rerun_confirm.title', { count: ids.length }),
      description: t('inventory.rerun_confirm.description', { count: ids.length }),
      confirmLabel: t('inventory.rerun_selected'),
      tone: 'default'
    });
    if (!confirmed) return;
    setNotice(null);
    await run(setBusy, setError, async () => {
      const result = await api.bulkRerun(ids, RERUN_STAGES);
      setSelected(new Set());
      setNotice(t('inventory.rerun_done', { count: result.queued }));
      await refreshLoaded();
    }, t);
  }, [visibleSelected, confirm, refreshLoaded, setError, t]);

  const openDiagnose = useCallback(
    async (documentId: number) => {
      diagnoseDocumentIdRef.current = documentId;
      setDiagnoseDocumentId(documentId);
      setDiagnoseTrace(null);
      setDiagnoseMissing(false);
      setDiagnoseBusy(true);
      try {
        const trace = await api.inventoryMetadataTrace(documentId);
        // Opening diagnose for B while A's slower trace is still in flight must
        // not render A's trace under B's header. (#272)
        if (diagnoseDocumentIdRef.current !== documentId) return;
        setDiagnoseTrace(trace);
      } catch (err) {
        if (diagnoseDocumentIdRef.current !== documentId) return;
        // The trace endpoint answers 404 when the document has no metadata run yet. (#432)
        if (isApiError(err) && err.status === 404) {
          setDiagnoseMissing(true);
        } else {
          setError(localizedErrorMessage(err, t));
          setDiagnoseDocumentId(null);
        }
      } finally {
        if (diagnoseDocumentIdRef.current === documentId) {
          setDiagnoseBusy(false);
        }
      }
    },
    [setError, t]
  );

  const closeDiagnose = useCallback(() => {
    diagnoseDocumentIdRef.current = null;
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

      <SavedViewsBar filters={filters} applyFilters={applyFilters} setError={setError} />

      {advancedOpen && (
        <AdvancedPanel filters={filters} setFilters={setFilters} languages={languages} facets={facets.data ?? null} />
      )}

      {duplicatesOpen && <DuplicatesPanel setError={setError} />}

      <div className="toolbar inventory-selection-bar">
        <button
          className="primary-button"
          disabled={busy || visibleSelected.size === 0}
          onClick={() => void rerunSelected()}
        >
          <RotateCcw size={16} /> {t('inventory.rerun_selected')}
        </button>
        {onAskInChat && (
          <button
            className="secondary-button"
            disabled={visibleSelected.size === 0 || visibleSelected.size > MAX_CHAT_DOCUMENTS}
            title={t('inventory.ask_in_chat_hint', { max: MAX_CHAT_DOCUMENTS })}
            onClick={() => onAskInChat(Array.from(visibleSelected).sort((a, b) => a - b))}
          >
            <MessageSquare size={16} /> {t('inventory.ask_in_chat')}
          </button>
        )}
        <small className="field-hint">{t('inventory.selected_count', { count: visibleSelected.size })}</small>
        {visibleSelected.size > 0 && (
          <button className="chip-button" onClick={() => setSelected(new Set())}>
            <X size={14} /> {t('inventory.clear_selection')}
          </button>
        )}
        <small className="field-hint inline-notice" role="status" aria-live="polite">{notice}</small>
      </div>

      <InventoryTable
        items={items}
        loading={loading}
        loadError={loadError}
        onRetry={() => void loadFirst()}
        pendingRows={pendingRows}
        selected={visibleSelected}
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
      {confirmDialog}
    </section>
  );
}
