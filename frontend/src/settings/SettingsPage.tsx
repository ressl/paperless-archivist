import { useEffect, useState } from 'react';
import { Save, UserPlus } from 'lucide-react';
import {
  api,
  AiProviderKind,
  ModelCatalogEntry,
  ProviderTuning,
  RuntimeSettings
} from '../api/client';
import {
  defaultProvider,
  isOllamaCloudProvider,
  providerDefaults,
  recommendedModel,
  withModelDefaults
} from '../modelCatalog';
import { useI18n } from '../i18n/I18nProvider';
import { ActionButton, PageHeader, errorToString, localizedErrorMessage, run } from '../lib/ui';
import { LanguageSelector } from '../lib/LanguageSelector';
import { AiDefaultsSection } from './sections/AiDefaultsSection';
import { CompletionTagsSection } from './sections/CompletionTagsSection';
import { FieldsSection } from './sections/FieldsSection';
import { FirstRunWizard, firstRunWizardSteps } from './sections/FirstRunWizard';
import { MetadataSection } from './sections/MetadataSection';
import { ModelCatalogEditor } from './sections/ModelCatalogEditor';
import { NotificationsSection } from './sections/NotificationsSection';
import { OcrSection } from './sections/OcrSection';
import { PaperlessSection } from './sections/PaperlessSection';
import { ProviderCard } from './sections/ProviderCard';
import { SecuritySection } from './sections/SecuritySection';
import { TaggingSection } from './sections/TaggingSection';
import { UiSection } from './sections/UiSection';
import { WorkflowProcessingSection } from './sections/WorkflowProcessingSection';
import {
  TUNING_PRESETS,
  tuningPresetKindFor,
  type TuningField
} from './sections/tuning';
import {
  paperlessBaseUrlProblem,
  paperlessBaseUrlProblemFeedback,
  paperlessSettingsChanged,
  paperlessTestFailure,
  paperlessTestSuccess,
  paperlessUnsavedSettingsFeedback,
  providerTestFailure,
  providerTestSuccess
} from './sections/connectionTests';
import { sanitizeConnectionDetail } from './sections/helpers';
import type { ConnectionTestState, OllamaModelLoadState } from './sections/types';

// #229: the typed-but-unsaved provider API keys are tracked by list index so a
// rename keeps the entry. The save endpoint still matches secrets by provider
// name, so translate back to a name-keyed map at the boundary.
function providerSecretsByName(
  settings: RuntimeSettings,
  secretsByIndex: Record<number, string>
): Record<string, string> {
  const out: Record<string, string> = {};
  settings.ai.providers.forEach((provider, index) => {
    const secret = secretsByIndex[index];
    if (secret && provider.name) out[provider.name] = secret;
  });
  return out;
}

export function SettingsPage({ setError }: { setError: (error: string | null) => void }) {
  const { t } = useI18n();
  const [settings, setSettings] = useState<RuntimeSettings | null>(null);
  const [savedSettings, setSavedSettings] = useState<RuntimeSettings | null>(null);
  const [token, setToken] = useState('');
  // #229: key transient per-provider state (typed API key + loaded models) by
  // the provider's stable list index rather than its mutable display name, so
  // renaming a provider no longer drops the in-progress secret or model list.
  const [providerSecrets, setProviderSecrets] = useState<Record<number, string>>({});
  const [notificationWebhook, setNotificationWebhook] = useState('');
  const [ollamaModels, setOllamaModels] = useState<Record<number, OllamaModelLoadState>>({});
  const [paperlessTest, setPaperlessTest] = useState<ConnectionTestState | null>(null);
  const [providerTest, setProviderTest] = useState<ConnectionTestState | null>(null);
  const [notificationTest, setNotificationTest] = useState<ConnectionTestState | null>(null);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);

  const loadOllamaModels = (index: number, providerName: string) => {
    setOllamaModels((current) => ({
      ...current,
      [index]: {
        loading: true,
        loaded: current[index]?.loaded ?? false,
        models: current[index]?.models ?? [],
        error: null
      }
    }));
    return api
      .ollamaModels(providerName)
      .then((data) => {
        setOllamaModels((current) => ({
          ...current,
          [index]: {
            loading: false,
            loaded: true,
            models: data.models,
            error: null
          }
        }));
      })
      .catch((err: unknown) => {
        // Surface the real server error (e.g. "… requires an API key", a 401,
        // or an unreachable host) instead of always blaming Ollama — model
        // discovery now covers OpenAI/Anthropic/OpenAI-compatible too.
        const message = err instanceof Error && err.message ? err.message : t('settings.ollama.load_error');
        setOllamaModels((current) => ({
          ...current,
          [index]: {
            loading: false,
            loaded: true,
            models: current[index]?.models ?? [],
            error: message
          }
        }));
      });
  };

  const refreshInstalledOllamaModels = (nextSettings: RuntimeSettings) => {
    const targets = nextSettings.ai.providers
      .map((provider, index) => ({ provider, index }))
      .filter(
        ({ provider }) => provider.kind === 'ollama' && !isOllamaCloudProvider(provider) && Boolean(provider.name)
      );
    void Promise.allSettled(targets.map(({ provider, index }) => loadOllamaModels(index, provider.name)));
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

  if (!settings) {
    return (
      <section className="page">
        <PageHeader title={t('settings.loading_title')} />
      </section>
    );
  }

  const update = (updater: (settings: RuntimeSettings) => RuntimeSettings) =>
    setSettings((current) => (current ? updater(current) : current));

  // Per-slice patch helpers keep the section components purely presentational:
  // each receives its own slice + a typed onChange that merges a partial patch.
  const updatePaperless = (patch: Partial<RuntimeSettings['paperless']>) =>
    update((s) => ({ ...s, paperless: { ...s.paperless, ...patch } }));
  const updateAi = (patch: Partial<RuntimeSettings['ai']>) =>
    update((s) => ({ ...s, ai: { ...s.ai, ...patch } }));
  const updateWorkflow = (patch: Partial<RuntimeSettings['workflow']>) =>
    update((s) => ({ ...s, workflow: { ...s.workflow, ...patch } }));
  const updateWorkflowTags = (patch: Record<string, string>) =>
    update((s) => ({ ...s, workflow: { ...s.workflow, tags: { ...s.workflow.tags, ...patch } } }));
  const updateOcr = (patch: Partial<RuntimeSettings['ocr']>) =>
    update((s) => ({ ...s, ocr: { ...s.ocr, ...patch } }));
  const updateTagging = (patch: Partial<RuntimeSettings['tagging']>) =>
    update((s) => ({ ...s, tagging: { ...s.tagging, ...patch } }));
  const updateFields = (patch: Partial<RuntimeSettings['fields']>) =>
    update((s) => ({ ...s, fields: { ...s.fields, ...patch } }));
  const updateMetadata = (patch: Partial<RuntimeSettings['metadata']>) =>
    update((s) => ({ ...s, metadata: { ...s.metadata, ...patch } }));
  const updateNotifications = (patch: Partial<RuntimeSettings['notifications']>) =>
    update((s) => ({ ...s, notifications: { ...s.notifications, ...patch } }));
  const updateSecurity = (patch: Partial<RuntimeSettings['security']>) =>
    update((s) => ({ ...s, security: { ...s.security, ...patch } }));
  const updateUi = (patch: Partial<NonNullable<RuntimeSettings['ui']>>) =>
    update((s) => ({ ...s, ui: { ...(s.ui ?? {}), ...patch } }));

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
  const resetProviderTuningBlock = (index: number, fields: readonly TuningField[]) =>
    update((s) => {
      const providers = [...s.ai.providers];
      const current = providers[index];
      const presetKind = tuningPresetKindFor(current);
      const preset = TUNING_PRESETS[presetKind];
      const tuning: ProviderTuning = { ...(current.tuning ?? {}) };
      for (const field of fields) {
        // Type-safe write: each field is a key on ProviderTuning and the
        // preset map carries the same field. Cast through Record to satisfy
        // TS's per-field union; the satisfies in tuning.tsx keeps names honest.
        (tuning as Record<string, unknown>)[field] = (preset as Record<string, unknown>)[field] ?? null;
      }
      providers[index] = { ...current, tuning };
      return { ...s, ai: { ...s.ai, providers } };
    });
  const selectDefaultProvider = (name: string) =>
    update((s) => {
      const provider = s.ai.providers.find((entry) => entry.name === name);
      const selectedProvider =
        provider ?? { name: 'ollama', kind: 'ollama' as AiProviderKind, base_url: s.ai.ollama_base_url };
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
  const selectedDefaultProviderIndex = settings.ai.providers.findIndex(
    (provider) => provider.name === settings.ai.default_provider
  );

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
        setProviderTest(
          data.ok
            ? providerTestSuccess(selectedDefaultProvider, t)
            : providerTestFailure(selectedDefaultProvider, data.error, t)
        );
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
        setNotificationTest(
          data.ok
            ? {
                status: 'success',
                title: t('settings.notifications.success.title'),
                description: t('settings.notifications.success.description'),
                hints: [t('settings.notifications.success.hint')]
              }
            : {
                status: 'error',
                title: t('settings.notifications.failure.title'),
                description: t('settings.notifications.failure.description'),
                hints: [
                  t('settings.notifications.failure.hint_url'),
                  t('settings.notifications.failure.hint_reachable'),
                  t('settings.notifications.failure.hint_saved')
                ],
                details: sanitizeConnectionDetail(data.error ?? t('generic.request_failed'))
              }
        );
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

  const saveSettings = () =>
    run(
      setBusy,
      setError,
      () =>
        api
          .saveSettings(settings, token, providerSecretsByName(settings, providerSecrets), notificationWebhook)
          .then((saved) => {
            const nextSettings = withModelDefaults(saved);
            setSettings(nextSettings);
            setSavedSettings(nextSettings);
            setToken('');
            setProviderSecrets({});
            setNotificationWebhook('');
            setResult(t('generic.saved'));
            refreshInstalledOllamaModels(nextSettings);
          }),
      t
    );

  return (
    <section className="page">
      <PageHeader title={t('settings.title')} />
      <FirstRunWizard steps={firstRunSteps} />
      <div className="settings-language-row">
        <LanguageSelector compact />
      </div>
      <div className="card-grid card-grid--default">
        <PaperlessSection
          value={settings.paperless}
          onChange={updatePaperless}
          token={token}
          onTokenChange={setToken}
          test={paperlessTest}
          onTest={runPaperlessTest}
        />
        <AiDefaultsSection
          ai={settings.ai}
          onChange={updateAi}
          selectedProvider={selectedDefaultProvider}
          ollamaState={ollamaModels[selectedDefaultProviderIndex]}
          onSelectDefaultProvider={selectDefaultProvider}
          onRefreshModels={() => loadOllamaModels(selectedDefaultProviderIndex, selectedDefaultProvider.name)}
          test={providerTest}
          onTest={runProviderTest}
        />
        <WorkflowProcessingSection value={settings.workflow} onChange={updateWorkflow} />
        <OcrSection value={settings.ocr} onChange={updateOcr} />
        <TaggingSection value={settings.tagging} onChange={updateTagging} />
        <FieldsSection value={settings.fields} onChange={updateFields} />
        <MetadataSection value={settings.metadata} onChange={updateMetadata} />
        <CompletionTagsSection value={settings.workflow.tags} onChange={updateWorkflowTags} />
        <NotificationsSection
          value={settings.notifications}
          onChange={updateNotifications}
          webhook={notificationWebhook}
          onWebhookChange={setNotificationWebhook}
          test={notificationTest}
          onTest={runNotificationTest}
        />
        <SecuritySection value={settings.security} onChange={updateSecurity} />
        <UiSection value={settings.ui} onChange={updateUi} />
      </div>
      <PageHeader title={t('settings.providers')} />
      <div className="card-grid card-grid--default">
        {settings.ai.providers.map((provider, index) => (
          <ProviderCard
            key={`${provider.name}-${index}`}
            provider={provider}
            catalog={settings.ai.model_catalog}
            globals={settings}
            ollamaState={ollamaModels[index]}
            secret={providerSecrets[index] ?? ''}
            onSecretChange={(value) => setProviderSecrets((current) => ({ ...current, [index]: value }))}
            onChangeProvider={(patch) => updateProvider(index, patch)}
            onChangeTuning={(patch) => updateProviderTuning(index, patch)}
            onResetTuningBlock={(fields) => resetProviderTuningBlock(index, fields)}
            onRefreshModels={() => loadOllamaModels(index, provider.name)}
          />
        ))}
      </div>
      <ModelCatalogEditor catalog={settings.ai.model_catalog} onChange={updateCatalog} />
      <div className="toolbar">
        <button title={t('settings.provider.add')} onClick={addProvider}>
          <UserPlus size={16} /> {t('settings.provider.add')}
        </button>
        <ActionButton icon={<Save />} label={t('generic.save')} busy={busy} onClick={saveSettings} />
        {result && <span className="result">{result}</span>}
      </div>
    </section>
  );
}
