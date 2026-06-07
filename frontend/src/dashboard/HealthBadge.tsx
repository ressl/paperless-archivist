import { useI18n } from '../i18n/I18nProvider';
import { useFreshness } from './hooks';
import { healthTone } from './helpers';

// Isolated leaf component: owns the 1s freshness ticker so the per-second
// re-renders stay scoped to the countdown and never reach the heavy Dashboard
// tree (Recharts + tables).
function FreshnessCountdown({ intervalMs, lastLoadedAt }: { intervalMs: number; lastLoadedAt: string | null }) {
  const { t } = useI18n();
  const { nextRefreshIn, pulse } = useFreshness(intervalMs, lastLoadedAt);
  return (
    <div className="freshness-indicator" aria-live="polite">
      <span className={`pulse-dot ${pulse ? 'is-pulsing' : ''}`} aria-hidden="true" />
      <em>{t('dashboard.freshness.next', { seconds: nextRefreshIn })}</em>
    </div>
  );
}

export function HealthBadge({
  score,
  generatedAt,
  lastLoadedAt
}: {
  score: number | null;
  generatedAt: string | null;
  lastLoadedAt: string | null;
}) {
  const { t, formatRelativeTime } = useI18n();
  const tone = healthTone(score);
  const label = score == null
    ? t('dashboard.health.score_label')
    : tone === 'success'
      ? t('dashboard.health.healthy')
      : tone === 'warning'
        ? t('dashboard.health.degraded')
        : t('dashboard.health.unhealthy');
  return (
    <div className={`health-badge ${tone}`} role="status">
      <span className="health-score">{score ?? '-'}</span>
      <div>
        <strong>{label}</strong>
        <em>{generatedAt ? formatRelativeTime(generatedAt) : '-'}</em>
      </div>
      <FreshnessCountdown intervalMs={30_000} lastLoadedAt={lastLoadedAt} />
    </div>
  );
}
