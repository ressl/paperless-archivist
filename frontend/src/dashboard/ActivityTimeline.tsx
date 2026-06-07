import { useMemo } from 'react';
import { History } from 'lucide-react';
import { DashboardLiveStatus } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { formatMs, stageLabel } from '../lib/format';

type TimelineEntry = {
  id: string;
  at: string;
  kind: 'llm' | 'failure';
  stage: string;
  primary: string;
  secondary?: string;
};

export function ActivityTimeline({ live }: { live: DashboardLiveStatus | null }) {
  const { t, formatRelativeTime } = useI18n();
  const entries = useMemo<TimelineEntry[]>(() => {
    const llm = (live?.recent_llm_events ?? []).map<TimelineEntry>((event) => ({
      id: `llm-${event.id}`,
      at: event.created_at,
      kind: 'llm',
      stage: event.stage,
      primary: t('dashboard.timeline.llm_event', { provider: event.provider, model: event.model, stage: stageLabel(event.stage, t) }),
      secondary: event.duration_ms ? formatMs(event.duration_ms) : undefined
    }));
    const failures = (live?.recent_failures ?? []).map<TimelineEntry>((failure) => ({
      id: `failure-${failure.id}`,
      at: failure.updated_at,
      kind: 'failure',
      stage: failure.stage,
      primary: t('dashboard.timeline.failure', { document: failure.paperless_document_id, stage: stageLabel(failure.stage, t) }),
      secondary: failure.error_message
    }));
    return [...llm, ...failures].sort((a, b) => new Date(b.at).getTime() - new Date(a.at).getTime()).slice(0, 30);
  }, [live, t]);
  return (
    <section className="activity-timeline">
      <header>
        <History size={16} />
        <strong>{t('dashboard.timeline.title')}</strong>
      </header>
      {entries.length === 0 ? (
        <p className="empty-state compact">{t('dashboard.timeline.empty')}</p>
      ) : (
        <ol>
          {entries.map((entry) => (
            <li key={entry.id} className={`timeline-entry kind-${entry.kind}`}>
              <time>{formatRelativeTime(entry.at)}</time>
              <div>
                <strong>{entry.primary}</strong>
                {entry.secondary && <span>{entry.secondary}</span>}
              </div>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
