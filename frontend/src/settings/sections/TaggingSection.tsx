import { useId, useMemo } from 'react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField, NumberField, Section } from '../../lib/ui';
import { languageOptionLabel, languageOptions } from '../../data/worldLanguages';

export function TaggingSection({
  value,
  onChange
}: {
  value: RuntimeSettings['tagging'];
  onChange: (patch: Partial<RuntimeSettings['tagging']>) => void;
}) {
  const { t, locale } = useI18n();
  const worldLanguages = useMemo(() => languageOptions(locale), [locale]);
  const listId = useId();
  const ids = {
    maxTags: useId(),
    confidence: useId(),
    language: useId()
  };
  return (
    <Section title={t('settings.workflow.section.tagging')}>
      <FormField label={t('settings.workflow.max_tags')} htmlFor={ids.maxTags}>
        <NumberField
          id={ids.maxTags}
          min={1}
          max={20}
          value={value.max_tags}
          onCommit={(max_tags) => onChange({ max_tags })}
        />
      </FormField>
      <FormField label={t('settings.workflow.tag_confidence')} htmlFor={ids.confidence}>
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
        label={t('settings.workflow.tag_output_language')}
        help={t('settings.workflow.tag_output_hint')}
        htmlFor={ids.language}
      >
        <input
          id={ids.language}
          list={listId}
          value={value.tag_output_language}
          onChange={(event) => onChange({ tag_output_language: event.target.value })}
          placeholder={t('settings.workflow.tag_output_placeholder')}
        />
        <datalist id={listId}>
          {worldLanguages.map((language) => (
            <option key={language.tag} value={language.tag}>
              {languageOptionLabel(language)}
            </option>
          ))}
        </datalist>
      </FormField>
      <label className="inline">
        <input
          type="checkbox"
          checked={value.allow_new_tags}
          onChange={(event) => onChange({ allow_new_tags: event.target.checked })}
        />
        {t('settings.workflow.allow_new_tags')}
      </label>
    </Section>
  );
}
