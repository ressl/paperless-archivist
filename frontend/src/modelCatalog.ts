import type { AiProvider, AiProviderKind, ModelCapability, RuntimeSettings } from './api/client';

export type { ModelCapability } from './api/client';

type ModelOption = {
  value: string;
  label: string;
  recommendation?: boolean;
};

type ProviderDescriptor = Pick<AiProvider, 'name' | 'kind' | 'base_url'>;

export const MINIMAX_M3_MODEL = 'ressl/MiniMax-M3-uncensored-NVFP4';
export const SGLANG_MINIMAX_M3_PROVIDER_NAME = 'sglang-minimax-m3';

function blankProviderTuning(): AiProvider['tuning'] {
  return {
    worker_concurrency: null,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
    reasoning_effort: null,
    max_output_tokens: null,
    structured_output: null,
    ocr_page_limit: null,
    hourly_document_limit: null,
    daily_document_limit: null,
    metadata_confidence_threshold: null,
    title_confidence_threshold: null,
    correspondent_confidence_threshold: null,
    document_type_confidence_threshold: null,
    document_date_confidence_threshold: null,
    tags_confidence_threshold: null,
    fields_confidence_threshold: null,
    max_tags: null,
    allowed_list_max: null,
    request_timeout_seconds: null
  };
}

const localOllamaProvider: ProviderDescriptor = {
  name: 'ollama',
  kind: 'ollama',
  base_url: 'http://ollama:11434'
};

const ollamaCloudProvider: AiProvider = {
  name: 'ollama-cloud',
  kind: 'ollama',
  base_url: 'https://ollama.com',
  default_text_model: 'glm-5.1',
  default_vision_model: 'qwen3-vl:235b-instruct',
  cost_per_1m_input_tokens_usd: null,
  cost_per_1m_output_tokens_usd: null,
  secret_id: null,
  // Injected as a UI suggestion only — must stay disabled so merely opening
  // Settings doesn't persist an enabled, unconfigured external provider on the
  // next save. The operator enables it explicitly after adding a key. (#272)
  enabled: false,
  tuning: {
    worker_concurrency: null,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
    reasoning_effort: 'medium',
    max_output_tokens: null,
    structured_output: null,
    ocr_page_limit: null,
    hourly_document_limit: null,
    daily_document_limit: null,
    metadata_confidence_threshold: null,
    title_confidence_threshold: null,
    correspondent_confidence_threshold: null,
    document_type_confidence_threshold: null,
    document_date_confidence_threshold: null,
    tags_confidence_threshold: null,
    fields_confidence_threshold: null,
    max_tags: null,
    allowed_list_max: null,
    request_timeout_seconds: null
  }
};

const sglangMinimaxM3Provider: AiProvider = {
  name: SGLANG_MINIMAX_M3_PROVIDER_NAME,
  kind: 'openai_compatible',
  base_url: '',
  default_text_model: MINIMAX_M3_MODEL,
  default_vision_model: null,
  cost_per_1m_input_tokens_usd: null,
  cost_per_1m_output_tokens_usd: null,
  secret_id: null,
  enabled: false,
  tuning: blankProviderTuning()
};

const builtInProviderNames = new Set([
  'ollama',
  'ollama-cloud',
  'openai',
  'anthropic',
  'openai-compatible',
  SGLANG_MINIMAX_M3_PROVIDER_NAME,
  'mineru'
]);

function normalizedProviderName(name: string) {
  return name.trim().toLowerCase();
}

export function isBuiltInProviderName(name: string) {
  return builtInProviderNames.has(normalizedProviderName(name));
}

export function isSglangMinimaxM3Provider(provider: ProviderDescriptor) {
  return (
    provider.kind === 'openai_compatible' &&
    normalizedProviderName(provider.name) === SGLANG_MINIMAX_M3_PROVIDER_NAME
  );
}

const localOllamaTextModels: ModelOption[] = [
  { value: 'qwen3:8b', label: 'qwen3:8b', recommendation: true },
  { value: 'qwen3:4b', label: 'qwen3:4b' },
  { value: 'qwen3:14b', label: 'qwen3:14b' },
  { value: 'qwen3:30b', label: 'qwen3:30b' },
  { value: 'qwen3:32b', label: 'qwen3:32b' },
  { value: 'qwen3:235b', label: 'qwen3:235b' },
  { value: 'qwen3-coder:30b', label: 'qwen3-coder:30b' },
  { value: 'qwen3-coder:480b', label: 'qwen3-coder:480b' },
  { value: 'qwen3-next:80b', label: 'qwen3-next:80b' },
  { value: 'gpt-oss:20b', label: 'gpt-oss:20b' },
  { value: 'gpt-oss:120b', label: 'gpt-oss:120b' },
  { value: 'gemma3:4b', label: 'gemma3:4b' },
  { value: 'gemma3:12b', label: 'gemma3:12b' },
  { value: 'gemma3:27b', label: 'gemma3:27b' },
  { value: 'llama3.3:70b', label: 'llama3.3:70b' },
  { value: 'llama3.2:3b', label: 'llama3.2:3b' },
  { value: 'mistral-small3.2:24b', label: 'mistral-small3.2:24b' },
  { value: 'deepseek-r1:8b', label: 'deepseek-r1:8b' },
  { value: 'deepseek-r1:14b', label: 'deepseek-r1:14b' },
  { value: 'deepseek-r1:32b', label: 'deepseek-r1:32b' },
  { value: 'deepseek-r1:70b', label: 'deepseek-r1:70b' }
];

const localOllamaVisionModels: ModelOption[] = [
  { value: 'qwen2.5vl:7b', label: 'qwen2.5vl:7b', recommendation: true },
  { value: 'qwen2.5vl:3b', label: 'qwen2.5vl:3b' },
  { value: 'qwen2.5vl:32b', label: 'qwen2.5vl:32b' },
  { value: 'qwen2.5vl:72b', label: 'qwen2.5vl:72b' },
  { value: 'qwen3-vl:8b', label: 'qwen3-vl:8b' },
  { value: 'qwen3-vl:30b', label: 'qwen3-vl:30b' },
  { value: 'qwen3-vl:235b', label: 'qwen3-vl:235b' },
  { value: 'gemma3:4b', label: 'gemma3:4b' },
  { value: 'gemma3:12b', label: 'gemma3:12b' },
  { value: 'gemma3:27b', label: 'gemma3:27b' },
  { value: 'llava:7b', label: 'llava:7b' },
  { value: 'llava:13b', label: 'llava:13b' },
  { value: 'llava:34b', label: 'llava:34b' }
];

const ollamaCloudTextModels: ModelOption[] = [
  { value: 'glm-5.1', label: 'glm-5.1', recommendation: true },
  { value: 'glm-5', label: 'glm-5' },
  { value: 'glm-4.7', label: 'glm-4.7' },
  { value: 'glm-4.6', label: 'glm-4.6' },
  { value: 'minimax-m2.7', label: 'minimax-m2.7' },
  { value: 'minimax-m2.5', label: 'minimax-m2.5' },
  { value: 'minimax-m2.1', label: 'minimax-m2.1' },
  { value: 'minimax-m2', label: 'minimax-m2' },
  { value: 'deepseek-v4-pro', label: 'deepseek-v4-pro' },
  { value: 'deepseek-v4-flash', label: 'deepseek-v4-flash' },
  { value: 'deepseek-v3.2', label: 'deepseek-v3.2' },
  { value: 'deepseek-v3.1:671b', label: 'deepseek-v3.1:671b' },
  { value: 'qwen3.5:397b', label: 'qwen3.5:397b' },
  { value: 'qwen3-coder:480b', label: 'qwen3-coder:480b' },
  { value: 'qwen3-coder-next', label: 'qwen3-coder-next' },
  { value: 'qwen3-next:80b', label: 'qwen3-next:80b' },
  { value: 'qwen3-vl:235b-instruct', label: 'qwen3-vl:235b-instruct' },
  { value: 'qwen3-vl:235b', label: 'qwen3-vl:235b' },
  { value: 'kimi-k2.6', label: 'kimi-k2.6' },
  { value: 'kimi-k2.5', label: 'kimi-k2.5' },
  { value: 'kimi-k2-thinking', label: 'kimi-k2-thinking' },
  { value: 'kimi-k2:1t', label: 'kimi-k2:1t' },
  { value: 'gpt-oss:120b', label: 'gpt-oss:120b' },
  { value: 'gpt-oss:20b', label: 'gpt-oss:20b' },
  { value: 'gemini-3-flash-preview', label: 'gemini-3-flash-preview' },
  { value: 'gemma4:31b', label: 'gemma4:31b' },
  { value: 'gemma3:27b', label: 'gemma3:27b' },
  { value: 'gemma3:12b', label: 'gemma3:12b' },
  { value: 'gemma3:4b', label: 'gemma3:4b' },
  { value: 'mistral-large-3:675b', label: 'mistral-large-3:675b' },
  { value: 'ministral-3:14b', label: 'ministral-3:14b' },
  { value: 'ministral-3:8b', label: 'ministral-3:8b' },
  { value: 'ministral-3:3b', label: 'ministral-3:3b' },
  { value: 'devstral-2:123b', label: 'devstral-2:123b' },
  { value: 'devstral-small-2:24b', label: 'devstral-small-2:24b' },
  { value: 'nemotron-3-super', label: 'nemotron-3-super' },
  { value: 'nemotron-3-nano:30b', label: 'nemotron-3-nano:30b' },
  { value: 'cogito-2.1:671b', label: 'cogito-2.1:671b' },
  { value: 'rnj-1:8b', label: 'rnj-1:8b' }
];

const ollamaCloudVisionModels: ModelOption[] = [
  { value: 'qwen3-vl:235b-instruct', label: 'qwen3-vl:235b-instruct', recommendation: true },
  { value: 'qwen3-vl:235b', label: 'qwen3-vl:235b' },
  { value: 'gemini-3-flash-preview', label: 'gemini-3-flash-preview' },
  { value: 'gemma4:31b', label: 'gemma4:31b' },
  { value: 'gemma3:27b', label: 'gemma3:27b' },
  { value: 'gemma3:12b', label: 'gemma3:12b' },
  { value: 'gemma3:4b', label: 'gemma3:4b' }
];

const openAiTextModels: ModelOption[] = [
  { value: 'gpt-5.5', label: 'gpt-5.5', recommendation: true },
  { value: 'gpt-5.5-pro', label: 'gpt-5.5-pro' },
  { value: 'gpt-5.4', label: 'gpt-5.4' },
  { value: 'gpt-5.4-pro', label: 'gpt-5.4-pro' },
  { value: 'gpt-5.4-mini', label: 'gpt-5.4-mini' },
  { value: 'gpt-5.4-nano', label: 'gpt-5.4-nano' },
  { value: 'gpt-5.2', label: 'gpt-5.2' },
  { value: 'gpt-5.2-pro', label: 'gpt-5.2-pro' },
  { value: 'gpt-5.1', label: 'gpt-5.1' },
  { value: 'gpt-5', label: 'gpt-5' },
  { value: 'gpt-5-pro', label: 'gpt-5-pro' },
  { value: 'gpt-5-mini', label: 'gpt-5-mini' },
  { value: 'gpt-5-nano', label: 'gpt-5-nano' },
  { value: 'gpt-4.1', label: 'gpt-4.1' },
  { value: 'gpt-4.1-mini', label: 'gpt-4.1-mini' },
  { value: 'gpt-4.1-nano', label: 'gpt-4.1-nano' },
  { value: 'o3-pro', label: 'o3-pro' },
  { value: 'o3', label: 'o3' },
  { value: 'o4-mini', label: 'o4-mini' },
  { value: 'o3-mini', label: 'o3-mini' },
  { value: 'o1-pro', label: 'o1-pro' },
  { value: 'o1', label: 'o1' },
  { value: 'o1-mini', label: 'o1-mini' },
  { value: 'gpt-4o', label: 'gpt-4o' },
  { value: 'gpt-4o-mini', label: 'gpt-4o-mini' },
  { value: 'gpt-4.5-preview', label: 'gpt-4.5-preview' },
  { value: 'gpt-4-turbo', label: 'gpt-4-turbo' },
  { value: 'gpt-4', label: 'gpt-4' },
  { value: 'gpt-3.5-turbo', label: 'gpt-3.5-turbo' }
];

const openAiVisionModels: ModelOption[] = [
  { value: 'gpt-5.5', label: 'gpt-5.5', recommendation: true },
  { value: 'gpt-5.5-pro', label: 'gpt-5.5-pro' },
  { value: 'gpt-5.4', label: 'gpt-5.4' },
  { value: 'gpt-5.4-pro', label: 'gpt-5.4-pro' },
  { value: 'gpt-5.4-mini', label: 'gpt-5.4-mini' },
  { value: 'gpt-5.4-nano', label: 'gpt-5.4-nano' },
  { value: 'gpt-5.2', label: 'gpt-5.2' },
  { value: 'gpt-5.2-pro', label: 'gpt-5.2-pro' },
  { value: 'gpt-5.1', label: 'gpt-5.1' },
  { value: 'gpt-5', label: 'gpt-5' },
  { value: 'gpt-5-pro', label: 'gpt-5-pro' },
  { value: 'gpt-5-mini', label: 'gpt-5-mini' },
  { value: 'gpt-5-nano', label: 'gpt-5-nano' },
  { value: 'gpt-4.1', label: 'gpt-4.1' },
  { value: 'gpt-4.1-mini', label: 'gpt-4.1-mini' },
  { value: 'gpt-4.1-nano', label: 'gpt-4.1-nano' },
  { value: 'o3', label: 'o3' },
  { value: 'o4-mini', label: 'o4-mini' },
  { value: 'gpt-4o', label: 'gpt-4o' },
  { value: 'gpt-4o-mini', label: 'gpt-4o-mini' },
  { value: 'gpt-4-turbo', label: 'gpt-4-turbo' }
];

const anthropicModels: ModelOption[] = [
  { value: 'claude-sonnet-4-6', label: 'claude-sonnet-4-6', recommendation: true },
  { value: 'claude-opus-4-7', label: 'claude-opus-4-7' },
  { value: 'claude-haiku-4-5-20251001', label: 'claude-haiku-4-5-20251001' },
  { value: 'claude-haiku-4-5', label: 'claude-haiku-4-5' },
  { value: 'claude-opus-4-6', label: 'claude-opus-4-6' },
  { value: 'claude-sonnet-4-5', label: 'claude-sonnet-4-5' },
  { value: 'claude-sonnet-4-5-20250929', label: 'claude-sonnet-4-5-20250929' },
  { value: 'claude-opus-4-1-20250805', label: 'claude-opus-4-1-20250805' },
  { value: 'claude-opus-4-20250514', label: 'claude-opus-4-20250514' },
  { value: 'claude-sonnet-4-20250514', label: 'claude-sonnet-4-20250514' },
  { value: 'claude-3-7-sonnet-20250219', label: 'claude-3-7-sonnet-20250219' },
  { value: 'claude-3-5-sonnet-20241022', label: 'claude-3-5-sonnet-20241022' },
  { value: 'claude-3-5-haiku-20241022', label: 'claude-3-5-haiku-20241022' },
  { value: 'claude-3-opus-20240229', label: 'claude-3-opus-20240229' },
  { value: 'claude-3-sonnet-20240229', label: 'claude-3-sonnet-20240229' },
  { value: 'claude-3-haiku-20240307', label: 'claude-3-haiku-20240307' }
];

const openAiCompatibleTextModels: ModelOption[] = [
  { value: 'qwen3:8b', label: 'qwen3:8b', recommendation: true },
  { value: MINIMAX_M3_MODEL, label: MINIMAX_M3_MODEL },
  { value: 'qwen3:14b', label: 'qwen3:14b' },
  { value: 'qwen3:30b', label: 'qwen3:30b' },
  { value: 'qwen3:32b', label: 'qwen3:32b' },
  { value: 'qwen3-coder:480b', label: 'qwen3-coder:480b' },
  { value: 'gpt-oss:20b', label: 'gpt-oss:20b' },
  { value: 'gpt-oss:120b', label: 'gpt-oss:120b' },
  { value: 'llama3.3:70b', label: 'llama3.3:70b' },
  { value: 'mistral-large-latest', label: 'mistral-large-latest' },
  { value: 'deepseek-chat', label: 'deepseek-chat' }
];

const openAiCompatibleVisionModels: ModelOption[] = [
  { value: 'qwen2.5vl:7b', label: 'qwen2.5vl:7b', recommendation: true },
  { value: 'qwen2.5vl:3b', label: 'qwen2.5vl:3b' },
  { value: 'qwen2.5vl:32b', label: 'qwen2.5vl:32b' },
  { value: 'qwen2.5vl:72b', label: 'qwen2.5vl:72b' },
  { value: 'qwen3-vl:235b', label: 'qwen3-vl:235b' },
  { value: 'gpt-4o', label: 'gpt-4o' },
  { value: 'gpt-4o-mini', label: 'gpt-4o-mini' }
];

const modelOptionsByProvider: Record<AiProviderKind, Record<ModelCapability, ModelOption[]>> = {
  ollama: {
    text: localOllamaTextModels,
    vision: localOllamaVisionModels
  },
  openai: {
    text: openAiTextModels,
    vision: openAiVisionModels
  },
  anthropic: {
    text: anthropicModels,
    vision: anthropicModels
  },
  openai_compatible: {
    text: openAiCompatibleTextModels,
    vision: openAiCompatibleVisionModels
  },
  // Mineru is vision-only OCR (no chat/text capability) with a fixed vision
  // model ("mineru") rendered as a disabled input, not a picker — there is
  // no catalog of selectable models, so no picker options are needed. Empty
  // arrays keep this Record total (required by AiProviderKind) without
  // inventing catalog data; modelOptions()'s "current custom" fallback still
  // renders the backend-provided default_vision_model ("mineru") safely.
  mineru: {
    text: [],
    vision: []
  }
};

export function isOllamaCloudProvider(provider: ProviderDescriptor) {
  return (
    provider.kind === 'ollama' &&
    (provider.name.toLowerCase().includes('cloud') ||
      provider.name.toLowerCase().includes('commercial') ||
      provider.base_url.trim().replace(/\/+$/, '') === 'https://ollama.com')
  );
}

function baseModelOptions(provider: ProviderDescriptor, capability: ModelCapability) {
  if (isOllamaCloudProvider(provider)) {
    return capability === 'text' ? ollamaCloudTextModels : ollamaCloudVisionModels;
  }
  return modelOptionsByProvider[provider.kind][capability];
}

export function recommendedModel(provider: ProviderDescriptor, capability: ModelCapability) {
  // Mineru is a fixed, vision-only OCR backend: it has no text capability and
  // no model catalog to pick from — the vision "model" is really just the
  // provider kind itself. Special-case it ahead of the catalog lookup so the
  // ProviderCard's disabled vision input and the kind-switch handler both get
  // a stable, non-empty value without inventing catalog entries.
  if (provider.kind === 'mineru') {
    return capability === 'vision' ? 'mineru' : '';
  }
  const options = baseModelOptions(provider, capability);
  return options.find((option) => option.recommendation)?.value ?? options[0]?.value ?? '';
}

export function modelOptions(provider: ProviderDescriptor, capability: ModelCapability, current?: string | null) {
  const options = baseModelOptions(provider, capability);
  if (!current || options.some((option) => option.value === current)) return options;
  return [{ value: current, label: `${current} (current custom)` }, ...options];
}

export function modelOptionLabel(option: ModelOption) {
  return option.recommendation ? `${option.label} (Recommendation)` : option.label;
}

export function defaultProvider(settings: RuntimeSettings): ProviderDescriptor {
  return settings.ai.providers.find((provider) => provider.name === settings.ai.default_provider) ?? localOllamaProvider;
}

export function providerDefaults(kind: AiProviderKind): Pick<AiProvider, 'default_text_model' | 'default_vision_model'> {
  const provider: ProviderDescriptor = { name: kind, kind, base_url: '' };
  return {
    default_text_model: recommendedModel(provider, 'text'),
    default_vision_model: recommendedModel(provider, 'vision')
  };
}

export function withModelDefaults(settings: RuntimeSettings): RuntimeSettings {
  const knownProviders = [...settings.ai.providers];
  const security = settings.security as Partial<RuntimeSettings['security']> | undefined;
  const notifications = settings.notifications as Partial<RuntimeSettings['notifications']> | undefined;
  const paperless = settings.paperless as Partial<RuntimeSettings['paperless']>;
  const fields = settings.fields as Partial<RuntimeSettings['fields']>;
  if (
    !knownProviders.some(
      (provider) => normalizedProviderName(provider.name) === ollamaCloudProvider.name
    )
  ) {
    knownProviders.push(ollamaCloudProvider);
  }
  if (
    !knownProviders.some(
      (provider) =>
        normalizedProviderName(provider.name) === SGLANG_MINIMAX_M3_PROVIDER_NAME
    )
  ) {
    knownProviders.push(sglangMinimaxM3Provider);
  }
  const providers = knownProviders.map((provider) => ({
    ...provider,
    default_text_model: isSglangMinimaxM3Provider(provider)
      ? provider.default_text_model
      : provider.default_text_model || recommendedModel(provider, 'text'),
    default_vision_model:
      provider.default_vision_model ||
      (isSglangMinimaxM3Provider(provider) ? null : recommendedModel(provider, 'vision'))
  }));
  const modelCatalog = [...(settings.ai.model_catalog ?? [])];
  if (
    !modelCatalog.some(
      (entry) =>
        entry.provider_kind === 'openai_compatible' &&
        entry.capability === 'text' &&
        entry.model_id === MINIMAX_M3_MODEL
    )
  ) {
    modelCatalog.push({
      provider_kind: 'openai_compatible',
      capability: 'text',
      model_id: MINIMAX_M3_MODEL,
      recommended: false,
      modality: 'text',
      best_for: 'MiniMax M3 served by SGLang'
    });
  }
  const defaultProviderName = normalizedProviderName(settings.ai.default_provider);
  const selectedProvider =
    providers.find((provider) => normalizedProviderName(provider.name) === defaultProviderName) ??
    localOllamaProvider;
  return {
    ...settings,
    paperless: {
      ...settings.paperless,
      delta_sync_enabled: false,
      delta_sync_overlap_minutes: 5,
      active_archive: 'default',
      archive_profiles: [],
      ...paperless
    },
    security: {
      audit_retention_days: 365,
      ai_artifact_retention_days: 30,
      runs_retention_days: 365,
      ai_artifact_storage: 'redacted',
      api_token_expiry_required: true,
      api_token_default_ttl_days: 90,
      api_token_max_ttl_days: 365,
      ...(security ?? {})
    },
    notifications: {
      enabled: false,
      webhook_url_secret_id: null,
      review_queue_threshold: 10,
      repeated_failure_threshold: 3,
      cooldown_minutes: 60,
      ...(notifications ?? {})
    },
    workflow: {
      ...settings.workflow,
      rules: settings.workflow.rules ?? { include_tags: [], exclude_tags: [] }
    },
    fields: {
      ...settings.fields,
      mappings: [],
      ...fields
    },
    ai: {
      ...settings.ai,
      default_text_model: settings.ai.default_text_model || recommendedModel(selectedProvider, 'text'),
      default_vision_model: settings.ai.default_vision_model || recommendedModel(selectedProvider, 'vision'),
      providers,
      model_catalog: modelCatalog
    }
  };
}
