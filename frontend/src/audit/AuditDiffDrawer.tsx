import { useEffect, useMemo, useRef } from 'react';
import { X } from 'lucide-react';
import { api } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { ErrorState, LoadingState } from '../lib/states';
import { Status, localizedErrorMessage, useFocusTrap } from '../lib/ui';
import { useResource } from '../lib/useResource';
import { diffAuditSnapshots, formatAuditValue } from './auditDiff';

/**
 * Before/after inspector for one audit event (#448). Loads the event detail
 * (snapshots are only served per event, never in the list) and renders the
 * field-level diff; the raw snapshots stay available below it.
 */
export function AuditDiffDrawer({ eventId, onClose }: { eventId: string; onClose: () => void }) {
  const { t, formatDateTime } = useI18n();
  const drawerRef = useRef<HTMLElement>(null);
  const titleId = `audit-diff-title-${eventId}`;
  useFocusTrap(true, drawerRef);
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [onClose]);

  const detail = useResource((signal) => api.auditEvent(eventId, { signal }), [eventId]);
  const rows = useMemo(
    () => (detail.data ? diffAuditSnapshots(detail.data.before, detail.data.after) : []),
    [detail.data]
  );

  return (
    <div className="drawer-root" role="dialog" aria-modal="true" aria-labelledby={titleId}>
      <div className="drawer-backdrop" onClick={onClose} />
      <aside ref={drawerRef} className="drawer audit-diff-drawer" aria-busy={detail.loading}>
        <header>
          <strong id={titleId}>
            {t('audit.diff.title', { event: detail.data?.event_type ?? '…' })}
          </strong>
          <button type="button" className="drawer-close" onClick={onClose} aria-label={t('audit.diff.close')}>
            <X size={18} />
          </button>
        </header>
        <div className="audit-diff-body">
          {detail.loading && !detail.data && <LoadingState compact />}
          {detail.error != null && (
            <ErrorState
              compact
              title={t('audit.diff.load_failed')}
              detail={localizedErrorMessage(detail.error, t)}
              onRetry={() => void detail.reload()}
            />
          )}
          {detail.data && (
            <>
              <p className="field-hint">
                {formatDateTime(detail.data.created_at)} · {detail.data.actor_username ?? detail.data.actor_id ?? detail.data.actor_type}
                {detail.data.paperless_document_id != null ? ` · #${detail.data.paperless_document_id}` : ''}{' '}
                <Status value={detail.data.outcome} />
              </p>
              {rows.length === 0 ? (
                <p className="field-hint">{t('audit.diff.empty')}</p>
              ) : (
                <div className="table-wrap">
                  <table aria-label={t('audit.diff.table')}>
                    <thead>
                      <tr>
                        <th scope="col">{t('audit.diff.field')}</th>
                        <th scope="col">{t('audit.diff.before')}</th>
                        <th scope="col">{t('audit.diff.after')}</th>
                      </tr>
                    </thead>
                    <tbody>
                      {rows.map((row) => (
                        <tr key={row.path || '(value)'} className={`audit-diff-${row.kind}`}>
                          <th scope="row"><code>{row.path || '-'}</code></th>
                          <td>{row.kind === 'added' ? '-' : <del>{formatAuditValue(row.before)}</del>}</td>
                          <td>{row.kind === 'removed' ? '-' : <ins>{formatAuditValue(row.after)}</ins>}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
              <details className="diagnose-raw">
                <summary>{t('audit.diff.raw')}</summary>
                <pre>{JSON.stringify({ before: detail.data.before ?? null, after: detail.data.after ?? null }, null, 2)}</pre>
              </details>
            </>
          )}
        </div>
      </aside>
    </div>
  );
}
