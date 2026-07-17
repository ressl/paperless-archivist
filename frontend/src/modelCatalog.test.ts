import { describe, expect, it } from 'vitest';
import type { RuntimeSettings } from './api/client';
import {
  MINIMAX_M3_MODEL,
  SGLANG_MINIMAX_M3_PROVIDER_NAME,
  modelOptions,
  recommendedModel,
  withModelDefaults
} from './modelCatalog';

function oldSettings(): RuntimeSettings {
  return {
    ai: {
      default_provider: 'ollama',
      default_text_model: 'qwen3:8b',
      default_vision_model: 'qwen2.5vl:7b',
      providers: [],
      model_catalog: []
    },
    paperless: {},
    security: {},
    notifications: {},
    workflow: {},
    fields: {}
  } as unknown as RuntimeSettings;
}

describe('SGLang MiniMax M3 model defaults', () => {
  it('adds the exact M3 model without replacing the generic Qwen recommendation', () => {
    const provider = {
      name: SGLANG_MINIMAX_M3_PROVIDER_NAME,
      kind: 'openai_compatible' as const,
      base_url: ''
    };
    const options = modelOptions(provider, 'text');

    expect(options.map((option) => option.value)).toContain(MINIMAX_M3_MODEL);
    expect(recommendedModel(provider, 'text')).toBe('qwen3:8b');
    expect(modelOptions(provider, 'vision').map((option) => option.value)).not.toContain(
      MINIMAX_M3_MODEL
    );
  });

  it('upgrades old settings once with a disabled text-only preset and catalog entry', () => {
    const upgraded = withModelDefaults(withModelDefaults(oldSettings()));
    const presets = upgraded.ai.providers.filter(
      (provider) => provider.name === SGLANG_MINIMAX_M3_PROVIDER_NAME
    );
    const catalogEntries = upgraded.ai.model_catalog.filter(
      (entry) =>
        entry.provider_kind === 'openai_compatible' &&
        entry.capability === 'text' &&
        entry.model_id === MINIMAX_M3_MODEL
    );

    expect(presets).toHaveLength(1);
    expect(presets[0]).toMatchObject({
      kind: 'openai_compatible',
      base_url: '',
      default_text_model: MINIMAX_M3_MODEL,
      default_vision_model: null,
      secret_id: null,
      enabled: false,
      tuning: {
        worker_concurrency: 1,
        reasoning_effort: null,
        max_output_tokens: 4096,
        structured_output: 'auto',
        request_timeout_seconds: 180
      }
    });
    expect(catalogEntries).toHaveLength(1);
    expect(catalogEntries[0].recommended).toBe(false);
  });

  it('preserves a case-renamed operator preset and custom catalog metadata', () => {
    const settings = oldSettings();
    settings.ai.providers.push({
      name: ' SGLANG-MINIMAX-M3 ',
      kind: 'openai_compatible',
      base_url: 'https://operator.example.test/v1',
      default_text_model: null,
      default_vision_model: null,
      cost_per_1m_input_tokens_usd: null,
      cost_per_1m_output_tokens_usd: null,
      secret_id: null,
      enabled: false,
      tuning: {} as RuntimeSettings['ai']['providers'][number]['tuning']
    });
    settings.ai.model_catalog.push({
      provider_kind: 'openai_compatible',
      capability: 'text',
      model_id: MINIMAX_M3_MODEL,
      label: 'Operator label',
      recommended: false,
      context: 'custom context',
      modality: 'text',
    });

    const upgraded = withModelDefaults(settings);
    const matchingProviders = upgraded.ai.providers.filter(
      (provider) => provider.name.trim().toLowerCase() === SGLANG_MINIMAX_M3_PROVIDER_NAME
    );
    const matchingCatalog = upgraded.ai.model_catalog.filter(
      (entry) => entry.model_id === MINIMAX_M3_MODEL
    );

    expect(matchingProviders).toHaveLength(1);
    expect(matchingProviders[0].base_url).toBe('https://operator.example.test/v1');
    expect(matchingProviders[0].default_text_model).toBeNull();
    expect(matchingCatalog).toHaveLength(1);
    expect(matchingCatalog[0].label).toBe('Operator label');
    expect(matchingCatalog[0].context).toBe('custom context');
  });
});
