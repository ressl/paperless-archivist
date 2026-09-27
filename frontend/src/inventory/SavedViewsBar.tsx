import { useState, type FormEvent } from 'react';
import { Bookmark, Download, Save, Trash2, X } from 'lucide-react';
import { api, isApiError, type InventorySavedView } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { useConfirm } from '../lib/ConfirmDialog';
import { localizedErrorMessage } from '../lib/ui';
import { useResource } from '../lib/useResource';
import { filtersToParams, filtersToUrl, parseFiltersFromSearch, type Filters } from './types';

type SavedViewsBarProps = {
  filters: Filters;
  /** Replace the active filters with a saved view's filters. */
  applyFilters: (filters: Filters) => void;
  setError: (error: string | null) => void;
};

/**
 * Saved inventory views and the filtered export (#447).
 *
 * Views are stored server-side per user (`/api/inventory/views`), so they
 * survive reloads and follow the user across browsers; the active filters
 * themselves also live in the URL. An API error on the initial list load
 * (e.g. an API-token session, which has no personal views) only hides the
 * picker instead of raising a page-level error.
 */
export function SavedViewsBar({ filters, applyFilters, setError }: SavedViewsBarProps) {
  const { t } = useI18n();
  const { confirm, dialog: confirmDialog } = useConfirm();
  const views = useResource((signal) => api.inventoryViews({ signal }), []);
  const [selectedId, setSelectedId] = useState('');
  const [naming, setNaming] = useState(false);
  const [name, setName] = useState('');
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const items: InventorySavedView[] = views.data?.items ?? [];
  const selected = items.find((view) => view.id === selectedId) ?? null;
  const currentQuery = filtersToUrl(filters);
  const params = filtersToParams(filters);

  const selectView = (id: string) => {
    setSelectedId(id);
    setNotice(null);
    const view = items.find((item) => item.id === id);
    if (view) applyFilters(parseFiltersFromSearch(view.query));
  };

  const perform = async (action: () => Promise<void>) => {
    setBusy(true);
    setNotice(null);
    try {
      await action();
    } catch (err) {
      setError(localizedErrorMessage(err, t));
    } finally {
      setBusy(false);
    }
  };

  const saveNew = (event: FormEvent) => {
    event.preventDefault();
    const trimmed = name.trim();
    if (!trimmed) return;
    void perform(async () => {
      const view = await api.createInventoryView(trimmed, currentQuery);
      await views.reload();
      setSelectedId(view.id);
      setNaming(false);
      setName('');
      setNotice(t('inventory.views.saved', { name: view.name }));
    });
  };

  const updateSelected = () => {
    if (!selected) return;
    void perform(async () => {
      const view = await api.updateInventoryView(selected.id, selected.name, currentQuery);
      await views.reload();
      setNotice(t('inventory.views.updated', { name: view.name }));
    });
  };

  const deleteSelected = async () => {
    if (!selected) return;
    const confirmed = await confirm({
      title: t('inventory.views.delete_confirm.title', { name: selected.name }),
      description: t('inventory.views.delete_confirm.description'),
      confirmLabel: t('inventory.views.delete')
    });
    if (!confirmed) return;
    await perform(async () => {
      try {
        await api.deleteInventoryView(selected.id);
      } catch (err) {
        // Already gone (e.g. deleted in another tab): just refresh the list.
        if (!(isApiError(err) && err.status === 404)) throw err;
      }
      setSelectedId('');
      await views.reload();
    });
  };

  const viewsAvailable = views.error == null;

  return (
    <div className="toolbar inventory-views-bar">
      {viewsAvailable && (
        <>
          <label className="inventory-views-select">
            <Bookmark size={16} aria-hidden="true" />
            <select
              aria-label={t('inventory.views.label')}
              value={selectedId}
              disabled={busy}
              onChange={(event) => selectView(event.target.value)}
            >
              <option value="">{t('inventory.views.placeholder')}</option>
              {items.map((view) => (
                <option key={view.id} value={view.id}>{view.name}</option>
              ))}
            </select>
          </label>
          {naming ? (
            <form className="inventory-views-form" onSubmit={saveNew}>
              <input
                type="text"
                value={name}
                maxLength={80}
                autoFocus
                aria-label={t('inventory.views.name')}
                placeholder={t('inventory.views.name')}
                onChange={(event) => setName(event.target.value)}
              />
              <button type="submit" className="chip-button" disabled={busy || !name.trim()}>
                <Save size={14} aria-hidden="true" /> {t('generic.save')}
              </button>
              <button type="button" className="chip-button" onClick={() => setNaming(false)}>
                <X size={14} aria-hidden="true" /> {t('generic.cancel')}
              </button>
            </form>
          ) : (
            <button type="button" className="chip-button" disabled={busy} onClick={() => setNaming(true)}>
              <Save size={14} aria-hidden="true" /> {t('inventory.views.save')}
            </button>
          )}
          {selected && (
            <>
              <button type="button" className="chip-button" disabled={busy} onClick={updateSelected}>
                {t('inventory.views.update')}
              </button>
              <button type="button" className="chip-button" disabled={busy} onClick={() => void deleteSelected()}>
                <Trash2 size={14} aria-hidden="true" /> {t('inventory.views.delete')}
              </button>
            </>
          )}
        </>
      )}
      <a className="button-link" href={api.inventoryExportUrl(params, 'csv')}>
        <Download size={16} aria-hidden="true" /> {t('inventory.export_csv')}
      </a>
      <a className="button-link" href={api.inventoryExportUrl(params, 'json')}>
        <Download size={16} aria-hidden="true" /> {t('inventory.export_json')}
      </a>
      <small className="field-hint inline-notice" role="status" aria-live="polite">{notice}</small>
      {confirmDialog}
    </div>
  );
}
