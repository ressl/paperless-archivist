import { useId } from 'react';
import { Send } from 'lucide-react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { Button, FormField, Section } from '../../lib/ui';
import { ConnectionTestFeedback } from './ConnectionTestFeedback';
import type { ConnectionTestState } from './types';

export function NotificationsSection({
  value,
  onChange,
  webhook,
  onWebhookChange,
  test,
  onTest
}: {
  value: RuntimeSettings['notifications'];
  onChange: (patch: Partial<RuntimeSettings['notifications']>) => void;
  webhook: string;
  onWebhookChange: (webhook: string) => void;
  test: ConnectionTestState | null;
  onTest: () => void;
}) {
  const { t } = useI18n();
  const ids = {
    webhook: useId(),
    reviewThreshold: useId(),
    failureThreshold: useId(),
    cooldown: useId()
  };
  const testing = test?.status === 'running';
  return (
    <Section title={t('settings.notifications')}>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.enabled}
          onChange={(event) => onChange({ enabled: event.target.checked })}
        />
        {t('settings.notifications.enabled')}
      </label>
      <FormField
        label={t('settings.notifications.webhook_url')}
        help={t('settings.notifications.webhook_hint')}
        htmlFor={ids.webhook}
      >
        <input
          id={ids.webhook}
          value={webhook}
          type="password"
          onChange={(event) => onWebhookChange(event.target.value)}
          placeholder={value.webhook_url_secret_id ? t('settings.paperless.configured') : 'https://hooks.example.com/...'}
        />
      </FormField>
      <FormField label={t('settings.notifications.review_threshold')} htmlFor={ids.reviewThreshold}>
        <input
          id={ids.reviewThreshold}
          type="number"
          min="1"
          value={value.review_queue_threshold}
          onChange={(event) => onChange({ review_queue_threshold: Number(event.target.value) })}
        />
      </FormField>
      <FormField label={t('settings.notifications.failure_threshold')} htmlFor={ids.failureThreshold}>
        <input
          id={ids.failureThreshold}
          type="number"
          min="1"
          value={value.repeated_failure_threshold}
          onChange={(event) => onChange({ repeated_failure_threshold: Number(event.target.value) })}
        />
      </FormField>
      <FormField label={t('settings.notifications.cooldown')} htmlFor={ids.cooldown}>
        <input
          id={ids.cooldown}
          type="number"
          min="1"
          max="1440"
          value={value.cooldown_minutes}
          onChange={(event) => onChange({ cooldown_minutes: Number(event.target.value) })}
        />
      </FormField>
      <Button variant="secondary" icon={<Send size={16} />} title={t('generic.test')} disabled={testing} onClick={onTest}>
        {testing ? t('generic.testing') : t('generic.test')}
      </Button>
      <ConnectionTestFeedback state={test} />
    </Section>
  );
}
