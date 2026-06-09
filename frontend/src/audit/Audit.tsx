import { useEffect, useState } from 'react';
import { Archive, Check, FileText, Shield, X } from 'lucide-react';
import { api, AuditEvent, AuditIntegrityReport, RetentionResult } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { ActionButton, Button, PageHeader, Status, localizedErrorMessage, run } from '../lib/ui';

export function Audit({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime, formatNumber } = useI18n();
  const [items, setItems] = useState<AuditEvent[]>([]);
  const [integrity, setIntegrity] = useState<AuditIntegrityReport | null>(null);
  const [retentionResult, setRetentionResult] = useState<RetentionResult | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    Promise.all([api.audit(), api.auditIntegrity()])
      .then(([auditData, integrityData]) => {
        setItems(auditData.items);
        setIntegrity(integrityData);
      })
      .catch((err) => setError(localizedErrorMessage(err, t)));
  }, [setError, t]);
  const refreshIntegrity = () => api.auditIntegrity()
    .then(setIntegrity)
    .catch((err) => setError(localizedErrorMessage(err, t)));
  return (
    <section className="page">
      <PageHeader title={t('audit.title')} />
      <div className="toolbar">
        <a className="button-link" href="/api/audit/export.csv">
          <FileText size={16} /> {t('audit.export_csv')}
        </a>
        <Button variant="secondary" icon={<Shield size={16} />} onClick={refreshIntegrity}>
          {t('audit.verify_chain')}
        </Button>
        <ActionButton
          icon={<Archive />}
          label={t('audit.apply_retention')}
          busy={busy}
          onClick={() => run(setBusy, setError, () => api.applyAuditRetention().then((result) => {
            setRetentionResult(result);
            return Promise.all([api.audit(), api.auditIntegrity()]).then(([auditData, integrityData]) => {
              setItems(auditData.items);
              setIntegrity(integrityData);
            });
          }), t)}
        />
      </div>
      {integrity && (
        <div
          className={`connection-feedback ${integrity.ok ? 'success' : 'error'}`}
          role={integrity.ok ? 'status' : 'alert'}
          aria-live={integrity.ok ? 'polite' : 'assertive'}
        >
          <header>
            {integrity.ok ? <Check size={16} /> : <X size={16} />}
            <strong>{integrity.ok ? t('audit.chain_verified') : t('audit.chain_problem')}</strong>
          </header>
          <p>
            {t('audit.checked_events', { count: formatNumber(integrity.checked_events) })}
            {integrity.legacy_events > 0 ? ` ${t('audit.legacy_events', { count: formatNumber(integrity.legacy_events) })}` : ''}
            {integrity.broken_reason ? ` ${integrity.broken_reason}` : ''}
          </p>
        </div>
      )}
      {retentionResult && (
        <div className="connection-feedback success" role="status" aria-live="polite">
          <header><Check size={16} /><strong>{t('audit.retention_applied')}</strong></header>
          <p>
            {t('audit.retention_summary', {
              artifacts: formatNumber(retentionResult.ai_artifacts_deleted),
              events: formatNumber(retentionResult.audit_events_deleted),
              ocr_pages: formatNumber(retentionResult.ocr_page_cache_deleted)
            })}
          </p>
        </div>
      )}
      <div className="table-wrap">
        <table>
          <thead>
            <tr><th>{t('audit.col_time')}</th><th>{t('audit.col_event')}</th><th>{t('audit.col_actor')}</th><th>{t('audit.col_document')}</th><th>{t('audit.col_outcome')}</th><th>{t('audit.col_hash')}</th></tr>
          </thead>
          <tbody>
            {items.map((item) => (
              <tr key={item.id}>
                <td>{formatDateTime(item.created_at)}</td>
                <td>{item.event_type}</td>
                <td>{item.actor_type}</td>
                <td>{item.paperless_document_id || '-'}</td>
                <td><Status value={item.outcome} /></td>
                <td>{item.event_hash ? `${item.event_hash.slice(0, 12)}...` : t('audit.hash_legacy')}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}
