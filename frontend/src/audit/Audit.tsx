import { useEffect, useState } from 'react';
import { Archive, Check, FileText, Shield, X } from 'lucide-react';
import { api, AuditEvent, AuditIntegrityReport, RetentionResult } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { ActionButton, PageHeader, Status, localizedErrorMessage, run } from '../lib/ui';

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
      <PageHeader title="Audit Log" />
      <div className="toolbar">
        <a className="button-link" href="/api/audit/export.csv">
          <FileText size={16} /> Export CSV
        </a>
        <button onClick={refreshIntegrity}>
          <Shield size={16} /> Verify chain
        </button>
        <ActionButton
          icon={<Archive />}
          label="Apply retention"
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
        <div className={`connection-feedback ${integrity.ok ? 'success' : 'error'}`}>
          <header>
            {integrity.ok ? <Check size={16} /> : <X size={16} />}
            <strong>{integrity.ok ? 'Audit chain verified' : 'Audit chain problem'}</strong>
          </header>
          <p>
            Checked {formatNumber(integrity.checked_events)} hashed events.
            {integrity.legacy_events > 0 ? ` ${formatNumber(integrity.legacy_events)} legacy events predate hash-chain tracking.` : ''}
            {integrity.broken_reason ? ` ${integrity.broken_reason}` : ''}
          </p>
        </div>
      )}
      {retentionResult && (
        <div className="connection-feedback success">
          <header><Check size={16} /><strong>Retention applied</strong></header>
          <p>
            Deleted {formatNumber(retentionResult.ai_artifacts_deleted)} AI artifacts and {formatNumber(retentionResult.audit_events_deleted)} audit events outside retention.
          </p>
        </div>
      )}
      <div className="table-wrap">
        <table>
          <thead>
            <tr><th>Time</th><th>Event</th><th>Actor</th><th>Document</th><th>Outcome</th><th>Hash</th></tr>
          </thead>
          <tbody>
            {items.map((item) => (
              <tr key={item.id}>
                <td>{formatDateTime(item.created_at)}</td>
                <td>{item.event_type}</td>
                <td>{item.actor_type}</td>
                <td>{item.paperless_document_id || '-'}</td>
                <td><Status value={item.outcome} /></td>
                <td>{item.event_hash ? `${item.event_hash.slice(0, 12)}...` : 'legacy'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}
