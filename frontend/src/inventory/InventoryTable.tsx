import { useEffect, useRef } from 'react';
import { type InventoryItem } from '../api/client';
import type { languageOptions } from '../data/worldLanguages';
import { useI18n } from '../i18n/I18nProvider';
import { InventoryRow } from './InventoryRow';

const COLUMN_COUNT = 11;

type InventoryTableProps = {
  items: InventoryItem[];
  loading: boolean;
  selected: Set<number>;
  allOnPageSelected: boolean;
  onToggleSelect: (documentId: number) => void;
  onToggleSelectAll: () => void;
  languages: ReturnType<typeof languageOptions>;
  onTriggerOcr: (documentId: number) => void;
  onTriggerMetadata: (documentId: number) => void;
  onTriggerPipeline: (documentId: number) => void;
  onDiagnose: (documentId: number) => void;
};

export function InventoryTable({
  items,
  loading,
  selected,
  allOnPageSelected,
  onToggleSelect,
  onToggleSelectAll,
  languages,
  onTriggerOcr,
  onTriggerMetadata,
  onTriggerPipeline,
  onDiagnose,
}: InventoryTableProps) {
  const { t } = useI18n();

  // A11y: reflect a partial page selection as the checkbox's indeterminate
  // state (cannot be expressed via the `checked` attribute).
  const selectAllRef = useRef<HTMLInputElement>(null);
  const someSelected = items.some((item) => selected.has(item.paperless_document_id));
  useEffect(() => {
    if (selectAllRef.current) {
      selectAllRef.current.indeterminate = someSelected && !allOnPageSelected;
    }
  }, [someSelected, allOnPageSelected]);

  return (
    <div className="table-wrap">
      <table aria-busy={loading} aria-label={t('inventory.title')}>
        <thead>
          <tr>
            <th scope="col" className="select-col">
              <input
                ref={selectAllRef}
                type="checkbox"
                checked={allOnPageSelected}
                disabled={items.length === 0}
                onChange={onToggleSelectAll}
                aria-label={t('inventory.select_all')}
              />
            </th>
            <th scope="col">{t('inventory.id')}</th>
            <th scope="col">{t('inventory.document_title')}</th>
            <th scope="col">{t('inventory.ocr')}</th>
            <th scope="col">{t('inventory.metadata')}</th>
            <th scope="col">{t('inventory.language')}</th>
            <th scope="col">{t('inventory.tags')}</th>
            <th scope="col">{t('inventory.date')}</th>
            <th scope="col">{t('inventory.run')}</th>
            <th scope="col">{t('inventory.debug')}</th>
            <th scope="col">{t('inventory.actions')}</th>
          </tr>
        </thead>
        <tbody>
          {items.length === 0 ? (
            <tr>
              <td colSpan={COLUMN_COUNT} className="empty-row">
                {loading ? t('generic.loading') : t('inventory.no_results')}
              </td>
            </tr>
          ) : (
            items.map((item) => (
              <InventoryRow
                key={item.paperless_document_id}
                item={item}
                selected={selected.has(item.paperless_document_id)}
                onToggleSelect={onToggleSelect}
                languages={languages}
                t={t}
                onTriggerOcr={onTriggerOcr}
                onTriggerMetadata={onTriggerMetadata}
                onTriggerPipeline={onTriggerPipeline}
                onDiagnose={onDiagnose}
              />
            ))
          )}
        </tbody>
      </table>
    </div>
  );
}
