import { useEffect, useId, useState } from 'react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField, Section } from '../../lib/ui';
import { parseFieldMappings, serializeFieldMappings } from './helpers';

export function FieldsSection({
  value,
  onChange
}: {
  value: RuntimeSettings['fields'];
  onChange: (patch: Partial<RuntimeSettings['fields']>) => void;
}) {
  const { t } = useI18n();
  const ids = {
    maxFields: useId(),
    confidence: useId(),
    mappings: useId()
  };
  // Hold the raw textarea text while editing and only parse on blur. Parsing
  // on every keystroke (parse -> serialize round-trip) swallowed Enter and
  // blank lines, so a new mapping line could not be started by keyboard. (#265)
  const serializedMappings = serializeFieldMappings(value.mappings);
  const [mappingsDraft, setMappingsDraft] = useState(serializedMappings);
  useEffect(() => {
    setMappingsDraft(serializedMappings);
  }, [serializedMappings]);
  return (
    <Section title={t('settings.workflow.section.fields')}>
      <FormField label={t('settings.workflow.max_fields')} htmlFor={ids.maxFields}>
        <input
          id={ids.maxFields}
          type="number"
          min="1"
          max="50"
          value={value.max_fields}
          onChange={(event) => onChange({ max_fields: Number(event.target.value) })}
        />
      </FormField>
      <FormField label={t('settings.workflow.field_confidence')} htmlFor={ids.confidence}>
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
      <FormField
        label={t('settings.workflow.field_mappings')}
        help={t('settings.workflow.field_mappings_hint')}
        htmlFor={ids.mappings}
      >
        <textarea
          id={ids.mappings}
          rows={5}
          value={mappingsDraft}
          onChange={(event) => setMappingsDraft(event.target.value)}
          onBlur={() => onChange({ mappings: parseFieldMappings(mappingsDraft) })}
          placeholder={t('settings.workflow.field_mappings_placeholder')}
        />
      </FormField>
    </Section>
  );
}
