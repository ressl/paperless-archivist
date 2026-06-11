import { useId } from 'react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField, NumberField, Section } from '../../lib/ui';

export function SecuritySection({
  value,
  onChange
}: {
  value: RuntimeSettings['security'];
  onChange: (patch: Partial<RuntimeSettings['security']>) => void;
}) {
  const { t } = useI18n();
  const ids = {
    auditRetention: useId(),
    artifactRetention: useId(),
    runsRetention: useId(),
    artifactStorage: useId(),
    defaultTtl: useId(),
    maxTtl: useId()
  };
  return (
    <Section title={t('settings.security')}>
      <FormField label={t('settings.security.audit_retention')} htmlFor={ids.auditRetention}>
        <NumberField
          id={ids.auditRetention}
          min={30}
          max={3650}
          value={value.audit_retention_days}
          onCommit={(audit_retention_days) => onChange({ audit_retention_days })}
        />
      </FormField>
      <FormField label={t('settings.security.ai_artifact_retention')} htmlFor={ids.artifactRetention}>
        <NumberField
          id={ids.artifactRetention}
          min={1}
          max={365}
          value={value.ai_artifact_retention_days}
          onCommit={(ai_artifact_retention_days) => onChange({ ai_artifact_retention_days })}
        />
      </FormField>
      <FormField
        label={t('settings.security.runs_retention')}
        help={t('settings.security.runs_retention.hint')}
        htmlFor={ids.runsRetention}
      >
        <NumberField
          id={ids.runsRetention}
          min={30}
          max={3650}
          value={value.runs_retention_days}
          onCommit={(runs_retention_days) => onChange({ runs_retention_days })}
        />
      </FormField>
      <FormField
        label={t('settings.security.ai_artifact_storage')}
        help={t('settings.security.hint')}
        htmlFor={ids.artifactStorage}
      >
        <select
          id={ids.artifactStorage}
          value={value.ai_artifact_storage}
          onChange={(event) =>
            onChange({ ai_artifact_storage: event.target.value as RuntimeSettings['security']['ai_artifact_storage'] })
          }
        >
          <option value="redacted">{t('settings.security.storage.redacted')}</option>
          <option value="metadata_only">{t('settings.security.storage.metadata_only')}</option>
          <option value="full">{t('settings.security.storage.full')}</option>
        </select>
      </FormField>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.api_token_expiry_required}
          onChange={(event) => onChange({ api_token_expiry_required: event.target.checked })}
        />
        {t('settings.security.token_expiry_required')}
      </label>
      <FormField label={t('settings.security.token_default_ttl')} htmlFor={ids.defaultTtl}>
        <NumberField
          id={ids.defaultTtl}
          min={1}
          max={365}
          value={value.api_token_default_ttl_days}
          onCommit={(api_token_default_ttl_days) => onChange({ api_token_default_ttl_days })}
        />
      </FormField>
      <FormField label={t('settings.security.token_max_ttl')} htmlFor={ids.maxTtl}>
        <NumberField
          id={ids.maxTtl}
          min={1}
          max={3650}
          value={value.api_token_max_ttl_days}
          onCommit={(api_token_max_ttl_days) => onChange({ api_token_max_ttl_days })}
        />
      </FormField>
    </Section>
  );
}
