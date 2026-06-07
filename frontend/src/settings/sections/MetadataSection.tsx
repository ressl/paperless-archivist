import { useId } from 'react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField, Section } from '../../lib/ui';

export function MetadataSection({
  value,
  onChange
}: {
  value: RuntimeSettings['metadata'];
  onChange: (patch: Partial<RuntimeSettings['metadata']>) => void;
}) {
  const { t } = useI18n();
  const ids = {
    confidence: useId(),
    dateConfidence: useId()
  };
  return (
    <Section title={t('settings.workflow.section.metadata')}>
      <FormField label={t('settings.workflow.metadata_confidence')} htmlFor={ids.confidence}>
        <input
          id={ids.confidence}
          type="number"
          min="0"
          max="1"
          step="0.05"
          value={value.confidence_threshold}
          onChange={(event) => onChange({ confidence_threshold: Number(event.target.value) })}
        />
      </FormField>
      <FormField label={t('settings.workflow.date_confidence')} htmlFor={ids.dateConfidence}>
        <input
          id={ids.dateConfidence}
          type="number"
          min="0"
          max="1"
          step="0.05"
          value={value.document_date_confidence_threshold}
          onChange={(event) => onChange({ document_date_confidence_threshold: Number(event.target.value) })}
        />
      </FormField>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.overwrite_existing_correspondent}
          onChange={(event) => onChange({ overwrite_existing_correspondent: event.target.checked })}
        />
        {t('settings.workflow.overwrite_correspondent')}
      </label>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.overwrite_existing_document_type}
          onChange={(event) => onChange({ overwrite_existing_document_type: event.target.checked })}
        />
        {t('settings.workflow.overwrite_document_type')}
      </label>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.overwrite_existing_document_date}
          onChange={(event) => onChange({ overwrite_existing_document_date: event.target.checked })}
        />
        {t('settings.workflow.overwrite_document_date')}
      </label>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.allow_new_correspondents}
          onChange={(event) => onChange({ allow_new_correspondents: event.target.checked })}
        />
        {t('settings.workflow.allow_new_correspondents')}
      </label>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.allow_new_document_types}
          onChange={(event) => onChange({ allow_new_document_types: event.target.checked })}
        />
        {t('settings.workflow.allow_new_document_types')}
      </label>
    </Section>
  );
}
