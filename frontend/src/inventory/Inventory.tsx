import { memo, useCallback, useEffect, useMemo, useState } from 'react';
import { FileText, RefreshCw, Sparkles } from 'lucide-react';
import { api, InventoryItem } from '../api/client';
import { languageOptions } from '../data/worldLanguages';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { ActionButton, PageHeader, Status, localizedErrorMessage, run } from '../lib/ui';
import { DebugContextDetails } from '../lib/DebugContextDetails';

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
  const [busy, setBusy] = useState(false);
  const languages = useMemo(() => languageOptions(locale), [locale]);
  const load = useCallback(
    () => api.inventory().then((data) => setItems(data.items)).catch((err) => setError(localizedErrorMessage(err, t))),
    [setError, t]
  );

  useEffect(() => {
    void load();
  }, [load]);

  const triggerOcr = useCallback(
    (documentId: number) =>
      api
        .triggerDocument(documentId, ['ocr'], 'manual_review')
        .then(load)
        .catch((err) => setError(localizedErrorMessage(err, t))),
    [load, setError, t]
  );

  const triggerPipeline = useCallback(
    (documentId: number) =>
      api
        .triggerDocument(documentId, ['ocr', 'metadata'], 'manual_review')
        .then(load)
        .catch((err) => setError(localizedErrorMessage(err, t))),
    [load, setError, t]
  );

  return (
    <section className="page">
      <PageHeader title={t('inventory.title')} />
      <div className="toolbar">
        <ActionButton icon={<RefreshCw />} label={t('generic.reload')} busy={busy} onClick={() => run(setBusy, setError, load, t)} />
      </div>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
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
            {items.map((item) => (
              <InventoryRow
                key={item.paperless_document_id}
                item={item}
                languages={languages}
                t={t}
                onTriggerOcr={triggerOcr}
                onTriggerPipeline={triggerPipeline}
              />
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

type InventoryRowProps = {
  item: InventoryItem;
  languages: ReturnType<typeof languageOptions>;
  t: TFunction;
  onTriggerOcr: (documentId: number) => void;
  onTriggerPipeline: (documentId: number) => void;
};

const InventoryRow = memo(
  function InventoryRow({ item, languages, t, onTriggerOcr, onTriggerPipeline }: InventoryRowProps) {
    return (
      <tr>
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
          <button title={t('inventory.trigger_pipeline')} onClick={() => onTriggerPipeline(item.paperless_document_id)}>
            <Sparkles size={16} />
          </button>
        </td>
      </tr>
    );
  },
  (prev, next) => {
    if (prev.t !== next.t) return false;
    if (prev.languages !== next.languages) return false;
    if (prev.onTriggerOcr !== next.onTriggerOcr) return false;
    if (prev.onTriggerPipeline !== next.onTriggerPipeline) return false;
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
