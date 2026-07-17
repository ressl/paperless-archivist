import { useId } from 'react';
import { Trash2 } from 'lucide-react';
import { AiProviderKind, ModelCatalogEntry, ProviderTuning, RuntimeSettings } from '../../api/client';
import { isSglangMinimaxM3Provider, recommendedModel } from '../../modelCatalog';
import { useI18n } from '../../i18n/I18nProvider';
import { Button, FormField } from '../../lib/ui';
import { ProviderModelSelect } from './ProviderModelSelect';
import { TuningDisclosure, type TuningField } from './tuning';
import { optionalNumber } from './helpers';
import type { OllamaModelLoadState } from './types';

type Provider = RuntimeSettings['ai']['providers'][number];
export type ProviderFieldErrors = { name?: string; baseUrl?: string };

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
  onRefreshModels,
  errors = {},
  builtIn,
  removalError,
  onRemove
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
  errors?: ProviderFieldErrors;
  builtIn: boolean;
  removalError?: string;
  onRemove: () => void;
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
  const nameErrorId = `${ids.name}-error`;
  const baseUrlErrorId = `${ids.baseUrl}-error`;
  return (
    <fieldset className="card">
      <legend>{provider.name.trim() || t('settings.provider.provider')}</legend>
      <FormField label={t('settings.provider.name')} htmlFor={ids.name} error={errors.name} errorId={nameErrorId}>
        <input
          id={ids.name}
          value={provider.name}
          disabled={builtIn}
          aria-label={t('settings.provider.name')}
          aria-invalid={Boolean(errors.name)}
          aria-describedby={errors.name ? nameErrorId : undefined}
          onChange={(event) => onChangeProvider({ name: event.target.value })}
        />
      </FormField>
      <FormField label={t('settings.provider.kind')} htmlFor={ids.kind}>
        <select
          id={ids.kind}
          value={provider.kind}
          disabled={builtIn}
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
        error={errors.baseUrl}
        errorId={baseUrlErrorId}
      >
        <input
          id={ids.baseUrl}
          value={provider.base_url}
          aria-label={t('settings.provider.base_url')}
          aria-invalid={Boolean(errors.baseUrl)}
          aria-describedby={errors.baseUrl ? baseUrlErrorId : undefined}
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
        {provider.kind === 'mineru' || isSglangMinimaxM3Provider(provider) ? (
          <input
            value={provider.kind === 'mineru' ? 'mineru' : (provider.default_vision_model ?? '')}
            disabled
            aria-label={t('settings.provider.vision_model')}
          />
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
      {builtIn ? (
        <p className="field-hint">{t('settings.provider.builtin_disable_only')}</p>
      ) : (
        <Button
          type="button"
          variant="secondary"
          icon={<Trash2 size={16} aria-hidden="true" />}
          onClick={onRemove}
        >
          {t('settings.provider.remove')}
        </Button>
      )}
      {removalError && (
        <p className="field-hint error" role="alert">
          {removalError}
        </p>
      )}
      <TuningDisclosure
        provider={provider}
        globals={globals}
        onChangeTuning={onChangeTuning}
        onResetBlock={onResetTuningBlock}
      />
    </fieldset>
  );
}
