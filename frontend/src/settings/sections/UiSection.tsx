import type { RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField, NumberField, Section } from '../../lib/ui';

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
      {/* #450: informational budget alert on the dashboard; nothing is throttled. */}
      <FormField label={t('settings.ui.cost_budget')} help={t('settings.ui.cost_budget_hint')} htmlFor="ui-cost-budget">
        <NumberField
          id="ui-cost-budget"
          nullable
          integer={false}
          min={0}
          step={1}
          value={value?.monthly_cost_budget_usd ?? null}
          onCommit={(budget) => onChange({ monthly_cost_budget_usd: budget && budget > 0 ? budget : null })}
        />
      </FormField>
      <FormField label={t('settings.ui.cost_budget_warning_percent')} htmlFor="ui-cost-budget-warning">
        <NumberField
          id="ui-cost-budget-warning"
          min={1}
          max={100}
          value={value?.cost_budget_warning_percent ?? 80}
          onCommit={(percent) => onChange({ cost_budget_warning_percent: percent })}
        />
      </FormField>
    </Section>
  );
}
