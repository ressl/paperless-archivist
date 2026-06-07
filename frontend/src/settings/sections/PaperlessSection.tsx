import { useId } from 'react';
import { Database } from 'lucide-react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { Button, FormField, Section } from '../../lib/ui';
import { ConnectionTestFeedback } from './ConnectionTestFeedback';
import type { ConnectionTestState } from './types';

export function PaperlessSection({
  value,
  onChange,
  token,
  onTokenChange,
  test,
  onTest
}: {
  value: RuntimeSettings['paperless'];
  onChange: (patch: Partial<RuntimeSettings['paperless']>) => void;
  token: string;
  onTokenChange: (token: string) => void;
  test: ConnectionTestState | null;
  onTest: () => void;
}) {
  const { t } = useI18n();
  const ids = {
    baseUrl: useId(),
    token: useId(),
    overlap: useId(),
    archive: useId()
  };
  const testing = test?.status === 'running';
  return (
    <Section title={t('settings.paperless')}>
      <FormField label={t('settings.paperless.base_url')} help={t('settings.paperless.base_url_hint')} htmlFor={ids.baseUrl}>
        <input
          id={ids.baseUrl}
          value={value.base_url}
          onChange={(event) => onChange({ base_url: event.target.value })}
        />
      </FormField>
      <FormField label={t('settings.paperless.api_token')} htmlFor={ids.token}>
        <input
          id={ids.token}
          value={token}
          type="password"
          onChange={(event) => onTokenChange(event.target.value)}
          placeholder={value.token_secret_id ? t('settings.paperless.configured') : ''}
        />
      </FormField>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.login_bridge_enabled}
          onChange={(event) => onChange({ login_bridge_enabled: event.target.checked })}
        />
        {t('settings.paperless.login_bridge')}
      </label>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.delta_sync_enabled}
          onChange={(event) => onChange({ delta_sync_enabled: event.target.checked })}
        />
        {t('settings.paperless.delta_sync')}
      </label>
      <FormField label={t('settings.paperless.delta_overlap')} htmlFor={ids.overlap}>
        <input
          id={ids.overlap}
          type="number"
          min="0"
          max="1440"
          value={value.delta_sync_overlap_minutes}
          onChange={(event) => onChange({ delta_sync_overlap_minutes: Number(event.target.value) })}
        />
      </FormField>
      <FormField label={t('settings.paperless.active_archive')} htmlFor={ids.archive}>
        <input
          id={ids.archive}
          value={value.active_archive}
          onChange={(event) => onChange({ active_archive: event.target.value })}
        />
      </FormField>
      <Button variant="secondary" icon={<Database size={16} />} title={t('generic.test')} disabled={testing} onClick={onTest}>
        {testing ? t('generic.testing') : t('generic.test')}
      </Button>
      <ConnectionTestFeedback state={test} />
    </Section>
  );
}
