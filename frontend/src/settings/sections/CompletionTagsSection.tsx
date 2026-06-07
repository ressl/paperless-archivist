import { useId } from 'react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField, Section } from '../../lib/ui';

export function CompletionTagsSection({
  value,
  onChange
}: {
  value: RuntimeSettings['workflow']['tags'];
  onChange: (patch: Record<string, string>) => void;
}) {
  const { t } = useI18n();
  const ids = {
    ocr: useId(),
    metadata: useId()
  };
  return (
    <Section title={t('settings.completion_tags')}>
      <small>{t('settings.completion_tags.hint')}</small>
      <FormField label={t('settings.completion_tags.ocr')} htmlFor={ids.ocr}>
        <input
          id={ids.ocr}
          value={value.completion_ocr ?? 'archivist-ocr'}
          onChange={(event) => onChange({ completion_ocr: event.target.value })}
          placeholder="archivist-ocr"
        />
      </FormField>
      <FormField label={t('settings.completion_tags.metadata')} htmlFor={ids.metadata}>
        <input
          id={ids.metadata}
          value={value.completion_metadata ?? 'archivist-metadata'}
          onChange={(event) => onChange({ completion_metadata: event.target.value })}
          placeholder="archivist-metadata"
        />
      </FormField>
    </Section>
  );
}
