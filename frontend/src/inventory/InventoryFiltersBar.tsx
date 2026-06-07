import { Copy, Filter, RefreshCw, Search, X } from 'lucide-react';
import { useI18n } from '../i18n/I18nProvider';
import { ActionButton, run } from '../lib/ui';
import { EMPTY_FILTERS, isFiltersEmpty, type Filters } from './types';

function ChipButton({ label, active, onClick }: { label: string; active: boolean; onClick: () => void }) {
  return (
    <button className={`chip-button${active ? ' active' : ''}`} aria-pressed={active} onClick={onClick}>
      {label}
    </button>
  );
}

type InventoryFiltersBarProps = {
  filters: Filters;
  setFilters: React.Dispatch<React.SetStateAction<Filters>>;
  searchText: string;
  setSearchText: (value: string) => void;
  commitSearch: () => void;
  busy: boolean;
  setBusy: (value: boolean) => void;
  setError: (error: string | null) => void;
  reload: () => Promise<unknown> | unknown;
  shown: number;
  total: number;
  advancedOpen: boolean;
  setAdvancedOpen: React.Dispatch<React.SetStateAction<boolean>>;
  duplicatesOpen: boolean;
  setDuplicatesOpen: React.Dispatch<React.SetStateAction<boolean>>;
};

export function InventoryFiltersBar({
  filters,
  setFilters,
  searchText,
  setSearchText,
  commitSearch,
  busy,
  setBusy,
  setError,
  reload,
  shown,
  total,
  advancedOpen,
  setAdvancedOpen,
  duplicatesOpen,
  setDuplicatesOpen,
}: InventoryFiltersBarProps) {
  const { t } = useI18n();

  const toggleChip = (patch: Partial<Filters>, active: boolean) => {
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
  };

  const clearAll = () => {
    setSearchText('');
    setFilters(EMPTY_FILTERS);
  };

  const filtersActive = !isFiltersEmpty(filters);
  const chipFailedOcrActive = filters.ocr_status.includes('failed');
  const chipWaitingReviewActive = filters.run_status.includes('waiting_review');
  const chipHasErrorActive = filters.has_error === true;
  const chipMissingMetadataActive =
    filters.metadata_status.length > 0 && filters.metadata_status.every((s) => s === 'queued' || s === 'unknown');

  return (
    <>
      <div className="toolbar inventory-search-bar" role="search">
        <div className="inventory-search-input">
          <Search size={16} aria-hidden="true" />
          <input
            type="text"
            placeholder={t('inventory.search_placeholder')}
            aria-label={t('inventory.search_placeholder')}
            value={searchText}
            onChange={(event) => setSearchText(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') commitSearch();
            }}
          />
          <button className="chip-button" onClick={commitSearch}>{t('inventory.search_apply')}</button>
        </div>
        <ActionButton icon={<RefreshCw />} label={t('generic.reload')} busy={busy} onClick={() => run(setBusy, setError, reload, t)} />
        <small className="field-hint">{t('inventory.count_label', { shown, total })}</small>
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
        <button
          className={`chip-button${advancedOpen ? ' active' : ''}`}
          aria-pressed={advancedOpen}
          onClick={() => setAdvancedOpen((open) => !open)}
        >
          <Filter size={14} /> {t('inventory.advanced_toggle')}
        </button>
        <button
          className={`chip-button${duplicatesOpen ? ' active' : ''}`}
          aria-pressed={duplicatesOpen}
          onClick={() => setDuplicatesOpen((open) => !open)}
        >
          <Copy size={14} /> {t('inventory.duplicates_toggle')}
        </button>
        {filtersActive && (
          <button className="chip-button" onClick={clearAll}>
            <X size={14} /> {t('inventory.clear_filters')}
          </button>
        )}
      </div>
    </>
  );
}
