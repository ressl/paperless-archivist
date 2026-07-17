import { useId } from 'react';
import { Activity } from 'lucide-react';
import type { ModelCatalogEntry, RuntimeSettings } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { Button, FormField, NumberField, Section } from '../../lib/ui';
import { ConnectionTestFeedback } from './ConnectionTestFeedback';
import { ProviderModelSelect } from './ProviderModelSelect';
import type { ConnectionTestState, ModelProviderDescriptor, OllamaModelLoadState } from './types';

export function AiDefaultsSection({
  ai,
  onChange,
  selectedProvider,
  ollamaState,
  onSelectDefaultProvider,
  onRefreshModels,
  test,
  onTest,
  errors = {}
}: {
  ai: RuntimeSettings['ai'];
  onChange: (patch: Partial<RuntimeSettings['ai']>) => void;
  selectedProvider: ModelProviderDescriptor;
  ollamaState?: OllamaModelLoadState;
  onSelectDefaultProvider: (name: string) => void;
  onRefreshModels: () => void;
  test: ConnectionTestState | null;
  onTest: () => void;
  errors?: { defaultProvider?: string; ollamaBaseUrl?: string };
}) {
  const { t } = useI18n();
  const ids = {
    provider: useId(),
    ollamaUrl: useId(),
    visionCtx: useId(),
    textCtx: useId()
  };
  const providerErrorId = `${ids.provider}-error`;
  const ollamaUrlErrorId = `${ids.ollamaUrl}-error`;
  const catalog: ModelCatalogEntry[] = ai.model_catalog;
  const testing = test?.status === 'running';
  return (
    <Section title={t('settings.ai_defaults')}>
      <FormField
        label={t('settings.ai.default_provider')}
        htmlFor={ids.provider}
        error={errors.defaultProvider}
        errorId={providerErrorId}
      >
        <select
          id={ids.provider}
          value={ai.default_provider}
          aria-label={t('settings.ai.default_provider')}
          aria-invalid={Boolean(errors.defaultProvider)}
          aria-describedby={errors.defaultProvider ? providerErrorId : undefined}
          onChange={(event) => onSelectDefaultProvider(event.target.value)}
        >
          {ai.providers.map((provider) => (
            <option key={provider.name} value={provider.name}>
              {provider.name}
            </option>
          ))}
        </select>
      </FormField>
      <FormField
        label={t('settings.ai.legacy_ollama_url')}
        htmlFor={ids.ollamaUrl}
        error={errors.ollamaBaseUrl}
        errorId={ollamaUrlErrorId}
      >
        <input
          id={ids.ollamaUrl}
          value={ai.ollama_base_url}
          aria-label={t('settings.ai.legacy_ollama_url')}
          aria-invalid={Boolean(errors.ollamaBaseUrl)}
          aria-describedby={errors.ollamaBaseUrl ? ollamaUrlErrorId : undefined}
          onChange={(event) => onChange({ ollama_base_url: event.target.value })}
        />
      </FormField>
      <div className="settings-field">
        {t('settings.ai.fallback_text_model')}
        <ProviderModelSelect
          capability="text"
          provider={selectedProvider}
          value={ai.default_text_model}
          catalog={catalog}
          ollamaState={ollamaState}
          onChange={(value) => onChange({ default_text_model: value })}
          onRefresh={onRefreshModels}
        />
      </div>
      <div className="settings-field">
        {t('settings.ai.fallback_vision_model')}
        <ProviderModelSelect
          capability="vision"
          provider={selectedProvider}
          value={ai.default_vision_model}
          catalog={catalog}
          ollamaState={ollamaState}
          onChange={(value) => onChange({ default_vision_model: value })}
          onRefresh={onRefreshModels}
        />
      </div>
      <div className="settings-field">
        {t('settings.ai.vision_crash_fallback_model')}
        <ProviderModelSelect
          capability="vision"
          provider={selectedProvider}
          value={ai.fallback_vision_model ?? ''}
          catalog={catalog}
          ollamaState={ollamaState}
          onChange={(value) => onChange({ fallback_vision_model: value.trim() === '' ? undefined : value })}
          onRefresh={onRefreshModels}
        />
        <small>{t('settings.ai.vision_crash_fallback_hint')}</small>
      </div>
      <label className="inline">
        <input
          type="checkbox"
          checked={ai.requeue_vision_crashes_on_startup ?? true}
          onChange={(event) => onChange({ requeue_vision_crashes_on_startup: event.target.checked })}
        />
        {t('settings.ai.requeue_vision_crashes_on_startup')}
      </label>
      <FormField
        label={t('settings.ai.ollama_vision_num_ctx')}
        help={t('settings.ai.ollama_vision_num_ctx_hint')}
        htmlFor={ids.visionCtx}
      >
        <NumberField
          id={ids.visionCtx}
          min={2048}
          max={131072}
          step={1024}
          value={ai.ollama_vision_num_ctx ?? 16384}
          onCommit={(ollama_vision_num_ctx) => onChange({ ollama_vision_num_ctx })}
        />
      </FormField>
      <FormField
        label={t('settings.ai.ollama_text_num_ctx')}
        help={t('settings.ai.ollama_text_num_ctx_hint')}
        htmlFor={ids.textCtx}
      >
        <NumberField
          id={ids.textCtx}
          min={2048}
          max={131072}
          step={1024}
          value={ai.ollama_text_num_ctx ?? 8192}
          onCommit={(ollama_text_num_ctx) => onChange({ ollama_text_num_ctx })}
        />
      </FormField>
      <Button variant="secondary" icon={<Activity size={16} />} title={t('generic.test')} disabled={testing} onClick={onTest}>
        {testing ? t('generic.testing') : t('generic.test')}
      </Button>
      <ConnectionTestFeedback state={test} />
    </Section>
  );
}
