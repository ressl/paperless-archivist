import type { InventoryFacetValue, InventoryFacets } from '../api/client';
import type { languageOptions } from '../data/worldLanguages';
import { useI18n } from '../i18n/I18nProvider';
import { CommaListInput, FormField } from '../lib/ui';
import { RUN_STATUS_OPTIONS, STATUS_OPTIONS, type Filters } from './types';

type AdvancedPanelProps = {
  filters: Filters;
  setFilters: React.Dispatch<React.SetStateAction<Filters>>;
  languages: ReturnType<typeof languageOptions>;
  /** Correspondent / document type vocabularies (#447); null while loading or unavailable. */
  facets: InventoryFacets | null;
};

/**
 * Single-choice picker for a Paperless object filter (#447). The API accepts a
 * list (ids and/or `none`); the panel offers one value, but a multi-value
 * filter from a URL or saved view is still shown and preserved until changed.
 */
function FacetSelect({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: string[];
  options: InventoryFacetValue[];
  onChange: (value: string[]) => void;
}) {
  const { t } = useI18n();
  const current = value.join(',');
  const known = current === '' || current === 'none' || options.some((option) => String(option.id) === current);
  return (
    <FormField label={label}>
      <select value={current} onChange={(event) => onChange(event.target.value ? event.target.value.split(',') : [])}>
        <option value="">{t('inventory.filter.any')}</option>
        <option value="none">{t('inventory.filter.not_set')}</option>
        {!known && <option value={current}>{current}</option>}
        {options.map((option) => (
          <option key={option.id} value={String(option.id)}>{option.name}</option>
        ))}
      </select>
    </FormField>
  );
}

export function AdvancedPanel({ filters, setFilters, languages, facets }: AdvancedPanelProps) {
  const { t } = useI18n();
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
      <FormField label={t('inventory.filter.tags_include')}>
        <CommaListInput
          values={filters.tags_include}
          onCommit={(tags_include) => setFilters((f) => ({ ...f, tags_include }))}
        />
      </FormField>
      <FormField label={t('inventory.filter.tags_exclude')}>
        <CommaListInput
          values={filters.tags_exclude}
          onCommit={(tags_exclude) => setFilters((f) => ({ ...f, tags_exclude }))}
        />
      </FormField>
      <FacetSelect
        label={t('inventory.filter.correspondent')}
        value={filters.correspondent}
        options={facets?.correspondents ?? []}
        onChange={(correspondent) => setFilters((f) => ({ ...f, correspondent }))}
      />
      <FacetSelect
        label={t('inventory.filter.document_type')}
        value={filters.document_type}
        options={facets?.document_types ?? []}
        onChange={(document_type) => setFilters((f) => ({ ...f, document_type }))}
      />
      <FormField label={t('inventory.filter.language')}>
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
      </FormField>
      <FormField label={t('inventory.filter.date_from')}>
        <input
          type="date"
          value={filters.date_from ?? ''}
          onChange={(event) => setFilters((f) => ({ ...f, date_from: event.target.value || undefined }))}
        />
      </FormField>
      <FormField label={t('inventory.filter.date_to')}>
        <input
          type="date"
          value={filters.date_to ?? ''}
          onChange={(event) => setFilters((f) => ({ ...f, date_to: event.target.value || undefined }))}
        />
      </FormField>
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
