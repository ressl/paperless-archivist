import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AlertTriangle, Pause, Play, RefreshCw } from 'lucide-react';
import { api, type AuditEvent, type DashboardLiveStatus } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { ActionButton, PageHeader, Status, localizedErrorMessage, run } from '../lib/ui';

const POLL_INTERVAL_MS = 2_500;

function formatRelative(iso: string, t: ReturnType<typeof useI18n>['t']): string {
  const then = new Date(iso).getTime();
  const now = Date.now();
  const seconds = Math.max(0, Math.round((now - then) / 1000));
  if (seconds < 60) return t('debug.ago_seconds', { value: seconds });
  if (seconds < 3600) return t('debug.ago_minutes', { value: Math.round(seconds / 60) });
  return t('debug.ago_hours', { value: Math.round(seconds / 3600) });
}

export function DebugConsole({ setError }: { setError: (error: string | null) => void }) {
  const { t } = useI18n();
  const [live, setLive] = useState<DashboardLiveStatus | null>(null);
  const [audit, setAudit] = useState<AuditEvent[]>([]);
  const [busy, setBusy] = useState(false);
  const [paused, setPaused] = useState(false);
  const [lastRefresh, setLastRefresh] = useState<string | null>(null);
  const inflightRef = useRef(false);

  const reload = useCallback(async () => {
    if (inflightRef.current) return;
    inflightRef.current = true;
    try {
      const [liveData, auditData] = await Promise.all([
        api.dashboardLive(),
        api.audit(),
      ]);
      setLive(liveData);
      setAudit(auditData.items.slice(0, 50));
      setLastRefresh(new Date().toISOString());
    } catch (err) {
      setError(localizedErrorMessage(err, t));
    } finally {
      inflightRef.current = false;
    }
  }, [setError, t]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // Poll only while the tab is visible (mirrors the dashboard's
  // useVisibleInterval): pause on hidden, force-refresh once on return.
  useEffect(() => {
    if (paused) return;
    let timer: number | null = null;
    const start = () => {
      if (timer != null) return;
      timer = window.setInterval(() => {
        void reload();
      }, POLL_INTERVAL_MS);
    };
    const stop = () => {
      if (timer != null) {
        window.clearInterval(timer);
        timer = null;
      }
    };
    const handleVisibility = () => {
      if (document.hidden) {
        stop();
      } else {
        void reload();
        start();
      }
    };
    if (!document.hidden) start();
    document.addEventListener('visibilitychange', handleVisibility);
    return () => {
      stop();
      document.removeEventListener('visibilitychange', handleVisibility);
    };
  }, [paused, reload]);

  const activeJobs = live?.active_jobs ?? [];
  const activeRuns = live?.active_runs ?? [];
  const llmEvents = live?.recent_llm_events ?? [];
  const failures = live?.recent_failures ?? [];

  const lastRefreshLabel = useMemo(() => {
    if (!lastRefresh) return '-';
    return formatRelative(lastRefresh, t);
  }, [lastRefresh, t]);

  return (
    <section className="page debug-console">
      <PageHeader title={t('debug.title')} />
      <div className="toolbar">
        <ActionButton
          icon={<RefreshCw />}
          label={t('generic.reload')}
          busy={busy}
          onClick={() => run(setBusy, setError, reload, t)}
        />
        <button className="ghost-button" onClick={() => setPaused((p) => !p)}>
          {paused ? <Play size={16} /> : <Pause size={16} />}
          {paused ? t('debug.resume') : t('debug.pause')}
        </button>
        <small className="field-hint">
          {paused ? t('debug.paused_label') : t('debug.live_label', { value: lastRefreshLabel })}
        </small>
      </div>

      <div className="debug-grid">
        <DebugPanel title={t('debug.active_jobs', { count: activeJobs.length })}>
          {activeJobs.length === 0 ? (
            <p className="field-hint">{t('debug.empty_active_jobs')}</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th>{t('debug.col_document')}</th>
                  <th>{t('debug.col_stage')}</th>
                  <th>{t('debug.col_status')}</th>
                  <th>{t('debug.col_attempts')}</th>
                  <th>{t('debug.col_updated')}</th>
                </tr>
              </thead>
              <tbody>
                {activeJobs.map((job) => (
                  <tr key={job.id}>
                    <td>{job.paperless_document_id}</td>
                    <td>{job.stage}</td>
                    <td><Status value={job.status} /></td>
                    <td>{job.attempts}/{job.max_attempts}</td>
                    <td>{formatRelative(job.updated_at, t)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </DebugPanel>

        <DebugPanel title={t('debug.active_runs', { count: activeRuns.length })}>
          {activeRuns.length === 0 ? (
            <p className="field-hint">{t('debug.empty_active_runs')}</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th>{t('debug.col_document')}</th>
                  <th>{t('debug.col_status')}</th>
                  <th>{t('debug.col_stages')}</th>
                  <th>{t('debug.col_updated')}</th>
                </tr>
              </thead>
              <tbody>
                {activeRuns.map((run) => (
                  <tr key={run.id}>
                    <td>{run.paperless_document_id}</td>
                    <td><Status value={run.status} /></td>
                    <td>{run.stages.join(', ')}</td>
                    <td>{formatRelative(run.updated_at, t)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </DebugPanel>

        <DebugPanel title={t('debug.recent_llm', { count: llmEvents.length })}>
          {llmEvents.length === 0 ? (
            <p className="field-hint">{t('debug.empty_llm')}</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th>{t('debug.col_stage')}</th>
                  <th>{t('debug.col_provider')}</th>
                  <th>{t('debug.col_model')}</th>
                  <th>{t('debug.col_duration')}</th>
                  <th>{t('debug.col_created')}</th>
                </tr>
              </thead>
              <tbody>
                {llmEvents.map((ev) => (
                  <tr key={ev.id}>
                    <td>{ev.stage}</td>
                    <td>{ev.provider}</td>
                    <td>{ev.model}</td>
                    <td>{ev.duration_ms != null ? `${(ev.duration_ms / 1000).toFixed(2)}s` : '-'}</td>
                    <td>{formatRelative(ev.created_at, t)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </DebugPanel>

        <DebugPanel title={t('debug.recent_failures', { count: failures.length })}>
          {failures.length === 0 ? (
            <p className="field-hint">{t('debug.empty_failures')}</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th>{t('debug.col_document')}</th>
                  <th>{t('debug.col_stage')}</th>
                  <th>{t('debug.col_kind')}</th>
                  <th>{t('debug.col_error')}</th>
                  <th>{t('debug.col_updated')}</th>
                </tr>
              </thead>
              <tbody>
                {failures.map((f) => (
                  <tr key={f.id}>
                    <td>{f.paperless_document_id}</td>
                    <td>{f.stage}</td>
                    <td>
                      <span className="status danger">
                        <AlertTriangle size={12} /> {f.failure_kind}
                      </span>
                    </td>
                    <td className="error-cell">{f.error_message.slice(0, 200)}</td>
                    <td>{formatRelative(f.updated_at, t)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </DebugPanel>

        <DebugPanel title={t('debug.recent_audit', { count: audit.length })}>
          {audit.length === 0 ? (
            <p className="field-hint">{t('debug.empty_audit')}</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th>{t('debug.col_event')}</th>
                  <th>{t('debug.col_actor')}</th>
                  <th>{t('debug.col_outcome')}</th>
                  <th>{t('debug.col_document')}</th>
                  <th>{t('debug.col_created')}</th>
                </tr>
              </thead>
              <tbody>
                {audit.map((ev) => (
                  <tr key={ev.id}>
                    <td>{ev.event_type}</td>
                    <td>{ev.actor_type}</td>
                    <td><Status value={ev.outcome} /></td>
                    <td>{ev.paperless_document_id ?? '-'}</td>
                    <td>{formatRelative(ev.created_at, t)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </DebugPanel>
      </div>
    </section>
  );
}

function DebugPanel({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <article className="debug-panel">
      <header><h3>{title}</h3></header>
      <div className="table-wrap">{children}</div>
    </article>
  );
}
