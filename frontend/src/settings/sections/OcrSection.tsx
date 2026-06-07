import { useId } from 'react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField, Section } from '../../lib/ui';

export function OcrSection({
  value,
  onChange
}: {
  value: RuntimeSettings['ocr'];
  onChange: (patch: Partial<RuntimeSettings['ocr']>) => void;
}) {
  const { t } = useI18n();
  const pagesId = useId();
  return (
    <Section title={t('settings.workflow.section.ocr')}>
      <FormField label={t('settings.workflow.ocr_pages')} htmlFor={pagesId}>
        <input
          id={pagesId}
          type="number"
          min="1"
          max="20"
          value={value.page_limit}
          onChange={(event) => onChange({ page_limit: Number(event.target.value) })}
        />
      </FormField>
    </Section>
  );
}
