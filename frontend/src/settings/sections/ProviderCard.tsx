import { useId } from 'react';
import { AiProviderKind, ModelCatalogEntry, ProviderTuning, RuntimeSettings } from '../../api/client';
import { recommendedModel } from '../../modelCatalog';
import { useI18n } from '../../i18n/I18nProvider';
import { FormField } from '../../lib/ui';
import { ProviderModelSelect } from './ProviderModelSelect';
import { TuningDisclosure, type TuningField } from './tuning';
import { optionalNumber } from './helpers';
import type { OllamaModelLoadState } from './types';

type Provider = RuntimeSettings['ai']['providers'][number];

export function ProviderCard({
  provider,
  catalog,
  globals,
  ollamaState,
  secret,
  onSecretChange,
  onChangeProvider,
  onChangeTuning,
  onResetTuningBlock,
  onRefreshModels
}: {
  provider: Provider;
  catalog: ModelCatalogEntry[];
  globals: RuntimeSettings;
  ollamaState?: OllamaModelLoadState;
  secret: string;
  onSecretChange: (value: string) => void;
  onChangeProvider: (patch: Partial<Provider>) => void;
  onChangeTuning: (patch: Partial<ProviderTuning>) => void;
  onResetTuningBlock: (fields: readonly TuningField[]) => void;
  onRefreshModels: () => void;
}) {
  const { t } = useI18n();
  const ids = {
    name: useId(),
    kind: useId(),
    baseUrl: useId(),
    inputCost: useId(),
    outputCost: useId(),
    apiKey: useId()
  };
  return (
    <fieldset className="card">
      <legend>{provider.name || t('settings.provider.provider')}</legend>
      <FormField label={t('settings.provider.name')} htmlFor={ids.name}>
        <input id={ids.name} value={provider.name} onChange={(event) => onChangeProvider({ name: event.target.value })} />
      </FormField>
      <FormField label={t('settings.provider.kind')} htmlFor={ids.kind}>
        <select
          id={ids.kind}
          value={provider.kind}
          onChange={(event) => {
            const kind = event.target.value as AiProviderKind;
            const nextProvider = { ...provider, kind };
            onChangeProvider({
              kind,
              default_text_model: recommendedModel(nextProvider, 'text'),
              default_vision_model: recommendedModel(nextProvider, 'vision')
            });
          }}
        >
          <option value="ollama">ollama</option>
          <option value="openai">openai</option>
          <option value="anthropic">anthropic</option>
          <option value="openai_compatible">openai compatible</option>
          <option value="mineru">mineru (OCR)</option>
        </select>
      </FormField>
      <FormField
        label={t('settings.provider.base_url')}
        htmlFor={ids.baseUrl}
        help={provider.kind === 'mineru' ? t('settings.provider.mineru_base_url_hint') : undefined}
      >
        <input
          id={ids.baseUrl}
          value={provider.base_url}
          onChange={(event) => onChangeProvider({ base_url: event.target.value })}
        />
      </FormField>
      <FormField label={t('settings.provider.input_cost')} htmlFor={ids.inputCost}>
        <input
          id={ids.inputCost}
          type="number"
          min="0"
          step="0.01"
          value={provider.cost_per_1m_input_tokens_usd ?? ''}
          onChange={(event) => onChangeProvider({ cost_per_1m_input_tokens_usd: optionalNumber(event.target.value) })}
        />
      </FormField>
      <FormField label={t('settings.provider.output_cost')} htmlFor={ids.outputCost}>
        <input
          id={ids.outputCost}
          type="number"
          min="0"
          step="0.01"
          value={provider.cost_per_1m_output_tokens_usd ?? ''}
          onChange={(event) => onChangeProvider({ cost_per_1m_output_tokens_usd: optionalNumber(event.target.value) })}
        />
      </FormField>
      {provider.kind !== 'mineru' && (
        <div className="settings-field">
          {t('settings.provider.text_model')}
          <ProviderModelSelect
            capability="text"
            provider={provider}
            value={provider.default_text_model ?? ''}
            catalog={catalog}
            ollamaState={ollamaState}
            onChange={(value) => onChangeProvider({ default_text_model: value })}
            onRefresh={onRefreshModels}
          />
        </div>
      )}
      <div className="settings-field">
        {t('settings.provider.vision_model')}
        {provider.kind === 'mineru' ? (
          <input value="mineru" disabled aria-label={t('settings.provider.vision_model')} />
        ) : (
          <ProviderModelSelect
            capability="vision"
            provider={provider}
            value={provider.default_vision_model ?? ''}
            catalog={catalog}
            ollamaState={ollamaState}
            onChange={(value) => onChangeProvider({ default_vision_model: value })}
            onRefresh={onRefreshModels}
          />
        )}
      </div>
      <FormField label={t('settings.provider.api_key')} htmlFor={ids.apiKey}>
        <input
          id={ids.apiKey}
          type="password"
          value={secret}
          placeholder={provider.secret_id ? t('settings.paperless.configured') : ''}
          onChange={(event) => onSecretChange(event.target.value)}
        />
      </FormField>
      <label className="inline">
        <input
          type="checkbox"
          checked={provider.enabled}
          onChange={(event) => onChangeProvider({ enabled: event.target.checked })}
        />
        {t('settings.provider.enabled')}
      </label>
      <TuningDisclosure
        provider={provider}
        globals={globals}
        onChangeTuning={onChangeTuning}
        onResetBlock={onResetTuningBlock}
      />
    </fieldset>
  );
}
