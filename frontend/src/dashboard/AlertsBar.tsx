import { AlertTriangle } from 'lucide-react';
import { NeedsAttentionItem } from '../api/client';
import { localizedMessage, useI18n } from '../i18n/I18nProvider';

export function AlertsBar({
  items,
  onAction
}: {
  items: NeedsAttentionItem[];
  onAction: (item: NeedsAttentionItem) => void;
}) {
  const { t, formatNumber } = useI18n();
  if (items.length === 0) return null;
  return (
    <section className="alerts-bar" role="region" aria-label={t('dashboard.alerts.title')}>
      <header>
        <AlertTriangle size={16} />
        <strong>{t('dashboard.alerts.title')}</strong>
      </header>
      <ul>
        {items.map((item, idx) => (
          <li key={`${item.kind}-${idx}`} className={`alert-item severity-${item.severity}`}>
            <div className="alert-text">
              <strong>{item.title}</strong>
              <span>{item.description}</span>
            </div>
            {item.count != null && (
              <span className="alert-count" aria-label={localizedMessage('dashboard.alerts.count', t, 'count')}>{formatNumber(item.count)}</span>
            )}
            {item.action_key && (
              <button type="button" onClick={() => onAction(item)} className="alert-action">
                {localizedMessage(item.action_key, t, item.action_key)}
              </button>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
