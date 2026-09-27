import { memo } from 'react';
import { FileText, Sparkles, Stethoscope, Tags } from 'lucide-react';
import { type InventoryItem } from '../api/client';
import type { languageOptions } from '../data/worldLanguages';
import { type TFunction } from '../i18n/I18nProvider';
import { Status } from '../lib/ui';
import { DebugContextDetails } from '../lib/DebugContextDetails';
import { formatLanguageDetection } from './types';

export type InventoryRowProps = {
  item: InventoryItem;
  selected: boolean;
  /** A trigger request for this row is in flight (#426). */
  pending?: boolean;
  onToggleSelect: (documentId: number) => void;
  languages: ReturnType<typeof languageOptions>;
  t: TFunction;
  onTriggerOcr: (documentId: number) => void;
  onTriggerMetadata: (documentId: number) => void;
  onTriggerPipeline: (documentId: number) => void;
  onDiagnose: (documentId: number) => void;
};

export const InventoryRow = memo(
  function InventoryRow({ item, selected, pending = false, onToggleSelect, languages, t, onTriggerOcr, onTriggerMetadata, onTriggerPipeline, onDiagnose }: InventoryRowProps) {
    const id = item.paperless_document_id;
    // Icon-only buttons get a document-specific accessible name (#426).
    const actionLabel = (action: string) => t('inventory.action_for', { action, id });
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
        <td>
          {item.title || item.original_file_name || t('inventory.untitled')}
          {/* #447: Paperless correspondent / document type under the title. */}
          {(item.correspondent_name || item.document_type_name) && (
            <small className="inventory-row-meta">
              {[item.correspondent_name, item.document_type_name].filter(Boolean).join(' · ')}
            </small>
          )}
        </td>
        <td><Status value={item.ocr_status} /></td>
        <td><Status value={item.metadata_status} /></td>
        <td>{formatLanguageDetection(item, languages)}</td>
        <td>{item.current_tags && item.current_tags.length > 0 ? item.current_tags.join(', ') : '-'}</td>
        <td>{item.document_date ?? '-'}</td>
        <td>{item.current_run_status || '-'}</td>
        <td><DebugContextDetails context={item.debug_context} compact /></td>
        <td className="row-actions">
          <button
            type="button"
            title={t('inventory.trigger_ocr')}
            aria-label={actionLabel(t('inventory.trigger_ocr'))}
            disabled={pending}
            aria-busy={pending}
            onClick={() => onTriggerOcr(id)}
          >
            <FileText size={16} aria-hidden="true" />
          </button>
          <button
            type="button"
            title={t('inventory.trigger_metadata')}
            aria-label={actionLabel(t('inventory.trigger_metadata'))}
            disabled={pending}
            aria-busy={pending}
            onClick={() => onTriggerMetadata(id)}
          >
            <Tags size={16} aria-hidden="true" />
          </button>
          <button
            type="button"
            title={t('inventory.trigger_pipeline')}
            aria-label={actionLabel(t('inventory.trigger_pipeline'))}
            disabled={pending}
            aria-busy={pending}
            onClick={() => onTriggerPipeline(id)}
          >
            <Sparkles size={16} aria-hidden="true" />
          </button>
          <button
            type="button"
            title={t('inventory.diagnose.button')}
            aria-label={actionLabel(t('inventory.diagnose.button'))}
            onClick={() => onDiagnose(id)}
          >
            <Stethoscope size={16} aria-hidden="true" />
          </button>
        </td>
      </tr>
    );
  },
  (prev, next) => {
    if (prev.t !== next.t) return false;
    if (prev.selected !== next.selected) return false;
    if (prev.pending !== next.pending) return false;
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
      a.correspondent_name === b.correspondent_name &&
      a.document_type_name === b.document_type_name &&
      a.debug_context === b.debug_context
    );
  }
);
