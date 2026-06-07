import { useCallback, useEffect, useState } from 'react';
import { ExternalLink, RefreshCw } from 'lucide-react';
import { api, type DuplicateGroup } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { ActionButton, localizedErrorMessage } from '../lib/ui';

// Read-only dedup view (#216): lists groups of documents sharing the same OCR
// content hash, each member linking out to its Paperless detail page. Fetches
// its own data when first rendered so the cost is only paid when the operator
// opens the panel.
export function DuplicatesPanel({ setError }: { setError: (error: string | null) => void }) {
  const { t } = useI18n();
  const [groups, setGroups] = useState<DuplicateGroup[] | null>(null);
  const [paperlessBase, setPaperlessBase] = useState<string>('');
  const [busy, setBusy] = useState(false);

  const load = useCallback(() => {
    setBusy(true);
    return api
      .inventoryDuplicates()
      .then((data) => {
        setGroups(data.groups);
        setPaperlessBase(data.paperless_base);
      })
      .catch((err) => setError(localizedErrorMessage(err, t)))
      .finally(() => setBusy(false));
  }, [setError, t]);

  useEffect(() => {
    void load();
  }, [load]);

  const documentUrl = (id: number) =>
    paperlessBase ? `${paperlessBase}/documents/${id}/details` : null;

  return (
    <div className="card">
      <div className="toolbar">
        <strong>{t('inventory.duplicates_title')}</strong>
        <ActionButton icon={<RefreshCw />} label={t('generic.reload')} busy={busy} onClick={load} />
        {groups != null && (
          <small className="field-hint">{t('inventory.duplicates_group_count', { count: groups.length })}</small>
        )}
      </div>
      {busy && groups == null && <p className="field-hint">{t('generic.loading')}</p>}
      {groups != null && groups.length === 0 && (
        <p className="field-hint">{t('inventory.duplicates_empty')}</p>
      )}
      {groups != null && groups.length > 0 && (
        <div className="card-grid card-grid--wide">
          {groups.map((group) => (
            <div key={group.hash} className="card card--compact card--muted">
              <div className="field-hint">
                {t('inventory.duplicates_hash', { hash: group.hash.slice(0, 12) })} ·{' '}
                {t('inventory.duplicates_doc_count', { count: group.documents.length })}
              </div>
              <ul>
                {group.documents.map((doc) => {
                  const url = documentUrl(doc.paperless_document_id);
                  const label = `#${doc.paperless_document_id} ${doc.title ?? t('inventory.untitled')}`;
                  return (
                    <li key={doc.paperless_document_id}>
                      {url ? (
                        <a href={url} target="_blank" rel="noreferrer">
                          <ExternalLink size={14} /> {label}
                        </a>
                      ) : (
                        label
                      )}
                    </li>
                  );
                })}
              </ul>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
