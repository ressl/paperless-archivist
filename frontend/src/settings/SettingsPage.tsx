import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  Activity,
  Check,
  Database,
  Info,
  RefreshCw,
  RotateCcw,
  Save,
  Send,
  UserPlus,
  X
} from 'lucide-react';
import {
  api,
  AiProviderKind,
  AiRuntimeHints,
  ModelCatalogEntry,
  ModelUsageTier,
  OllamaInstalledModel,
  ProviderTuning,
  ReasoningEffort,
  RuntimeSettings
} from '../api/client';
import {
  defaultProvider,
  isOllamaCloudProvider,
  modelOptionLabel,
  modelOptions,
  providerDefaults,
  recommendedModel,
  withModelDefaults
} from '../modelCatalog';
import hardwareRecommendations from '../hardwareRecommendations.json';
import { languageOptionLabel, languageOptions } from '../data/worldLanguages';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { ActionButton, PageHeader, errorToString, localizedErrorMessage, run } from '../lib/ui';
import { LanguageSelector } from '../lib/LanguageSelector';
import { workflowModeDescription, workflowModeOptions } from '../lib/workflow';

type ModelCapability = 'text' | 'vision';

type ModelProviderDescriptor = Pick<RuntimeSettings['ai']['providers'][number], 'name' | 'kind' | 'base_url'>;

type OllamaModelLoadState = {
  loading: boolean;
  loaded: boolean;
  models: OllamaInstalledModel[];
  error: string | null;
};

type ConnectionTestState = {
  status: 'idle' | 'running' | 'success' | 'error';
  title: string;
  description: string;
  hints: string[];
  details?: string;
};

type HardwareRecommendationProfile = {
  id: string;
  label: string;
  title: string;
  items: Array<{
    label: string;
    model: string;
  }>;
};

type HardwareRecommendationData = {
  profiles: HardwareRecommendationProfile[];
};

const recommendationProfile = (hardwareRecommendations as HardwareRecommendationData).profiles[0];

// ---------------------------------------------------------------------------
// Provider tuning presets (v1.6.2). Mirror `ai_provider_defaults` in the
// backend; see docs/PROVIDER_TUNING_PLAN.md. The "Reset to defaults" buttons
// in the Tuning disclosure write the values for the provider's kind.
//
// Every field is explicitly listed even when null so the reset action also
// clears any operator-supplied overrides for that sub-block.
// ---------------------------------------------------------------------------

type TuningPresetKind = 'ollama' | 'ollama_cloud' | 'openai' | 'anthropic' | 'openai_compatible';

const TUNING_PRESETS: Record<TuningPresetKind, ProviderTuning> = {
  ollama: {
    worker_concurrency: 2,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: 4096,
    vision_num_ctx: 4096,
    ocr_page_limit: 2,
    hourly_document_limit: 200,
    daily_document_limit: 2000,
    metadata_confidence_threshold: null,
    title_confidence_threshold: null,
    correspondent_confidence_threshold: null,
    document_type_confidence_threshold: null,
    document_date_confidence_threshold: null,
    tags_confidence_threshold: null,
    fields_confidence_threshold: null,
    max_tags: null,
    allowed_list_max: null
  },
  ollama_cloud: {
    worker_concurrency: 4,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
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
    allowed_list_max: null
  },
  openai: {
    worker_concurrency: 8,
    consensus_secondary_text_model: 'gpt-4o-mini',
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
    ocr_page_limit: 8,
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
    allowed_list_max: null
  },
  anthropic: {
    worker_concurrency: 4,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
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
    allowed_list_max: null
  },
  openai_compatible: {
    worker_concurrency: 4,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
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
    allowed_list_max: null
  }
};

// Fields owned by each sub-block. Used by the per-block "Reset to defaults"
// buttons so each reset only touches its own keys.
const PERFORMANCE_FIELDS = [
  'worker_concurrency',
  'consensus_secondary_text_model',
  'consensus_date_tolerance_days',
  'text_num_ctx',
  'vision_num_ctx',
  'reasoning_effort'
] as const satisfies readonly (keyof ProviderTuning)[];

const CAPS_FIELDS = [
  'ocr_page_limit',
  'hourly_document_limit',
  'daily_document_limit'
] as const satisfies readonly (keyof ProviderTuning)[];

const THRESHOLD_FIELDS = [
  'metadata_confidence_threshold',
  'title_confidence_threshold',
  'correspondent_confidence_threshold',
  'document_type_confidence_threshold',
  'document_date_confidence_threshold',
  'tags_confidence_threshold',
  'fields_confidence_threshold',
  'max_tags',
  'allowed_list_max'
] as const satisfies readonly (keyof ProviderTuning)[];

function tuningPresetKindFor(
  provider: Pick<RuntimeSettings['ai']['providers'][number], 'kind' | 'name' | 'base_url'>
): TuningPresetKind {
  if (provider.kind === 'ollama' && isOllamaCloudProvider(provider)) return 'ollama_cloud';
  return provider.kind;
}

export function SettingsPage({ setError }: { setError: (error: string | null) => void }) {
  const { t, locale } = useI18n();
  const [settings, setSettings] = useState<RuntimeSettings | null>(null);
  const [savedSettings, setSavedSettings] = useState<RuntimeSettings | null>(null);
  const [token, setToken] = useState('');
  const [providerSecrets, setProviderSecrets] = useState<Record<string, string>>({});
  const [notificationWebhook, setNotificationWebhook] = useState('');
  const [ollamaModels, setOllamaModels] = useState<Record<string, OllamaModelLoadState>>({});
  const [paperlessTest, setPaperlessTest] = useState<ConnectionTestState | null>(null);
  const [providerTest, setProviderTest] = useState<ConnectionTestState | null>(null);
  const [notificationTest, setNotificationTest] = useState<ConnectionTestState | null>(null);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const worldLanguages = useMemo(() => languageOptions(locale), [locale]);

  const loadOllamaModels = (providerName: string) => {
    setOllamaModels((current) => ({
      ...current,
      [providerName]: {
        loading: true,
        loaded: current[providerName]?.loaded ?? false,
        models: current[providerName]?.models ?? [],
        error: null
      }
    }));
    return api
      .ollamaModels(providerName)
      .then((data) => {
        setOllamaModels((current) => ({
          ...current,
          [providerName]: {
            loading: false,
            loaded: true,
            models: data.models,
            error: null
          }
        }));
      })
      .catch(() => {
        setOllamaModels((current) => ({
          ...current,
          [providerName]: {
            loading: false,
            loaded: true,
            models: current[providerName]?.models ?? [],
            error: t('settings.ollama.load_error')
          }
        }));
      });
  };

  const refreshInstalledOllamaModels = (nextSettings: RuntimeSettings) => {
    const providerNames = Array.from(
      new Set(
        nextSettings.ai.providers
          .filter((provider) => provider.kind === 'ollama' && !isOllamaCloudProvider(provider))
          .map((provider) => provider.name)
          .filter(Boolean)
      )
    );
    void Promise.allSettled(providerNames.map((providerName) => loadOllamaModels(providerName)));
  };

  useEffect(() => {
    api
      .settings()
      .then((data) => {
        const nextSettings = withModelDefaults(data);
        setSettings(nextSettings);
        setSavedSettings(nextSettings);
        refreshInstalledOllamaModels(nextSettings);
      })
      .catch((err) => setError(localizedErrorMessage(err, t)));
  }, [setError]);

  if (!settings) return <section className="page"><PageHeader title={t('settings.loading_title')} /></section>;

  const update = (updater: (settings: RuntimeSettings) => RuntimeSettings) => setSettings((current) => (current ? updater(current) : current));
  const updateCatalog = (next: ModelCatalogEntry[]) =>
    update((s) => ({ ...s, ai: { ...s.ai, model_catalog: next } }));
  const updateProvider = (index: number, patch: Partial<RuntimeSettings['ai']['providers'][number]>) =>
    update((s) => {
      const providers = [...s.ai.providers];
      providers[index] = { ...providers[index], ...patch };
      return { ...s, ai: { ...s.ai, providers } };
    });
  const updateProviderTuning = (index: number, patch: Partial<ProviderTuning>) =>
    update((s) => {
      const providers = [...s.ai.providers];
      const current = providers[index];
      const tuning: ProviderTuning = { ...(current.tuning ?? {}), ...patch };
      providers[index] = { ...current, tuning };
      return { ...s, ai: { ...s.ai, providers } };
    });
  const resetProviderTuningBlock = (index: number, fields: readonly (keyof ProviderTuning)[]) =>
    update((s) => {
      const providers = [...s.ai.providers];
      const current = providers[index];
      const presetKind = tuningPresetKindFor(current);
      const preset = TUNING_PRESETS[presetKind];
      const tuning: ProviderTuning = { ...(current.tuning ?? {}) };
      for (const field of fields) {
        // Type-safe write: each field is a key on ProviderTuning and the
        // preset map carries the same field. Cast through `any` to satisfy
        // TS's per-field union; the satisfies above keeps the names honest.
        (tuning as Record<string, unknown>)[field] = (preset as Record<string, unknown>)[field] ?? null;
      }
      providers[index] = { ...current, tuning };
      return { ...s, ai: { ...s.ai, providers } };
    });
  const selectDefaultProvider = (name: string) =>
    update((s) => {
      const provider = s.ai.providers.find((entry) => entry.name === name);
      const selectedProvider = provider ?? { name: 'ollama', kind: 'ollama' as AiProviderKind, base_url: s.ai.ollama_base_url };
      return {
        ...s,
        ai: {
          ...s.ai,
          default_provider: name,
          default_text_model: provider?.default_text_model || recommendedModel(selectedProvider, 'text'),
          default_vision_model: provider?.default_vision_model || recommendedModel(selectedProvider, 'vision')
        }
      };
    });
  const openAiCompatibleDefaults = providerDefaults('openai_compatible');
  const addProvider = () =>
    update((s) => ({
      ...s,
      ai: {
        ...s.ai,
        providers: [
          ...s.ai.providers,
          {
            name: `provider-${s.ai.providers.length + 1}`,
            kind: 'openai_compatible',
            base_url: '',
            default_text_model: openAiCompatibleDefaults.default_text_model,
            default_vision_model: openAiCompatibleDefaults.default_vision_model,
            secret_id: null,
            enabled: true
          }
        ]
      }
    }));
  const selectedDefaultProvider = defaultProvider(settings);
  const runPaperlessTest = () => {
    if (savedSettings && paperlessSettingsChanged(settings, savedSettings, token)) {
      setPaperlessTest(paperlessUnsavedSettingsFeedback(settings, savedSettings, token, t));
      return;
    }
    const baseUrlProblem = paperlessBaseUrlProblem(settings.paperless.base_url);
    if (baseUrlProblem) {
      setPaperlessTest(paperlessBaseUrlProblemFeedback(baseUrlProblem, t));
      return;
    }
    setPaperlessTest({
      status: 'running',
      title: t('settings.paperless.test_running.title'),
      description: t('settings.paperless.test_running.description'),
      hints: [t('settings.paperless.test_running.hint')]
    });
    api
      .testPaperless()
      .then((data) => {
        setPaperlessTest(data.ok ? paperlessTestSuccess(t) : paperlessTestFailure(data.error, t));
      })
      .catch((err) => {
        setPaperlessTest(paperlessTestFailure(errorToString(err), t));
      });
  };
  const runProviderTest = () => {
    setProviderTest({
      status: 'running',
      title: t('settings.provider.test_running.title'),
      description: t('settings.provider.test_running.description', { provider: selectedDefaultProvider.name }),
      hints: [t('settings.provider.test_running.hint')]
    });
    api
      .testProvider()
      .then((data) => {
        setProviderTest(data.ok ? providerTestSuccess(selectedDefaultProvider, t) : providerTestFailure(selectedDefaultProvider, data.error, t));
      })
      .catch((err) => {
        setProviderTest(providerTestFailure(selectedDefaultProvider, errorToString(err), t));
      });
  };
  const runNotificationTest = () => {
    setNotificationTest({
      status: 'running',
      title: t('settings.notifications.test_running.title'),
      description: t('settings.notifications.test_running.description'),
      hints: [t('settings.notifications.test_running.hint')]
    });
    api
      .testNotification()
      .then((data) => {
        setNotificationTest(data.ok ? {
          status: 'success',
          title: t('settings.notifications.success.title'),
          description: t('settings.notifications.success.description'),
          hints: [t('settings.notifications.success.hint')]
        } : {
          status: 'error',
          title: t('settings.notifications.failure.title'),
          description: t('settings.notifications.failure.description'),
          hints: [
            t('settings.notifications.failure.hint_url'),
            t('settings.notifications.failure.hint_reachable'),
            t('settings.notifications.failure.hint_saved')
          ],
          details: sanitizeConnectionDetail(data.error ?? t('generic.request_failed'))
        });
      })
      .catch((err) => {
        setNotificationTest({
          status: 'error',
          title: t('settings.notifications.failure.title'),
          description: t('settings.notifications.failure.description'),
          hints: [
            t('settings.notifications.failure.hint_url'),
            t('settings.notifications.failure.hint_reachable'),
            t('settings.notifications.failure.hint_saved')
          ],
          details: sanitizeConnectionDetail(errorToString(err))
        });
      });
  };
  const firstRunSteps = firstRunWizardSteps(settings, savedSettings, selectedDefaultProvider, t);

  return (
    <section className="page">
      <PageHeader title={t('settings.title')} />
      <FirstRunWizard steps={firstRunSteps} />
      <div className="settings-language-row">
        <LanguageSelector compact />
      </div>
      <div className="settings-grid">
        <fieldset>
          <legend>{t('settings.paperless')}</legend>
          <label>
            {t('settings.paperless.base_url')}
            <input value={settings.paperless.base_url} onChange={(event) => update((s) => ({ ...s, paperless: { ...s.paperless, base_url: event.target.value } }))} />
          </label>
          <p className="field-hint">
            {t('settings.paperless.base_url_hint')}
          </p>
          <label>
            {t('settings.paperless.api_token')}
            <input value={token} type="password" onChange={(event) => setToken(event.target.value)} placeholder={settings.paperless.token_secret_id ? t('settings.paperless.configured') : ''} />
          </label>
          <label className="inline">
            <input
              type="checkbox"
              checked={settings.paperless.login_bridge_enabled}
              onChange={(event) => update((s) => ({ ...s, paperless: { ...s.paperless, login_bridge_enabled: event.target.checked } }))}
            />
            {t('settings.paperless.login_bridge')}
          </label>
          <label className="inline">
            <input
              type="checkbox"
              checked={settings.paperless.delta_sync_enabled}
              onChange={(event) => update((s) => ({ ...s, paperless: { ...s.paperless, delta_sync_enabled: event.target.checked } }))}
            />
            {t('settings.paperless.delta_sync')}
          </label>
          <label>
            {t('settings.paperless.delta_overlap')}
            <input
              type="number"
              min="0"
              max="1440"
              value={settings.paperless.delta_sync_overlap_minutes}
              onChange={(event) => update((s) => ({ ...s, paperless: { ...s.paperless, delta_sync_overlap_minutes: Number(event.target.value) } }))}
            />
          </label>
          <label>
            {t('settings.paperless.active_archive')}
            <input
              value={settings.paperless.active_archive}
              onChange={(event) => update((s) => ({ ...s, paperless: { ...s.paperless, active_archive: event.target.value } }))}
            />
          </label>
          <button title={t('generic.test')} disabled={paperlessTest?.status === 'running'} onClick={runPaperlessTest}>
            <Database size={16} /> {paperlessTest?.status === 'running' ? t('generic.testing') : t('generic.test')}
          </button>
          <ConnectionTestFeedback state={paperlessTest} />
        </fieldset>
        <fieldset>
          <legend>{t('settings.ai_defaults')}</legend>
          <label>
            {t('settings.ai.default_provider')}
            <select value={settings.ai.default_provider} onChange={(event) => selectDefaultProvider(event.target.value)}>
              {settings.ai.providers.map((provider) => (
                <option key={provider.name} value={provider.name}>{provider.name}</option>
              ))}
            </select>
          </label>
          <label>
            {t('settings.ai.legacy_ollama_url')}
            <input value={settings.ai.ollama_base_url} onChange={(event) => update((s) => ({ ...s, ai: { ...s.ai, ollama_base_url: event.target.value } }))} />
          </label>
          <div className="settings-field">
            {t('settings.ai.fallback_text_model')}
            <ProviderModelSelect
              capability="text"
              provider={selectedDefaultProvider}
              value={settings.ai.default_text_model}
              catalog={settings.ai.model_catalog}
              ollamaState={ollamaModels[selectedDefaultProvider.name]}
              onChange={(value) => update((s) => ({ ...s, ai: { ...s.ai, default_text_model: value } }))}
              onRefresh={() => loadOllamaModels(selectedDefaultProvider.name)}
            />
          </div>
          <div className="settings-field">
            {t('settings.ai.fallback_vision_model')}
            <ProviderModelSelect
              capability="vision"
              provider={selectedDefaultProvider}
              value={settings.ai.default_vision_model}
              catalog={settings.ai.model_catalog}
              ollamaState={ollamaModels[selectedDefaultProvider.name]}
              onChange={(value) => update((s) => ({ ...s, ai: { ...s.ai, default_vision_model: value } }))}
              onRefresh={() => loadOllamaModels(selectedDefaultProvider.name)}
            />
          </div>
          <div className="settings-field">
            {t('settings.ai.vision_crash_fallback_model')}
            <ProviderModelSelect
              capability="vision"
              provider={selectedDefaultProvider}
              value={settings.ai.fallback_vision_model ?? ''}
              catalog={settings.ai.model_catalog}
              ollamaState={ollamaModels[selectedDefaultProvider.name]}
              onChange={(value) =>
                update((s) => ({
                  ...s,
                  ai: { ...s.ai, fallback_vision_model: value.trim() === '' ? null : value }
                }))
              }
              onRefresh={() => loadOllamaModels(selectedDefaultProvider.name)}
            />
            <small>{t('settings.ai.vision_crash_fallback_hint')}</small>
          </div>
          <label className="inline">
            <input
              type="checkbox"
              checked={settings.ai.requeue_vision_crashes_on_startup ?? true}
              onChange={(event) =>
                update((s) => ({
                  ...s,
                  ai: { ...s.ai, requeue_vision_crashes_on_startup: event.target.checked }
                }))
              }
            />
            {t('settings.ai.requeue_vision_crashes_on_startup')}
          </label>
          <label>
            {t('settings.ai.ollama_vision_num_ctx')}
            <input
              type="number"
              min="2048"
              max="131072"
              step="1024"
              value={settings.ai.ollama_vision_num_ctx ?? 16384}
              onChange={(event) =>
                update((s) => ({
                  ...s,
                  ai: { ...s.ai, ollama_vision_num_ctx: Number(event.target.value) }
                }))
              }
            />
            <small>{t('settings.ai.ollama_vision_num_ctx_hint')}</small>
          </label>
          <label>
            {t('settings.ai.ollama_text_num_ctx')}
            <input
              type="number"
              min="2048"
              max="131072"
              step="1024"
              value={settings.ai.ollama_text_num_ctx ?? 8192}
              onChange={(event) =>
                update((s) => ({
                  ...s,
                  ai: { ...s.ai, ollama_text_num_ctx: Number(event.target.value) }
                }))
              }
            />
            <small>{t('settings.ai.ollama_text_num_ctx_hint')}</small>
          </label>
          <button title={t('generic.test')} disabled={providerTest?.status === 'running'} onClick={runProviderTest}>
            <Activity size={16} /> {providerTest?.status === 'running' ? t('generic.testing') : t('generic.test')}
          </button>
          <ConnectionTestFeedback state={providerTest} />
        </fieldset>
        <fieldset>
          <legend>{t('settings.workflow')}</legend>
          <label>
            {t('settings.workflow.mode')}
            <select value={settings.workflow.mode} onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, mode: event.target.value as RuntimeSettings['workflow']['mode'] } }))}>
              {workflowModeOptions.map((option) => (
                <option key={option.value} value={option.value}>
                  {t(option.labelKey)}
                </option>
              ))}
            </select>
            <small>{workflowModeDescription(settings.workflow.mode, t)}</small>
          </label>
          <label className="inline">
            <input type="checkbox" checked={settings.workflow.paused} onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, paused: event.target.checked } }))} />
            {t('settings.workflow.paused')}
          </label>
          <label className="inline">
            <input type="checkbox" checked={settings.workflow.dry_run} onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, dry_run: event.target.checked } }))} />
            {t('settings.workflow.dry_run')}
          </label>
          <label>
            {t('settings.workflow.hourly_limit')}
            <input
              type="number"
              min="1"
              value={settings.workflow.hourly_document_limit ?? ''}
              placeholder={t('settings.workflow.limit_placeholder')}
              onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, hourly_document_limit: optionalPositiveInteger(event.target.value) } }))}
            />
          </label>
          <label>
            {t('settings.workflow.daily_limit')}
            <input
              type="number"
              min="1"
              value={settings.workflow.daily_document_limit ?? ''}
              placeholder={t('settings.workflow.limit_placeholder')}
              onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, daily_document_limit: optionalPositiveInteger(event.target.value) } }))}
            />
          </label>
          <label>
            {t('settings.workflow.ocr_pages')}
            <input type="number" min="1" max="20" value={settings.ocr.page_limit} onChange={(event) => update((s) => ({ ...s, ocr: { ...s.ocr, page_limit: Number(event.target.value) } }))} />
          </label>
          <label>
            {t('settings.workflow.max_tags')}
            <input type="number" min="1" max="20" value={settings.tagging.max_tags} onChange={(event) => update((s) => ({ ...s, tagging: { ...s.tagging, max_tags: Number(event.target.value) } }))} />
          </label>
          <label>
            {t('settings.workflow.tag_confidence')}
            <input type="number" min="0" max="1" step="0.05" value={settings.tagging.confidence_threshold} onChange={(event) => update((s) => ({ ...s, tagging: { ...s.tagging, confidence_threshold: Number(event.target.value) } }))} />
          </label>
          <label>
            {t('settings.workflow.tag_output_language')}
            <input
              list="tag-output-language-options"
              value={settings.tagging.tag_output_language}
              onChange={(event) => update((s) => ({ ...s, tagging: { ...s.tagging, tag_output_language: event.target.value } }))}
              placeholder={t('settings.workflow.tag_output_placeholder')}
            />
            <datalist id="tag-output-language-options">
              {worldLanguages.map((language) => (
                <option key={language.tag} value={language.tag}>
                  {languageOptionLabel(language)}
                </option>
              ))}
            </datalist>
            <small>{t('settings.workflow.tag_output_hint')}</small>
          </label>
          <label>
            {t('settings.workflow.max_fields')}
            <input type="number" min="1" max="50" value={settings.fields.max_fields} onChange={(event) => update((s) => ({ ...s, fields: { ...s.fields, max_fields: Number(event.target.value) } }))} />
          </label>
          <label>
            {t('settings.workflow.field_confidence')}
            <input type="number" min="0" max="1" step="0.05" value={settings.fields.confidence_threshold} onChange={(event) => update((s) => ({ ...s, fields: { ...s.fields, confidence_threshold: Number(event.target.value) } }))} />
          </label>
          <label>
            {t('settings.workflow.field_mappings')}
            <textarea
              rows={5}
              value={serializeFieldMappings(settings.fields.mappings)}
              onChange={(event) => update((s) => ({ ...s, fields: { ...s.fields, mappings: parseFieldMappings(event.target.value) } }))}
              placeholder={t('settings.workflow.field_mappings_placeholder')}
            />
            <small>{t('settings.workflow.field_mappings_hint')}</small>
          </label>
          <label>
            {t('settings.workflow.metadata_confidence')}
            <input type="number" min="0" max="1" step="0.05" value={settings.metadata.confidence_threshold} onChange={(event) => update((s) => ({ ...s, metadata: { ...s.metadata, confidence_threshold: Number(event.target.value) } }))} />
          </label>
          <label>
            {t('settings.workflow.date_confidence')}
            <input type="number" min="0" max="1" step="0.05" value={settings.metadata.document_date_confidence_threshold} onChange={(event) => update((s) => ({ ...s, metadata: { ...s.metadata, document_date_confidence_threshold: Number(event.target.value) } }))} />
          </label>
          <label className="inline">
            <input type="checkbox" checked={settings.metadata.overwrite_existing_correspondent} onChange={(event) => update((s) => ({ ...s, metadata: { ...s.metadata, overwrite_existing_correspondent: event.target.checked } }))} />
            {t('settings.workflow.overwrite_correspondent')}
          </label>
          <label className="inline">
            <input type="checkbox" checked={settings.metadata.overwrite_existing_document_type} onChange={(event) => update((s) => ({ ...s, metadata: { ...s.metadata, overwrite_existing_document_type: event.target.checked } }))} />
            {t('settings.workflow.overwrite_document_type')}
          </label>
          <label className="inline">
            <input type="checkbox" checked={settings.metadata.overwrite_existing_document_date} onChange={(event) => update((s) => ({ ...s, metadata: { ...s.metadata, overwrite_existing_document_date: event.target.checked } }))} />
            {t('settings.workflow.overwrite_document_date')}
          </label>
          <label className="inline">
            <input type="checkbox" checked={settings.metadata.allow_new_correspondents} onChange={(event) => update((s) => ({ ...s, metadata: { ...s.metadata, allow_new_correspondents: event.target.checked } }))} />
            {t('settings.workflow.allow_new_correspondents')}
          </label>
          <label className="inline">
            <input type="checkbox" checked={settings.metadata.allow_new_document_types} onChange={(event) => update((s) => ({ ...s, metadata: { ...s.metadata, allow_new_document_types: event.target.checked } }))} />
            {t('settings.workflow.allow_new_document_types')}
          </label>
          <label className="inline">
            <input type="checkbox" checked={settings.tagging.allow_new_tags} onChange={(event) => update((s) => ({ ...s, tagging: { ...s.tagging, allow_new_tags: event.target.checked } }))} />
            {t('settings.workflow.allow_new_tags')}
          </label>
          <label>
            {t('settings.workflow.include_tags')}
            <input
              value={settings.workflow.rules.include_tags.join(', ')}
              onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, rules: { ...s.workflow.rules, include_tags: splitTags(event.target.value) } } }))}
              placeholder={t('settings.workflow.optional_tags')}
            />
          </label>
          <label>
            {t('settings.workflow.exclude_tags')}
            <input
              value={settings.workflow.rules.exclude_tags.join(', ')}
              onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, rules: { ...s.workflow.rules, exclude_tags: splitTags(event.target.value) } } }))}
              placeholder={t('settings.workflow.optional_tags')}
            />
          </label>
        </fieldset>
        <fieldset>
          <legend>{t('settings.completion_tags')}</legend>
          <small>{t('settings.completion_tags.hint')}</small>
          <label>
            {t('settings.completion_tags.ocr')}
            <input
              value={settings.workflow.tags.completion_ocr ?? 'archivist-ocr'}
              onChange={(event) => update((s) => ({
                ...s,
                workflow: {
                  ...s.workflow,
                  tags: { ...s.workflow.tags, completion_ocr: event.target.value }
                }
              }))}
              placeholder="archivist-ocr"
            />
          </label>
          <label>
            {t('settings.completion_tags.metadata')}
            <input
              value={settings.workflow.tags.completion_metadata ?? 'archivist-metadata'}
              onChange={(event) => update((s) => ({
                ...s,
                workflow: {
                  ...s.workflow,
                  tags: { ...s.workflow.tags, completion_metadata: event.target.value }
                }
              }))}
              placeholder="archivist-metadata"
            />
          </label>
        </fieldset>
        <fieldset>
          <legend>{t('settings.notifications')}</legend>
          <label className="inline">
            <input
              type="checkbox"
              checked={settings.notifications.enabled}
              onChange={(event) => update((s) => ({ ...s, notifications: { ...s.notifications, enabled: event.target.checked } }))}
            />
            {t('settings.notifications.enabled')}
          </label>
          <label>
            {t('settings.notifications.webhook_url')}
            <input
              value={notificationWebhook}
              type="password"
              onChange={(event) => setNotificationWebhook(event.target.value)}
              placeholder={settings.notifications.webhook_url_secret_id ? t('settings.paperless.configured') : 'https://hooks.example.com/...'}
            />
            <small>{t('settings.notifications.webhook_hint')}</small>
          </label>
          <label>
            {t('settings.notifications.review_threshold')}
            <input
              type="number"
              min="1"
              value={settings.notifications.review_queue_threshold}
              onChange={(event) => update((s) => ({ ...s, notifications: { ...s.notifications, review_queue_threshold: Number(event.target.value) } }))}
            />
          </label>
          <label>
            {t('settings.notifications.failure_threshold')}
            <input
              type="number"
              min="1"
              value={settings.notifications.repeated_failure_threshold}
              onChange={(event) => update((s) => ({ ...s, notifications: { ...s.notifications, repeated_failure_threshold: Number(event.target.value) } }))}
            />
          </label>
          <label>
            {t('settings.notifications.cooldown')}
            <input
              type="number"
              min="1"
              max="1440"
              value={settings.notifications.cooldown_minutes}
              onChange={(event) => update((s) => ({ ...s, notifications: { ...s.notifications, cooldown_minutes: Number(event.target.value) } }))}
            />
          </label>
          <button title={t('generic.test')} disabled={notificationTest?.status === 'running'} onClick={runNotificationTest}>
            <Send size={16} /> {notificationTest?.status === 'running' ? t('generic.testing') : t('generic.test')}
          </button>
          <ConnectionTestFeedback state={notificationTest} />
        </fieldset>
        <fieldset>
          <legend>{t('settings.security')}</legend>
          <label>
            {t('settings.security.audit_retention')}
            <input
              type="number"
              min="30"
              max="3650"
              value={settings.security.audit_retention_days}
              onChange={(event) => update((s) => ({ ...s, security: { ...s.security, audit_retention_days: Number(event.target.value) } }))}
            />
          </label>
          <label>
            {t('settings.security.ai_artifact_retention')}
            <input
              type="number"
              min="1"
              max="365"
              value={settings.security.ai_artifact_retention_days}
              onChange={(event) => update((s) => ({ ...s, security: { ...s.security, ai_artifact_retention_days: Number(event.target.value) } }))}
            />
          </label>
          <label>
            {t('settings.security.ai_artifact_storage')}
            <select
              value={settings.security.ai_artifact_storage}
              onChange={(event) => update((s) => ({ ...s, security: { ...s.security, ai_artifact_storage: event.target.value as RuntimeSettings['security']['ai_artifact_storage'] } }))}
            >
              <option value="redacted">{t('settings.security.storage.redacted')}</option>
              <option value="metadata_only">{t('settings.security.storage.metadata_only')}</option>
              <option value="full">{t('settings.security.storage.full')}</option>
            </select>
            <small>{t('settings.security.hint')}</small>
          </label>
          <label className="inline">
            <input
              type="checkbox"
              checked={settings.security.api_token_expiry_required}
              onChange={(event) => update((s) => ({ ...s, security: { ...s.security, api_token_expiry_required: event.target.checked } }))}
            />
            {t('settings.security.token_expiry_required')}
          </label>
          <label>
            {t('settings.security.token_default_ttl')}
            <input
              type="number"
              min="1"
              max="365"
              value={settings.security.api_token_default_ttl_days}
              onChange={(event) => update((s) => ({ ...s, security: { ...s.security, api_token_default_ttl_days: Number(event.target.value) } }))}
            />
          </label>
          <label>
            {t('settings.security.token_max_ttl')}
            <input
              type="number"
              min="1"
              max="3650"
              value={settings.security.api_token_max_ttl_days}
              onChange={(event) => update((s) => ({ ...s, security: { ...s.security, api_token_max_ttl_days: Number(event.target.value) } }))}
            />
          </label>
        </fieldset>
        <fieldset>
          <legend>{t('settings.ui')}</legend>
          <label className="inline">
            <input
              type="checkbox"
              checked={settings.ui?.debug_console_enabled ?? false}
              onChange={(event) => update((s) => ({
                ...s,
                ui: { ...(s.ui ?? {}), debug_console_enabled: event.target.checked }
              }))}
            />
            <span>{t('settings.ui.debug_console_enabled')}</span>
          </label>
          <small className="field-hint">{t('settings.ui.debug_console_enabled_hint')}</small>
        </fieldset>
      </div>
      <PageHeader title={t('settings.providers')} />
      <div className="provider-list">
        {settings.ai.providers.map((provider, index) => (
          <fieldset key={`${provider.name}-${index}`}>
            <legend>{provider.name || t('settings.provider.provider')}</legend>
            <label>
              {t('settings.provider.name')}
              <input value={provider.name} onChange={(event) => updateProvider(index, { name: event.target.value })} />
            </label>
            <label>
              {t('settings.provider.kind')}
              <select
                value={provider.kind}
                onChange={(event) => {
                  const kind = event.target.value as AiProviderKind;
                  const nextProvider = { ...provider, kind };
                  updateProvider(index, {
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
              </select>
            </label>
            <label>
              {t('settings.provider.base_url')}
              <input value={provider.base_url} onChange={(event) => updateProvider(index, { base_url: event.target.value })} />
            </label>
            <label>
              {t('settings.provider.input_cost')}
              <input
                type="number"
                min="0"
                step="0.01"
                value={provider.cost_per_1m_input_tokens_usd ?? ''}
                onChange={(event) => updateProvider(index, { cost_per_1m_input_tokens_usd: optionalNumber(event.target.value) })}
              />
            </label>
            <label>
              {t('settings.provider.output_cost')}
              <input
                type="number"
                min="0"
                step="0.01"
                value={provider.cost_per_1m_output_tokens_usd ?? ''}
                onChange={(event) => updateProvider(index, { cost_per_1m_output_tokens_usd: optionalNumber(event.target.value) })}
              />
            </label>
            <div className="settings-field">
              {t('settings.provider.text_model')}
              <ProviderModelSelect
                capability="text"
                provider={provider}
                value={provider.default_text_model ?? ''}
                catalog={settings.ai.model_catalog}
                ollamaState={ollamaModels[provider.name]}
                onChange={(value) => updateProvider(index, { default_text_model: value })}
                onRefresh={() => loadOllamaModels(provider.name)}
              />
            </div>
            <div className="settings-field">
              {t('settings.provider.vision_model')}
              <ProviderModelSelect
                capability="vision"
                provider={provider}
                value={provider.default_vision_model ?? ''}
                catalog={settings.ai.model_catalog}
                ollamaState={ollamaModels[provider.name]}
                onChange={(value) => updateProvider(index, { default_vision_model: value })}
                onRefresh={() => loadOllamaModels(provider.name)}
              />
            </div>
            <label>
              {t('settings.provider.api_key')}
              <input
                type="password"
                value={providerSecrets[provider.name] ?? ''}
                placeholder={provider.secret_id ? t('settings.paperless.configured') : ''}
                onChange={(event) => setProviderSecrets((current) => ({ ...current, [provider.name]: event.target.value }))}
              />
            </label>
            <label className="inline">
              <input type="checkbox" checked={provider.enabled} onChange={(event) => updateProvider(index, { enabled: event.target.checked })} />
              {t('settings.provider.enabled')}
            </label>
            <TuningDisclosure
              provider={provider}
              globals={settings}
              onChangeTuning={(patch) => updateProviderTuning(index, patch)}
              onResetBlock={(fields) => resetProviderTuningBlock(index, fields)}
            />
          </fieldset>
        ))}
      </div>
      <ModelCatalogEditor catalog={settings.ai.model_catalog} onChange={updateCatalog} />
      <div className="toolbar">
        <button title={t('settings.provider.add')} onClick={addProvider}>
          <UserPlus size={16} /> {t('settings.provider.add')}
        </button>
        <ActionButton
          icon={<Save />}
          label={t('generic.save')}
          busy={busy}
          onClick={() => run(setBusy, setError, () => api.saveSettings(settings, token, providerSecrets, notificationWebhook).then((saved) => {
            const nextSettings = withModelDefaults(saved);
            setSettings(nextSettings);
            setSavedSettings(nextSettings);
            setToken('');
            setProviderSecrets({});
            setNotificationWebhook('');
            setResult(t('generic.saved'));
            refreshInstalledOllamaModels(nextSettings);
          }), t)}
        />
        {result && <span className="result">{result}</span>}
      </div>
    </section>
  );
}

type FirstRunStep = {
  key: string;
  label: string;
  description: string;
  complete: boolean;
};

function FirstRunWizard({ steps }: { steps: FirstRunStep[] }) {
  const { t } = useI18n();
  if (steps.every((step) => step.complete)) return null;
  return (
    <section className="first-run-wizard">
      <header>
        <div>
          <strong>{t('settings.first_run.title')}</strong>
          <p>{t('settings.first_run.description')}</p>
        </div>
        <span>{steps.filter((step) => step.complete).length}/{steps.length}</span>
      </header>
      <div className="first-run-steps">
        {steps.map((step) => (
          <article className={step.complete ? 'complete' : ''} key={step.key}>
            {step.complete ? <Check size={16} /> : <Info size={16} />}
            <div>
              <strong>{step.label}</strong>
              <p>{step.description}</p>
            </div>
          </article>
        ))}
      </div>
    </section>
  );
}

function firstRunWizardSteps(
  settings: RuntimeSettings,
  savedSettings: RuntimeSettings | null,
  provider: ModelProviderDescriptor,
  t: TFunction
): FirstRunStep[] {
  const saved = savedSettings ?? settings;
  const providerNeedsSecret = provider.kind !== 'ollama' || isOllamaCloudProvider(provider);
  return [
    {
      key: 'admin',
      label: t('settings.first_run.admin.label'),
      description: t('settings.first_run.admin.description'),
      complete: true
    },
    {
      key: 'paperless',
      label: t('settings.first_run.paperless.label'),
      description: t('settings.first_run.paperless.description'),
      complete: Boolean(saved.paperless.token_secret_id && saved.paperless.base_url.trim())
    },
    {
      key: 'provider',
      label: t('settings.first_run.provider.label'),
      description: t('settings.first_run.provider.description'),
      complete: Boolean(provider.base_url.trim() && (!providerNeedsSecret || settings.ai.providers.find((entry) => entry.name === provider.name)?.secret_id))
    },
    {
      key: 'language',
      label: t('settings.first_run.language.label'),
      description: t('settings.first_run.language.description'),
      complete: Boolean(settings.tagging.tag_output_language)
    },
    {
      key: 'mode',
      label: t('settings.first_run.mode.label'),
      description: t('settings.first_run.mode.description'),
      complete: Boolean(settings.workflow.mode)
    },
    {
      key: 'test',
      label: t('settings.first_run.test.label'),
      description: t('settings.first_run.test.description'),
      complete: Boolean(saved.paperless.token_secret_id && provider.base_url.trim())
    }
  ];
}

function ConnectionTestFeedback({ state }: { state: ConnectionTestState | null }) {
  const { t } = useI18n();
  if (!state) return null;
  return (
    <div className={`connection-feedback ${state.status}`} role={state.status === 'running' ? 'status' : 'alert'} aria-live="polite">
      <header>
        {state.status === 'success' && <Check size={16} />}
        {state.status === 'error' && <X size={16} />}
        {state.status === 'running' && <RefreshCw className="spin" size={16} />}
        <strong>{state.title}</strong>
      </header>
      <p>{state.description}</p>
      {state.hints.length > 0 && (
        <ul>
          {state.hints.map((hint) => (
            <li key={hint}>{hint}</li>
          ))}
        </ul>
      )}
      {state.details && (
        <details>
          <summary>{t('settings.details')}</summary>
          <code>{state.details}</code>
        </details>
      )}
    </div>
  );
}

function paperlessTestSuccess(t: TFunction): ConnectionTestState {
  return {
    status: 'success',
    title: t('settings.paperless.success.title'),
    description: t('settings.paperless.success.description'),
    hints: [t('settings.paperless.success.hint')]
  };
}

function paperlessTestFailure(error: string | undefined, t: TFunction): ConnectionTestState {
  const details = sanitizeConnectionDetail(error || 'Paperless test failed');
  return {
    status: 'error',
    title: t('settings.paperless.failure.title'),
    description: paperlessProblemDescription(details, t),
    hints: paperlessProblemHints(details, t),
    details
  };
}

function paperlessUnsavedSettingsFeedback(
  settings: RuntimeSettings,
  savedSettings: RuntimeSettings,
  token: string,
  t: TFunction
): ConnectionTestState {
  const changedFields = [
    settings.paperless.base_url.trim() !== savedSettings.paperless.base_url.trim() ? 'Base URL' : null,
    settings.paperless.timeout_seconds !== savedSettings.paperless.timeout_seconds ? 'Timeout' : null,
    settings.paperless.login_bridge_enabled !== savedSettings.paperless.login_bridge_enabled ? 'Login bridge' : null,
    token.trim() ? 'API token' : null
  ].filter(Boolean);
  return {
    status: 'error',
    title: t('settings.paperless.unsaved.title'),
    description: t('settings.paperless.unsaved.description'),
    hints: [
      t('settings.paperless.unsaved.changed', { fields: changedFields.join(', ') }),
      t('settings.paperless.unsaved.save_first'),
      t('settings.paperless.unsaved.saved_url', { url: savedSettings.paperless.base_url || t('generic.empty') })
    ],
    details: `Unsaved Paperless settings. Current Base URL: ${settings.paperless.base_url || '(empty)'}; saved Base URL: ${savedSettings.paperless.base_url || '(empty)'}`
  };
}

function paperlessBaseUrlProblem(baseUrl: string): { reason: 'invalid' | 'self'; baseUrl: string; appOrigin?: string } | null {
  const trimmed = baseUrl.trim();
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return { reason: 'invalid', baseUrl: trimmed };
  }
  if (typeof window !== 'undefined' && parsed.host === window.location.host) {
    return { reason: 'self', baseUrl: trimmed, appOrigin: window.location.origin };
  }
  return null;
}

function paperlessBaseUrlProblemFeedback(
  problem: { reason: 'invalid' | 'self'; baseUrl: string; appOrigin?: string },
  t: TFunction
): ConnectionTestState {
  if (problem.reason === 'invalid') {
    return {
      status: 'error',
      title: t('settings.paperless.invalid_url.title'),
      description: t('settings.paperless.invalid_url.description'),
      hints: [
        t('settings.paperless.hint.backend_url'),
        t('settings.paperless.hint.compose_example'),
        t('settings.paperless.hint.save_retry')
      ],
      details: `Invalid Paperless Base URL: ${problem.baseUrl || '(empty)'}`
    };
  }
  return {
    status: 'error',
    title: t('settings.paperless.self_url.title'),
    description: t('settings.paperless.self_url.description'),
    hints: [
      t('settings.paperless.hint.not_archivist'),
      t('settings.paperless.hint.kubernetes_internal'),
      t('settings.paperless.hint.save_retry')
    ],
    details: `Paperless Base URL points to Archivist itself: ${problem.baseUrl}. App origin: ${problem.appOrigin ?? 'unknown'}`
  };
}

function providerTestSuccess(provider: ModelProviderDescriptor, t: TFunction): ConnectionTestState {
  const providerName = provider.name || provider.kind;
  const isOllama = provider.kind === 'ollama';
  return {
    status: 'success',
    title: t('settings.provider.success.title'),
    description: isOllama
      ? t('settings.provider.success.ollama', { provider: providerName })
      : t('settings.provider.success.generic', { provider: providerName }),
    hints: isOllama
      ? [t('settings.provider.success.ollama_hint')]
      : [t('settings.provider.success.generic_hint')]
  };
}

function providerTestFailure(provider: ModelProviderDescriptor, error: string | undefined, t: TFunction): ConnectionTestState {
  const details = sanitizeConnectionDetail(error || 'Provider test failed');
  return {
    status: 'error',
    title: t('settings.provider.failure.title'),
    description: providerProblemDescription(provider, details, t),
    hints: providerProblemHints(provider, details, t),
    details
  };
}

function paperlessProblemDescription(details: string, t: TFunction) {
  const lower = details.toLowerCase();
  if (lower.includes('points to the paperless-ngx service') || lower.includes('406') || lower.includes('not acceptable')) {
    return t('settings.paperless.failure.not_acceptable');
  }
  if (lower.includes('api token') || lower.includes('secret') || lower.includes('token')) {
    return t('settings.paperless.failure.token');
  }
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return t('settings.paperless.failure.auth');
  }
  if (lower.includes('404')) {
    return t('settings.paperless.failure.not_found');
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return t('settings.paperless.failure.timeout');
  }
  if (lower.includes('connect') || lower.includes('dns') || lower.includes('resolve') || lower.includes('refused')) {
    return t('settings.paperless.failure.network');
  }
  return t('settings.paperless.failure.default');
}

function paperlessProblemHints(details: string, t: TFunction) {
  const lower = details.toLowerCase();
  if (lower.includes('points to the paperless-ngx service') || lower.includes('406') || lower.includes('not acceptable')) {
    return [
      t('settings.paperless.hint.real_api'),
      t('settings.paperless.hint.internal_service'),
      t('settings.paperless.hint.kubernetes_internal'),
      t('settings.paperless.hint.save_retry')
    ];
  }
  if (lower.includes('api token') || lower.includes('secret') || lower.includes('token') || lower.includes('401') || lower.includes('403')) {
    return [
      t('settings.paperless.hint.new_token'),
      t('settings.paperless.hint.save_token'),
      t('settings.paperless.hint.permissions')
    ];
  }
  if (lower.includes('404')) {
    return [
      t('settings.paperless.hint.url_root'),
      t('settings.paperless.hint.backend_reachability'),
      t('settings.paperless.hint.compose_example')
    ];
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return [
      t('settings.paperless.hint.running'),
      t('settings.paperless.hint.network'),
      t('settings.paperless.hint.timeout')
    ];
  }
  return [
    t('settings.paperless.hint.backend_reachability'),
    t('settings.paperless.hint.network'),
    t('settings.paperless.hint.save_retry')
  ];
}

function paperlessSettingsChanged(settings: RuntimeSettings, savedSettings: RuntimeSettings, token: string) {
  return (
    settings.paperless.base_url.trim() !== savedSettings.paperless.base_url.trim() ||
    (settings.paperless.public_url ?? '').trim() !== (savedSettings.paperless.public_url ?? '').trim() ||
    settings.paperless.timeout_seconds !== savedSettings.paperless.timeout_seconds ||
    settings.paperless.login_bridge_enabled !== savedSettings.paperless.login_bridge_enabled ||
    settings.paperless.delta_sync_enabled !== savedSettings.paperless.delta_sync_enabled ||
    settings.paperless.delta_sync_overlap_minutes !== savedSettings.paperless.delta_sync_overlap_minutes ||
    settings.paperless.active_archive.trim() !== savedSettings.paperless.active_archive.trim() ||
    Boolean(token.trim())
  );
}

function providerProblemDescription(provider: ModelProviderDescriptor, details: string, t: TFunction) {
  const lower = details.toLowerCase();
  if (provider.kind === 'ollama') {
    if (lower.includes('model') && lower.includes('not listed')) {
      return t('settings.provider.failure.ollama_missing_model');
    }
    if (lower.includes('timeout') || lower.includes('timed out')) {
      return t('settings.provider.failure.ollama_timeout');
    }
    if (lower.includes('connect') || lower.includes('dns') || lower.includes('resolve') || lower.includes('refused')) {
      return t('settings.provider.failure.ollama_network');
    }
    return t('settings.provider.failure.ollama_default');
  }
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return t('settings.provider.failure.auth');
  }
  if (lower.includes('model')) {
    return t('settings.provider.failure.model');
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return t('settings.provider.failure.timeout');
  }
  return t('settings.provider.failure.default');
}

function providerProblemHints(provider: ModelProviderDescriptor, details: string, t: TFunction) {
  const lower = details.toLowerCase();
  if (provider.kind === 'ollama') {
    if (lower.includes('model') && lower.includes('not listed')) {
      return [
        t('settings.provider.hint.install_model'),
        t('settings.provider.hint.refresh_save'),
        t('settings.provider.hint.text_vision')
      ];
    }
    return [
      t('settings.provider.hint.ollama_running'),
      t('settings.provider.hint.ollama_url'),
      t('settings.provider.hint.ollama_tags')
    ];
  }
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return [
      t('settings.provider.hint.api_key'),
      t('settings.provider.hint.model_access'),
      t('settings.provider.hint.base_url')
    ];
  }
  if (lower.includes('model')) {
    return [
      t('settings.provider.hint.supported_model'),
      t('settings.provider.hint.model_access'),
      t('settings.paperless.hint.save_retry')
    ];
  }
  return [
    t('settings.provider.hint.base_url'),
    t('settings.provider.hint.rate_limits'),
    t('settings.paperless.hint.save_retry')
  ];
}

// ---------------------------------------------------------------------------
// Tuning disclosure (v1.6.2). Renders the three Tuning sub-blocks and, for
// Ollama providers, the read-only "Ollama server hints" panel. The disclosure
// is implemented with native <details> elements so the layout stays
// accessible without any extra ARIA wiring; each <summary> is keyboard
// focusable and the open/close state is preserved across re-renders.
//
// `null` vs `0` semantics: a `null`/`undefined` value in `provider.tuning.<f>`
// means "inherit the global default". The number inputs render an empty
// string for null/undefined so operators can distinguish "unset" from an
// explicit zero. Writing `0` is preserved.
// ---------------------------------------------------------------------------

type TuningField = keyof ProviderTuning;

function TuningDisclosure({
  provider,
  globals,
  onChangeTuning,
  onResetBlock
}: {
  provider: RuntimeSettings['ai']['providers'][number];
  globals: RuntimeSettings;
  onChangeTuning: (patch: Partial<ProviderTuning>) => void;
  onResetBlock: (fields: readonly TuningField[]) => void;
}) {
  const { t } = useI18n();
  const tuning = provider.tuning ?? {};
  const isOllama = provider.kind === 'ollama';
  return (
    <details className="provider-tuning">
      <summary>{t('settings.tuning.title')}</summary>
      <div className="provider-tuning-body">
        <TuningSection
          titleKey="settings.tuning.section.performance"
          onReset={() => onResetBlock(PERFORMANCE_FIELDS)}
        >
          <TuningNumberField
            field="worker_concurrency"
            value={tuning.worker_concurrency}
            defaultValue={globals.workflow.mode ? null : null /* env-only fallback */}
            defaultLabel={t('settings.tuning.default.worker_concurrency_env')}
            min={1}
            step={1}
            onChange={(value) => onChangeTuning({ worker_concurrency: value })}
          />
          <TuningTextField
            field="consensus_secondary_text_model"
            value={tuning.consensus_secondary_text_model}
            defaultLabel={t('settings.tuning.default.consensus_disabled')}
            onChange={(value) => onChangeTuning({ consensus_secondary_text_model: value })}
          />
          <TuningNumberField
            field="consensus_date_tolerance_days"
            value={tuning.consensus_date_tolerance_days}
            defaultValue={1}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ consensus_date_tolerance_days: value })}
          />
          <TuningNumberField
            field="text_num_ctx"
            value={tuning.text_num_ctx}
            defaultValue={globals.ai.ollama_text_num_ctx ?? null}
            min={1024}
            step={1024}
            onChange={(value) => onChangeTuning({ text_num_ctx: value })}
          />
          <TuningNumberField
            field="vision_num_ctx"
            value={tuning.vision_num_ctx}
            defaultValue={globals.ai.ollama_vision_num_ctx ?? null}
            min={1024}
            step={1024}
            onChange={(value) => onChangeTuning({ vision_num_ctx: value })}
          />
          <TuningSelectField
            field="reasoning_effort"
            value={tuning.reasoning_effort}
            options={REASONING_EFFORT_OPTIONS}
            defaultLabel={t('settings.tuning.default.reasoning_effort')}
            hint={provider.kind === 'anthropic' ? t('settings.tuning.hint.reasoning_anthropic') : undefined}
            onChange={(value) => onChangeTuning({ reasoning_effort: value })}
          />
        </TuningSection>
        <TuningSection
          titleKey="settings.tuning.section.caps"
          onReset={() => onResetBlock(CAPS_FIELDS)}
        >
          <TuningNumberField
            field="ocr_page_limit"
            value={tuning.ocr_page_limit}
            defaultValue={globals.ocr.page_limit ?? null}
            min={1}
            step={1}
            onChange={(value) => onChangeTuning({ ocr_page_limit: value })}
          />
          <TuningNumberField
            field="hourly_document_limit"
            value={tuning.hourly_document_limit}
            defaultValue={globals.workflow.hourly_document_limit ?? null}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ hourly_document_limit: value })}
          />
          <TuningNumberField
            field="daily_document_limit"
            value={tuning.daily_document_limit}
            defaultValue={globals.workflow.daily_document_limit ?? null}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ daily_document_limit: value })}
          />
        </TuningSection>
        <TuningSection
          titleKey="settings.tuning.section.thresholds"
          onReset={() => onResetBlock(THRESHOLD_FIELDS)}
        >
          <TuningNumberField
            field="metadata_confidence_threshold"
            value={tuning.metadata_confidence_threshold}
            defaultValue={globals.metadata.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            onChange={(value) => onChangeTuning({ metadata_confidence_threshold: value })}
          />
          <TuningNumberField
            field="title_confidence_threshold"
            value={tuning.title_confidence_threshold}
            defaultValue={globals.metadata.title_confidence_threshold ?? globals.metadata.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            onChange={(value) => onChangeTuning({ title_confidence_threshold: value })}
          />
          <TuningNumberField
            field="correspondent_confidence_threshold"
            value={tuning.correspondent_confidence_threshold}
            defaultValue={globals.metadata.correspondent_confidence_threshold ?? globals.metadata.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            onChange={(value) => onChangeTuning({ correspondent_confidence_threshold: value })}
          />
          <TuningNumberField
            field="document_type_confidence_threshold"
            value={tuning.document_type_confidence_threshold}
            defaultValue={globals.metadata.document_type_confidence_threshold ?? globals.metadata.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            onChange={(value) => onChangeTuning({ document_type_confidence_threshold: value })}
          />
          <TuningNumberField
            field="document_date_confidence_threshold"
            value={tuning.document_date_confidence_threshold}
            defaultValue={globals.metadata.document_date_confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            onChange={(value) => onChangeTuning({ document_date_confidence_threshold: value })}
          />
          <TuningNumberField
            field="tags_confidence_threshold"
            value={tuning.tags_confidence_threshold}
            defaultValue={globals.metadata.tags_confidence_threshold ?? globals.tagging.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            onChange={(value) => onChangeTuning({ tags_confidence_threshold: value })}
          />
          <TuningNumberField
            field="fields_confidence_threshold"
            value={tuning.fields_confidence_threshold}
            defaultValue={globals.metadata.fields_confidence_threshold ?? globals.fields.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            onChange={(value) => onChangeTuning({ fields_confidence_threshold: value })}
          />
          <TuningNumberField
            field="max_tags"
            value={tuning.max_tags}
            defaultValue={globals.tagging.max_tags}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ max_tags: value })}
          />
          <TuningNumberField
            field="allowed_list_max"
            value={tuning.allowed_list_max}
            defaultValue={globals.metadata.allowed_list_max ?? null}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ allowed_list_max: value })}
          />
        </TuningSection>
        {isOllama && <OllamaServerHints providerName={provider.name} />}
      </div>
    </details>
  );
}

function TuningSection({
  titleKey,
  onReset,
  children
}: {
  titleKey: 'settings.tuning.section.performance' | 'settings.tuning.section.caps' | 'settings.tuning.section.thresholds';
  onReset: () => void;
  children: ReactNode;
}) {
  const { t } = useI18n();
  return (
    <details className="provider-tuning-section" open>
      <summary>{t(titleKey)}</summary>
      <div className="provider-tuning-fields">{children}</div>
      <button
        type="button"
        className="provider-tuning-reset"
        title={t('settings.tuning.reset_defaults')}
        onClick={onReset}
      >
        <RotateCcw size={14} /> {t('settings.tuning.reset_defaults')}
      </button>
    </details>
  );
}

function TuningNumberField({
  field,
  value,
  defaultValue,
  defaultLabel,
  min,
  max,
  step,
  onChange
}: {
  field: TuningField;
  value: number | null | undefined;
  defaultValue: number | null | undefined;
  defaultLabel?: string;
  min?: number;
  max?: number;
  step?: number;
  onChange: (next: number | null) => void;
}) {
  const { t } = useI18n();
  // value === null / undefined => render empty (operator sees "inherits default").
  // value === 0 => render '0' (explicit zero is preserved).
  const display = value === null || value === undefined ? '' : String(value);
  const labelKey = (`settings.tuning.field.${field}`) as Parameters<typeof t>[0];
  const renderedDefault = defaultLabel
    ?? (defaultValue === null || defaultValue === undefined
      ? t('settings.tuning.default.inherit')
      : t('settings.tuning.global_default', { value: defaultValue }));
  return (
    <label className="provider-tuning-field">
      <span>{t(labelKey)}</span>
      <input
        type="number"
        min={min}
        max={max}
        step={step}
        value={display}
        placeholder={defaultValue !== null && defaultValue !== undefined ? String(defaultValue) : ''}
        onChange={(event) => onChange(optionalNumber(event.target.value))}
      />
      <small className="field-hint">{renderedDefault}</small>
    </label>
  );
}

function TuningTextField({
  field,
  value,
  defaultLabel,
  onChange
}: {
  field: TuningField;
  value: string | null | undefined;
  defaultLabel: string;
  onChange: (next: string | null) => void;
}) {
  const { t } = useI18n();
  const display = value ?? '';
  const labelKey = (`settings.tuning.field.${field}`) as Parameters<typeof t>[0];
  return (
    <label className="provider-tuning-field">
      <span>{t(labelKey)}</span>
      <input
        type="text"
        value={display}
        onChange={(event) => {
          const next = event.target.value;
          onChange(next.trim() === '' ? null : next);
        }}
      />
      <small className="field-hint">{defaultLabel}</small>
    </label>
  );
}

function TuningSelectField({
  field,
  value,
  options,
  defaultLabel,
  hint,
  onChange
}: {
  field: TuningField;
  value: string | null | undefined;
  options: readonly { value: string; labelKey: Parameters<ReturnType<typeof useI18n>['t']>[0] }[];
  defaultLabel: string;
  hint?: string;
  onChange: (next: ReasoningEffort | null) => void;
}) {
  const { t } = useI18n();
  const labelKey = (`settings.tuning.field.${field}`) as Parameters<typeof t>[0];
  return (
    <label className="provider-tuning-field">
      <span>{t(labelKey)}</span>
      <select
        value={value ?? ''}
        onChange={(event) => {
          const next = event.target.value;
          onChange(next === '' ? null : (next as ReasoningEffort));
        }}
      >
        <option value="">{defaultLabel}</option>
        {options.map((option) => (
          <option key={option.value} value={option.value}>
            {t(option.labelKey)}
          </option>
        ))}
      </select>
      <small className="field-hint">{hint ?? defaultLabel}</small>
    </label>
  );
}

const REASONING_EFFORT_OPTIONS = [
  { value: 'off', labelKey: 'settings.tuning.reasoning.off' },
  { value: 'low', labelKey: 'settings.tuning.reasoning.low' },
  { value: 'medium', labelKey: 'settings.tuning.reasoning.medium' },
  { value: 'high', labelKey: 'settings.tuning.reasoning.high' }
] as const;

function OllamaServerHints({ providerName }: { providerName: string }) {
  const { t } = useI18n();
  const [hints, setHints] = useState<AiRuntimeHints | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    api
      .aiRuntimeHints(providerName)
      .then((data) => {
        if (!cancelled) {
          setHints(data);
          setLoading(false);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(errorToString(err));
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [providerName]);
  const reachable = Boolean(hints?.reachable);
  return (
    <details className="provider-tuning-section ollama-hints" open>
      <summary>
        <span
          className={`status-dot ${reachable ? 'ok' : 'down'}`}
          aria-hidden="true"
        />
        {t('settings.tuning.section.ollama_hints')}
      </summary>
      <div className="provider-tuning-fields">
        {loading && <p className="field-hint">{t('generic.loading')}</p>}
        {error && !loading && (
          <p className="field-hint error">
            {t('settings.tuning.ollama_unreachable')}: {error}
          </p>
        )}
        {!loading && hints && !reachable && (
          <p className="field-hint error">{t('settings.tuning.ollama_unreachable')}</p>
        )}
        {!loading && hints && (
          <>
            <dl className="provider-tuning-meta">
              <dt>{t('settings.tuning.field.version')}</dt>
              <dd>{hints.version ?? '-'}</dd>
              <dt>{t('settings.tuning.field.loaded_models')}</dt>
              <dd>
                {hints.loaded_models && hints.loaded_models.length > 0 ? (
                  <ul>
                    {hints.loaded_models.map((model) => (
                      <li key={model.name}>
                        <code>{model.name}</code>
                        {model.size_vram_bytes != null && (
                          <> — {formatVramBytes(model.size_vram_bytes)}</>
                        )}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <span>{t('generic.none')}</span>
                )}
              </dd>
            </dl>
            {hints.hint && (
              <div className="connection-feedback warning ollama-env-warning" role="note">
                <header>
                  <Info size={16} />
                  <strong>{t('settings.tuning.hint.ollama_env')}</strong>
                </header>
                <p>{hints.hint}</p>
                <pre className="ollama-kubectl-example">
                  <code>{t('settings.tuning.hint.kubectl_example')}</code>
                </pre>
              </div>
            )}
          </>
        )}
      </div>
    </details>
  );
}

function formatVramBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 MiB';
  const mib = bytes / (1024 * 1024);
  if (mib < 1024) {
    return `${Math.round(mib)} MiB`;
  }
  const gib = mib / 1024;
  return `${gib.toFixed(2)} GiB`;
}

function sanitizeConnectionDetail(detail: string) {
  return detail
    .replace(/Bearer\s+[A-Za-z0-9._~+/=-]+/gi, 'Bearer [redacted]')
    .replace(/Token\s+[A-Za-z0-9._~+/=-]+/gi, 'Token [redacted]')
    .replace(/sk-[A-Za-z0-9_-]{8,}/gi, 'sk-[redacted]')
    .replace(/api[_-]?key["'\s:=]+[A-Za-z0-9._~+/=-]+/gi, 'api_key=[redacted]');
}

function optionalNumber(value: string) {
  if (value.trim() === '') return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function optionalPositiveInteger(value: string) {
  if (value.trim() === '') return null;
  const parsed = Number(value);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : null;
}

function splitTags(value: string) {
  return value
    .split(',')
    .map((tag) => tag.trim())
    .filter(Boolean);
}

function serializeFieldMappings(mappings: RuntimeSettings['fields']['mappings']) {
  return mappings
    .map((mapping) => [
      mapping.field_name,
      mapping.enabled ? 'enabled' : 'disabled',
      mapping.aliases.join('; '),
      mapping.instructions ?? ''
    ].join(' | '))
    .join('\n');
}

function parseFieldMappings(value: string): RuntimeSettings['fields']['mappings'] {
  return value
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const [fieldName, enabled = 'enabled', aliases = '', instructions = ''] = line.split('|').map((part) => part.trim());
      return {
        field_name: fieldName,
        enabled: enabled.toLowerCase() !== 'disabled',
        aliases: aliases.split(';').map((alias) => alias.trim()).filter(Boolean),
        instructions: instructions || null
      };
    })
    .filter((mapping) => mapping.field_name);
}

const CATALOG_PROVIDER_KINDS: AiProviderKind[] = [
  'ollama',
  'openai',
  'anthropic',
  'openai_compatible'
];
const CATALOG_USAGE_TIERS: ModelUsageTier[] = ['low', 'medium', 'high', 'extra_high'];

function ModelCatalogEditor({
  catalog,
  onChange
}: {
  catalog: ModelCatalogEntry[];
  onChange: (next: ModelCatalogEntry[]) => void;
}) {
  const { t } = useI18n();
  const entries = catalog ?? [];
  const updateAt = (index: number, patch: Partial<ModelCatalogEntry>) =>
    onChange(entries.map((entry, idx) => (idx === index ? { ...entry, ...patch } : entry)));
  const removeAt = (index: number) => onChange(entries.filter((_, idx) => idx !== index));
  const add = () =>
    onChange([
      ...entries,
      { provider_kind: 'ollama', capability: 'text', model_id: '', recommended: false }
    ]);
  return (
    <details className="provider-tuning model-catalog">
      <summary>{t('settings.catalog.title')}</summary>
      <div className="provider-tuning-body">
        <p className="field-hint">{t('settings.catalog.hint')}</p>
        <table className="catalog-table">
          <thead>
            <tr>
              <th>{t('settings.catalog.col.provider')}</th>
              <th>{t('settings.catalog.col.capability')}</th>
              <th>{t('settings.catalog.col.model')}</th>
              <th>{t('settings.catalog.col.recommended')}</th>
              <th>{t('settings.catalog.col.usage')}</th>
              <th>{t('settings.catalog.col.context')}</th>
              <th>{t('settings.catalog.col.best_for')}</th>
              <th aria-label={t('settings.catalog.col.actions')} />
            </tr>
          </thead>
          <tbody>
            {entries.map((entry, index) => (
              <tr key={index}>
                <td>
                  <select
                    value={entry.provider_kind}
                    aria-label={t('settings.catalog.col.provider')}
                    onChange={(event) =>
                      updateAt(index, { provider_kind: event.target.value as AiProviderKind })
                    }
                  >
                    {CATALOG_PROVIDER_KINDS.map((kind) => (
                      <option key={kind} value={kind}>
                        {kind}
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  <select
                    value={entry.capability}
                    aria-label={t('settings.catalog.col.capability')}
                    onChange={(event) =>
                      updateAt(index, { capability: event.target.value as 'text' | 'vision' })
                    }
                  >
                    <option value="text">text</option>
                    <option value="vision">vision</option>
                  </select>
                </td>
                <td>
                  <input
                    value={entry.model_id}
                    aria-label={t('settings.catalog.col.model')}
                    onChange={(event) => updateAt(index, { model_id: event.target.value })}
                  />
                </td>
                <td>
                  <input
                    type="checkbox"
                    checked={entry.recommended}
                    aria-label={t('settings.catalog.col.recommended')}
                    onChange={(event) => updateAt(index, { recommended: event.target.checked })}
                  />
                </td>
                <td>
                  <select
                    value={entry.usage_tier ?? ''}
                    aria-label={t('settings.catalog.col.usage')}
                    onChange={(event) =>
                      updateAt(index, {
                        usage_tier:
                          event.target.value === ''
                            ? null
                            : (event.target.value as ModelUsageTier)
                      })
                    }
                  >
                    <option value="">—</option>
                    {CATALOG_USAGE_TIERS.map((tier) => (
                      <option key={tier} value={tier}>
                        {usageTierLabel(tier)}
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  <input
                    value={entry.context ?? ''}
                    aria-label={t('settings.catalog.col.context')}
                    onChange={(event) =>
                      updateAt(index, {
                        context: event.target.value.trim() === '' ? null : event.target.value
                      })
                    }
                  />
                </td>
                <td>
                  <input
                    value={entry.best_for ?? ''}
                    aria-label={t('settings.catalog.col.best_for')}
                    onChange={(event) =>
                      updateAt(index, {
                        best_for: event.target.value.trim() === '' ? null : event.target.value
                      })
                    }
                  />
                </td>
                <td>
                  <button
                    type="button"
                    className="icon-button"
                    aria-label={t('settings.catalog.remove')}
                    title={t('settings.catalog.remove')}
                    onClick={() => removeAt(index)}
                  >
                    <X size={16} />
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        <button type="button" className="secondary" onClick={add}>
          {t('settings.catalog.add')}
        </button>
      </div>
    </details>
  );
}

function ProviderModelSelect({
  capability,
  provider,
  value,
  catalog,
  ollamaState,
  onChange,
  onRefresh
}: {
  capability: ModelCapability;
  provider: ModelProviderDescriptor;
  value: string;
  catalog: ModelCatalogEntry[];
  ollamaState?: OllamaModelLoadState;
  onChange: (value: string) => void;
  onRefresh: () => void;
}) {
  const { t } = useI18n();
  // Local Ollama discovers *installed* models (/api/tags); every other provider
  // (Ollama Cloud, OpenAI, Anthropic, OpenAI-compatible) discovers its *catalog*
  // via /v1/models. Both flow through the same sync button + status.
  const usesInstalledModels = provider.kind === 'ollama' && !isOllamaCloudProvider(provider);
  const hasReliableList = Boolean(ollamaState?.loaded && !ollamaState.error);
  const syncedModels = hasReliableList ? (ollamaState?.models ?? []) : null;
  const options = usesInstalledModels
    ? installedOllamaModelOptions(
      ollamaState?.models ?? [],
      value,
      hasReliableList,
      ollamaSelectPlaceholder(ollamaState, t),
      t
    )
    : catalogModelOptions(provider, capability, value, catalog ?? [], syncedModels, t);
  const liveNames = syncedModels?.map((model) => model.name) ?? [];
  const currentIsMissing =
    Boolean(value) && hasReliableList && !liveNames.some((name) => name === value);

  return (
    <div className="model-select-block">
      <div className="model-select-row">
        <select
          value={value}
          aria-label={`${provider.name} ${capability} model`}
          onChange={(event) => onChange(event.target.value)}
        >
          {options.map((option) => (
            <option key={option.value || option.label} value={option.value} disabled={!option.value}>
              {option.label}
            </option>
          ))}
        </select>
        {usesInstalledModels && <HardwareRecommendationInfo />}
        <button
          className="icon-button"
          title={t('settings.ollama.reload_models')}
          aria-label={t('settings.ollama.reload_models')}
          type="button"
          disabled={ollamaState?.loading}
          onClick={onRefresh}
        >
          <RefreshCw size={16} />
        </button>
      </div>
      <OllamaModelStatus state={ollamaState} currentIsMissing={currentIsMissing} />
    </div>
  );
}

/// Builds the dropdown options for catalog-driven providers (everything except
/// local Ollama), merging the curated catalog with a live `/v1/models` sync:
/// catalog entries keep their recommendation label; entries absent from the
/// live list are flagged; live IDs not in the catalog are appended.
function usageTierLabel(tier: ModelUsageTier): string {
  switch (tier) {
    case 'low':
      return 'Low';
    case 'medium':
      return 'Medium';
    case 'high':
      return 'High';
    case 'extra_high':
      return 'Extra High';
  }
}

function catalogEntryLabel(entry: ModelCatalogEntry): string {
  const parts = [entry.label || entry.model_id];
  if (entry.recommended) parts.push('★');
  if (entry.usage_tier) parts.push(usageTierLabel(entry.usage_tier));
  if (entry.context) parts.push(entry.context);
  return parts.join(' · ');
}

function catalogModelOptions(
  provider: ModelProviderDescriptor,
  capability: ModelCapability,
  value: string,
  catalogEntries: ModelCatalogEntry[],
  syncedModels: OllamaInstalledModel[] | null,
  t: TFunction
): { value: string; label: string }[] {
  // The editable settings catalog is the source of truth. Fall back to the
  // static modelCatalog.ts only when the settings catalog has no entry for
  // this provider kind + capability (so the dropdown is never empty).
  const entries = catalogEntries.filter(
    (entry) => entry.provider_kind === provider.kind && entry.capability === capability
  );
  const base =
    entries.length > 0
      ? entries.map((entry) => ({ value: entry.model_id, label: catalogEntryLabel(entry) }))
      : modelOptions(provider, capability, value).map((option) => ({
        value: option.value,
        label: modelOptionLabel(option)
      }));
  if (!syncedModels) {
    return base;
  }
  const liveNames = new Set(syncedModels.map((model) => model.name));
  const baseValues = new Set(base.map((option) => option.value));
  const merged = base.map((option) => ({
    value: option.value,
    label: liveNames.has(option.value)
      ? option.label
      : `${option.label} · ⚠ ${t('settings.ollama.not_listed')}`
  }));
  for (const model of syncedModels) {
    if (!baseValues.has(model.name)) {
      merged.push({ value: model.name, label: model.name });
    }
  }
  if (value && !baseValues.has(value) && !liveNames.has(value)) {
    merged.unshift({ value, label: value });
  }
  return merged;
}

function installedOllamaModelOptions(
  models: OllamaInstalledModel[],
  current: string,
  loaded: boolean,
  placeholder: string,
  t: TFunction
) {
  const options = models.map((model) => ({
    value: model.name,
    label: installedOllamaModelLabel(model, t)
  }));
  const hasCurrent = models.some((model) => model.name === current);
  if (current && !loaded && !hasCurrent) {
    return [{ value: current, label: current }, ...options];
  }
  if (current && loaded && !hasCurrent) {
    return [{ value: current, label: `⚠ ${current} · ${t('settings.ollama.not_installed')}` }, ...options];
  }
  if (!current && loaded && options.length === 0) {
    return [{ value: '', label: t('settings.ollama.none_installed') }];
  }
  if (!current && !loaded) {
    return [{ value: '', label: placeholder }];
  }
  return options;
}

function ollamaSelectPlaceholder(state: OllamaModelLoadState | undefined, t: TFunction) {
  if (state?.error) return t('settings.ollama.unavailable');
  if (state?.loading) return t('settings.ollama.loading_select');
  return t('settings.ollama.load_select');
}

function installedOllamaModelLabel(model: OllamaInstalledModel, t: TFunction) {
  return [
    model.name,
    model.parameter_size || t('settings.ollama.unknown_parameters'),
    model.quantization_level || t('settings.ollama.unknown_quantization'),
    formatModelSize(model.size_bytes, t)
  ].join(' · ');
}

function formatModelSize(sizeBytes: number | null | undefined, t?: TFunction) {
  if (!sizeBytes || sizeBytes <= 0) return t ? t('settings.ollama.unknown_size') : 'unknown size';
  return `${(sizeBytes / 1024 ** 3).toFixed(sizeBytes >= 10 * 1024 ** 3 ? 1 : 2)} GB`;
}

function OllamaModelStatus({
  state,
  currentIsMissing
}: {
  state?: OllamaModelLoadState;
  currentIsMissing: boolean;
}) {
  const { t } = useI18n();
  if (state?.loading) {
    return <p className="field-hint">{t('settings.ollama.loading')}</p>;
  }
  if (state?.error) {
    return <p className="field-hint error">{state.error}</p>;
  }
  if (state?.loaded && state.models.length === 0) {
    return <p className="field-hint warning">{t('settings.ollama.no_models')}</p>;
  }
  if (currentIsMissing) {
    return <p className="field-hint warning">{t('settings.ollama.model_missing')}</p>;
  }
  return null;
}

function HardwareRecommendationInfo() {
  const [open, setOpen] = useState(false);
  const wrapperRef = useRef<HTMLSpanElement | null>(null);
  const tooltipId = useId();

  useEffect(() => {
    if (!open) return undefined;
    const closeOnOutsidePointer = (event: PointerEvent) => {
      if (wrapperRef.current && !wrapperRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setOpen(false);
      }
    };
    document.addEventListener('pointerdown', closeOnOutsidePointer);
    document.addEventListener('keydown', closeOnEscape);
    return () => {
      document.removeEventListener('pointerdown', closeOnOutsidePointer);
      document.removeEventListener('keydown', closeOnEscape);
    };
  }, [open]);

  if (!recommendationProfile) return null;

  return (
    <span
      className="tooltip-shell"
      ref={wrapperRef}
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
    >
      <button
        className="info-button"
        type="button"
        aria-label={`Hardware recommendation for ${recommendationProfile.label}`}
        aria-describedby={open ? tooltipId : undefined}
        aria-expanded={open}
        onFocus={() => setOpen(true)}
        onClick={(event) => {
          event.preventDefault();
          setOpen((current) => !current);
        }}
      >
        <Info size={16} />
      </button>
      {open && (
        <span className="hardware-tooltip" id={tooltipId} role="tooltip">
          <strong>{recommendationProfile.title}</strong>
          {recommendationProfile.items.map((item) => (
            <span key={item.label}><b>{item.label}:</b> <code>{item.model}</code></span>
          ))}
        </span>
      )}
    </span>
  );
}
