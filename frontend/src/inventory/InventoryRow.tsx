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
  onToggleSelect: (documentId: number) => void;
  languages: ReturnType<typeof languageOptions>;
  t: TFunction;
  onTriggerOcr: (documentId: number) => void;
  onTriggerMetadata: (documentId: number) => void;
  onTriggerPipeline: (documentId: number) => void;
  onDiagnose: (documentId: number) => void;
};

export const InventoryRow = memo(
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
