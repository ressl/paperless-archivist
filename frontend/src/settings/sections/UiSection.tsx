import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { Section } from '../../lib/ui';

export function UiSection({
  value,
  onChange
}: {
  value: RuntimeSettings['ui'] | undefined;
  onChange: (patch: Partial<NonNullable<RuntimeSettings['ui']>>) => void;
}) {
  const { t } = useI18n();
  return (
    <Section title={t('settings.ui')}>
      <label className="inline">
        <input
          type="checkbox"
          checked={value?.debug_console_enabled ?? false}
          onChange={(event) => onChange({ debug_console_enabled: event.target.checked })}
        />
        <span>{t('settings.ui.debug_console_enabled')}</span>
      </label>
      <small className="field-hint">{t('settings.ui.debug_console_enabled_hint')}</small>
    </Section>
  );
}
