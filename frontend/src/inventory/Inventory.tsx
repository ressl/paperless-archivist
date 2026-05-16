import { useEffect, useMemo, useState } from 'react';
import { FileText, RefreshCw, Tags } from 'lucide-react';
import { api, InventoryItem } from '../api/client';
import { languageOptions } from '../data/worldLanguages';
import { useI18n } from '../i18n/I18nProvider';
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
  const load = () => api.inventory().then((data) => setItems(data.items)).catch((err) => setError(localizedErrorMessage(err, t)));

  useEffect(() => {
    void load();
  }, []);

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
              <th>{t('inventory.language')}</th>
              <th>{t('inventory.tags')}</th>
              <th>{t('stage.title')}</th>
              <th>{t('inventory.type')}</th>
              <th>{t('inventory.date')}</th>
              <th>{t('inventory.run')}</th>
              <th>{t('inventory.debug')}</th>
              <th>{t('inventory.actions')}</th>
            </tr>
          </thead>
          <tbody>
            {items.map((item) => (
              <tr key={item.paperless_document_id}>
                <td>{item.paperless_document_id}</td>
                <td>{item.title || item.original_file_name || t('inventory.untitled')}</td>
                <td><Status value={item.ocr_status} /></td>
                <td>{formatLanguageDetection(item, languages)}</td>
                <td><Status value={item.tagging_status} /></td>
                <td><Status value={item.title_status} /></td>
                <td><Status value={item.document_type_status} /></td>
                <td><Status value={item.document_date_status} /> {item.document_date ?? ''}</td>
                <td>{item.current_run_status || '-'}</td>
                <td><DebugContextDetails context={item.debug_context} compact /></td>
                <td className="row-actions">
                  <button title={t('inventory.trigger_ocr')} onClick={() => api.triggerDocument(item.paperless_document_id, ['ocr'], 'manual_review').then(load).catch((err) => setError(localizedErrorMessage(err, t)))}>
                    <FileText size={16} />
                  </button>
                  <button title={t('inventory.trigger_tags')} onClick={() => api.triggerDocument(item.paperless_document_id, ['tags'], 'manual_review').then(load).catch((err) => setError(localizedErrorMessage(err, t)))}>
                    <Tags size={16} />
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}
