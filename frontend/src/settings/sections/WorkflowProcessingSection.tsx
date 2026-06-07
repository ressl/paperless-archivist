import { useId } from 'react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField, Section } from '../../lib/ui';
import { workflowModeDescription, workflowModeOptions } from '../../lib/workflow';
import { optionalPositiveInteger, splitTags } from './helpers';

export function WorkflowProcessingSection({
  value,
  onChange
}: {
  value: RuntimeSettings['workflow'];
  onChange: (patch: Partial<RuntimeSettings['workflow']>) => void;
}) {
  const { t } = useI18n();
  const ids = {
    mode: useId(),
    hourly: useId(),
    daily: useId(),
    include: useId(),
    exclude: useId()
  };
  return (
    <Section title={t('settings.workflow.section.processing')}>
      <FormField
        label={t('settings.workflow.mode')}
        help={workflowModeDescription(value.mode, t)}
        htmlFor={ids.mode}
      >
        <select
          id={ids.mode}
          value={value.mode}
          onChange={(event) => onChange({ mode: event.target.value as RuntimeSettings['workflow']['mode'] })}
        >
          {workflowModeOptions.map((option) => (
            <option key={option.value} value={option.value}>
              {t(option.labelKey)}
            </option>
          ))}
        </select>
      </FormField>
      <label className="inline">
        <input type="checkbox" checked={value.paused} onChange={(event) => onChange({ paused: event.target.checked })} />
        {t('settings.workflow.paused')}
      </label>
      <label className="inline">
        <input type="checkbox" checked={value.dry_run} onChange={(event) => onChange({ dry_run: event.target.checked })} />
        {t('settings.workflow.dry_run')}
      </label>
      <FormField label={t('settings.workflow.hourly_limit')} htmlFor={ids.hourly}>
        <input
          id={ids.hourly}
          type="number"
          min="1"
          value={value.hourly_document_limit ?? ''}
          placeholder={t('settings.workflow.limit_placeholder')}
          onChange={(event) => onChange({ hourly_document_limit: optionalPositiveInteger(event.target.value) })}
        />
      </FormField>
      <FormField label={t('settings.workflow.daily_limit')} htmlFor={ids.daily}>
        <input
          id={ids.daily}
          type="number"
          min="1"
          value={value.daily_document_limit ?? ''}
          placeholder={t('settings.workflow.limit_placeholder')}
          onChange={(event) => onChange({ daily_document_limit: optionalPositiveInteger(event.target.value) })}
        />
      </FormField>
      <FormField label={t('settings.workflow.include_tags')} htmlFor={ids.include}>
        <input
          id={ids.include}
          value={value.rules.include_tags.join(', ')}
          onChange={(event) => onChange({ rules: { ...value.rules, include_tags: splitTags(event.target.value) } })}
          placeholder={t('settings.workflow.optional_tags')}
        />
      </FormField>
      <FormField label={t('settings.workflow.exclude_tags')} htmlFor={ids.exclude}>
        <input
          id={ids.exclude}
          value={value.rules.exclude_tags.join(', ')}
          onChange={(event) => onChange({ rules: { ...value.rules, exclude_tags: splitTags(event.target.value) } })}
          placeholder={t('settings.workflow.optional_tags')}
        />
      </FormField>
    </Section>
  );
}
