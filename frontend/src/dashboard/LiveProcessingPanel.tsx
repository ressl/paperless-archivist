import { DashboardLiveStatus } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { Status } from '../lib/ui';
import { formatMs, shortId, stageLabel } from '../lib/format';
import { liveFailureKind, liveFailureTiming } from './helpers';

export function LiveProcessingPanel({ live }: { live: DashboardLiveStatus | null }) {
  const { t, formatNumber, formatRelativeTime: formatRelative } = useI18n();
  const activeJobs = live?.active_jobs ?? [];
  const activeRuns = live?.active_runs ?? [];
  const recentEvents = live?.recent_llm_events ?? [];
  const recentFailures = live?.recent_failures ?? [];
  const hardFailures = recentFailures.filter((failure) => liveFailureKind(failure) === 'failed').length;

  return (
    <aside className="live-processing-panel">
      <header>
        <div>
          <strong>{t('dashboard.live.title')}</strong>
          <span>{t('dashboard.live.subtitle')}</span>
        </div>
        <Status value={live?.workflow_mode ?? 'loading'} />
      </header>
      <div className="live-summary">
        <div>
          <span>{t('dashboard.live.runs')}</span>
          <strong>{formatNumber(activeRuns.length)}</strong>
        </div>
        <div>
          <span>{t('dashboard.live.jobs')}</span>
          <strong>{formatNumber(activeJobs.length)}</strong>
        </div>
        <div>
          <span>{t('dashboard.live.issues')}</span>
          <strong>{formatNumber(hardFailures || recentFailures.length)}</strong>
        </div>
      </div>

      <section className="live-debug-section">
        <h3>{t('dashboard.live.active_jobs')}</h3>
        {activeJobs.length === 0 && <p className="empty-state compact">{t('dashboard.live.no_active_jobs')}</p>}
        {activeJobs.slice(0, 8).map((job) => {
          const elapsedMs = Math.max(0, Date.now() - new Date(job.updated_at).getTime());
          return (
            <article className="live-job" key={job.id}>
              <div>
                <strong>{t('review.document', { id: job.paperless_document_id })}</strong>
                <span>
                  {stageLabel(job.stage, t)} · {t('dashboard.live.attempt', { attempts: job.attempts, max: job.max_attempts })}
                  {' · '}
                  {t('dashboard.live.elapsed', { time: formatMs(elapsedMs) })}
                </span>
              </div>
              <Status value={job.status} />
              <small>
                {job.lease_owner ? t('dashboard.live.worker', { worker: job.lease_owner }) : formatRelative(job.updated_at)}
                {' · '}
                {t('dashboard.live.trace', { trace: shortId(job.trace_id) })}
              </small>
            </article>
          );
        })}
      </section>

      <section className="live-debug-section">
        <h3>{t('dashboard.live.latest_llm')}</h3>
        {recentEvents.length === 0 && <p className="empty-state compact">{t('dashboard.live.no_llm')}</p>}
        {recentEvents.slice(0, 5).map((event) => (
          <article className="live-event" key={event.id}>
            <strong>{event.provider} / {event.model}</strong>
            <span>{stageLabel(event.stage, t)} · {formatMs(event.duration_ms ?? 0)} · {formatRelative(event.created_at)}</span>
          </article>
        ))}
      </section>

      <section className="live-debug-section">
        <h3>{t('dashboard.live.recent_failures')}</h3>
        {recentFailures.length === 0 && <p className="empty-state compact">{t('dashboard.live.no_failures')}</p>}
        {recentFailures.slice(0, 5).map((failure) => {
          const kind = liveFailureKind(failure);
          return (
            <article className={`live-failure ${kind !== 'failed' ? 'retry' : ''}`} key={failure.id}>
              <div className="failure-heading">
                <strong>{t('dashboard.live.document_stage', { document: failure.paperless_document_id, stage: stageLabel(failure.stage, t) })}</strong>
                <Status value={kind} />
              </div>
              <span>{failure.error_message}</span>
              <small>{liveFailureTiming(failure, kind, t, formatRelative)}</small>
            </article>
          );
        })}
      </section>
    </aside>
  );
}
