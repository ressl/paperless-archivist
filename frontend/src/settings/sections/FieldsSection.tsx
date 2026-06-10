import { useEffect, useId, useState } from 'react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField, NumberField, Section } from '../../lib/ui';
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
        <NumberField
          id={ids.maxFields}
          min={1}
          max={50}
          value={value.max_fields}
          onCommit={(max_fields) => onChange({ max_fields })}
        />
      </FormField>
      <FormField label={t('settings.workflow.field_confidence')} htmlFor={ids.confidence}>
        <NumberField
          id={ids.confidence}
          min={0}
          max={1}
          step={0.05}
          integer={false}
          value={value.confidence_threshold}
          onCommit={(confidence_threshold) => onChange({ confidence_threshold })}
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
