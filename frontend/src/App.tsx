import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  Activity,
  AlertTriangle,
  Archive,
  Check,
  ClipboardList,
  Database,
  FileText,
  GitCompare,
  History,
  Info,
  KeyRound,
  ListChecks,
  LogOut,
  MessageSquare,
  Power,
  Play,
  RefreshCw,
  RotateCcw,
  Save,
  Send,
  Settings,
  Shield,
  Tags,
  UserPlus,
  X
} from 'lucide-react';
import {
  api,
  AiProviderKind,
  ApiToken,
  AuditEvent,
  AuditIntegrityReport,
  CompletionTagReconcileResult,
  Counts,
  DashboardLiveFailure,
  DashboardRange,
  DashboardLiveStatus,
  DashboardStats,
  DashboardStatusCount,
  DocumentChatMessage,
  DocumentChatSession,
  InventoryItem,
  Me,
  NeedsAttentionItem,
  OidcConfig,
  OllamaInstalledModel,
  PaperlessConsistencyResult,
  Prompt,
  PromptTestResponse,
  PromptUsage,
  RecoveryCandidate,
  RetentionResult,
  ReviewItem,
  Role,
  RuntimeSettings,
  SessionItem,
  Stage,
  UserItem,
  WorkflowSafetyStatus
} from './api/client';
import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  CartesianGrid,
  ComposedChart,
  Legend,
  Line,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis
} from 'recharts';
import {
  defaultProvider,
  isOllamaCloudProvider,
  modelOptionLabel,
  modelOptions,
  providerDefaults,
  recommendedModel,
  withModelDefaults
} from './modelCatalog';
import hardwareRecommendations from './hardwareRecommendations.json';
import { promptStageHelp, promptStageOrder, type PromptStageHelp } from './data/promptHelp';
import { languageOptionLabel, languageOptions } from './data/worldLanguages';
import { buildInfo, buildInfoLabel } from './buildInfo';
import { localizedMessage, useI18n, type TFunction } from './i18n/I18nProvider';
import { Dashboard } from './dashboard/Dashboard';
import { Inventory } from './inventory/Inventory';
import { Reviews } from './reviews/Reviews';
import { Audit } from './audit/Audit';
import { Users } from './users/Users';
import { Prompts } from './prompts/Prompts';
import { DocumentChat } from './chat/DocumentChat';
import { ErrorBoundary } from './lib/ErrorBoundary';
import { ActionButton, PageHeader, Status, errorToString, localizedErrorMessage, run } from './lib/ui';
import { workflowModeDescription, workflowModeLabel, workflowModeOptions } from './lib/workflow';
import {
  deltaTone,
  formatCost,
  formatDelta,
  formatMs,
  formatMttc,
  formatPercent,
  formatRelativeTime,
  shortId,
  stageLabel,
  statusLabel,
  titleCaseStatus
} from './lib/format';

type Tab = 'dashboard' | 'inventory' | 'chat' | 'reviews' | 'settings' | 'prompts' | 'audit' | 'users';

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

export function App() {
  const { t } = useI18n();
  const [me, setMe] = useState<Me | null>(null);
  const [tab, setTab] = useState<Tab>('dashboard');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .me()
      .then(setMe)
      .catch(() => setMe(null))
      .finally(() => setLoading(false));
  }, []);

  if (loading) return <div className="boot">{t('app.loading')}</div>;
  if (!me) return <Login onLogin={setMe} />;

  const canUseChat = me.roles.some((role) => role === 'admin' || role === 'reviewer' || role === 'operator');
  const canManageSettings = me.roles.some((role) => role === 'admin');

  return (
    <ErrorBoundary>
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <img src="/assets/brand/paperless-archivist-logo.png" alt="" />
          <div>
            <strong>{t('app.name')}</strong>
            <span>{me.username}</span>
          </div>
        </div>
        <nav>
          <NavButton icon={<Activity />} label={t('nav.dashboard')} active={tab === 'dashboard'} onClick={() => setTab('dashboard')} />
          <NavButton icon={<Archive />} label={t('nav.inventory')} active={tab === 'inventory'} onClick={() => setTab('inventory')} />
          {canUseChat && <NavButton icon={<MessageSquare />} label={t('nav.chat')} active={tab === 'chat'} onClick={() => setTab('chat')} />}
          <NavButton icon={<ListChecks />} label={t('nav.review')} active={tab === 'reviews'} onClick={() => setTab('reviews')} />
          <NavButton icon={<Settings />} label={t('nav.settings')} active={tab === 'settings'} onClick={() => setTab('settings')} />
          <NavButton icon={<ClipboardList />} label={t('nav.prompts')} active={tab === 'prompts'} onClick={() => setTab('prompts')} />
          <NavButton icon={<Shield />} label={t('nav.audit')} active={tab === 'audit'} onClick={() => setTab('audit')} />
          <NavButton icon={<UserPlus />} label={t('nav.users')} active={tab === 'users'} onClick={() => setTab('users')} />
        </nav>
        <LanguageSelector />
        <div className="sidebar-version" aria-label={buildInfoLabel} title={buildInfoLabel}>
          <span>{t('nav.version')}</span>
          <strong>{buildInfo.version}</strong>
          {buildInfo.buildNumber && <small>{t('nav.build', { build: buildInfo.buildNumber })}</small>}
        </div>
        <button
          className="ghost-button"
          title={t('nav.logout')}
          onClick={async () => {
            await api.logout();
            setMe(null);
          }}
        >
          <LogOut size={18} /> {t('nav.logout')}
        </button>
      </aside>

      <main className="workspace">
        {error && (
          <div className="banner error">
            <span>{error}</span>
            <button title={t('generic.dismiss')} onClick={() => setError(null)}>
              <X size={16} />
            </button>
          </div>
        )}
        {tab === 'dashboard' && (
          <ErrorBoundary>
            <Dashboard setError={setError} canManageSettings={canManageSettings} />
          </ErrorBoundary>
        )}
        {tab === 'inventory' && (
          <ErrorBoundary>
            <Inventory setError={setError} />
          </ErrorBoundary>
        )}
        {tab === 'chat' && canUseChat && (
          <ErrorBoundary>
            <DocumentChat setError={setError} />
          </ErrorBoundary>
        )}
        {tab === 'reviews' && (
          <ErrorBoundary>
            <Reviews setError={setError} />
          </ErrorBoundary>
        )}
        {tab === 'settings' && (
          <ErrorBoundary>
            <SettingsPage setError={setError} />
          </ErrorBoundary>
        )}
        {tab === 'prompts' && (
          <ErrorBoundary>
            <Prompts setError={setError} />
          </ErrorBoundary>
        )}
        {tab === 'audit' && (
          <ErrorBoundary>
            <Audit setError={setError} />
          </ErrorBoundary>
        )}
        {tab === 'users' && (
          <ErrorBoundary>
            <Users setError={setError} />
          </ErrorBoundary>
        )}
      </main>
    </div>
    </ErrorBoundary>
  );
}

function Login({ onLogin }: { onLogin: (me: Me) => void }) {
  const { t } = useI18n();
  const [username, setUsername] = useState('admin');
  const [password, setPassword] = useState('');
  const [oidc, setOidc] = useState<OidcConfig | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loginBusy, setLoginBusy] = useState(false);

  useEffect(() => {
    api
      .oidcConfig()
      .then(setOidc)
      .catch(() => setOidc(null));
  }, []);

  const submitLogin = async (mode: 'local' | 'paperless') => {
    setError(null);
    setLoginBusy(true);
    try {
      onLogin(mode === 'paperless' ? await api.paperlessLogin(username, password) : await api.login(username, password));
    } catch (err) {
      setError(localizedErrorMessage(err, t, t('auth.error')));
    } finally {
      setLoginBusy(false);
    }
  };

  return (
    <main className="login">
      <section className="login-panel">
        <img src="/assets/brand/paperless-archivist-logo.png" alt="" />
        <h1>{t('app.name')}</h1>
        <LanguageSelector compact />
        {oidc?.enabled && oidc.login_url && (
          <a className="sso-button" href={oidc.login_url}>
            <KeyRound size={18} /> {t('auth.login_sso', { provider: oidc.provider ?? 'SSO' })}
          </a>
        )}
        {oidc?.enabled && <div className="login-divider" />}
        <form
          onSubmit={async (event) => {
            event.preventDefault();
            await submitLogin('local');
          }}
        >
          <label>
            {t('auth.username')}
            <input value={username} onChange={(event) => setUsername(event.target.value)} autoComplete="username" />
          </label>
          <label>
            {t('auth.password')}
            <input
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              type="password"
              autoComplete="current-password"
            />
          </label>
          {error && <p className="form-error">{error}</p>}
          <button className="primary-button" disabled={loginBusy}>
            <KeyRound size={18} /> {loginBusy ? t('auth.login_busy') : t('auth.login')}
          </button>
          {oidc?.paperless_login_enabled && (
            <button type="button" className="secondary-button" disabled={loginBusy} onClick={() => void submitLogin('paperless')}>
              <Archive size={18} /> {t('auth.login_paperless')}
            </button>
          )}
        </form>
      </section>
    </main>
  );
}

function SettingsPage({ setError }: { setError: (error: string | null) => void }) {
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
  const updateProvider = (index: number, patch: Partial<RuntimeSettings['ai']['providers'][number]>) =>
    update((s) => {
      const providers = [...s.ai.providers];
      providers[index] = { ...providers[index], ...patch };
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
              ollamaState={ollamaModels[selectedDefaultProvider.name]}
              onChange={(value) => update((s) => ({ ...s, ai: { ...s.ai, default_vision_model: value } }))}
              onRefresh={() => loadOllamaModels(selectedDefaultProvider.name)}
            />
          </div>
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
          </fieldset>
        ))}
      </div>
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

function ProviderModelSelect({
  capability,
  provider,
  value,
  ollamaState,
  onChange,
  onRefresh
}: {
  capability: ModelCapability;
  provider: ModelProviderDescriptor;
  value: string;
  ollamaState?: OllamaModelLoadState;
  onChange: (value: string) => void;
  onRefresh: () => void;
}) {
  const { t } = useI18n();
  const usesInstalledModels = provider.kind === 'ollama' && !isOllamaCloudProvider(provider);
  const hasReliableInstalledList = Boolean(ollamaState?.loaded && !ollamaState.error);
  const options = usesInstalledModels
    ? installedOllamaModelOptions(
      ollamaState?.models ?? [],
      value,
      hasReliableInstalledList,
      ollamaSelectPlaceholder(ollamaState, t),
      t
    )
    : modelOptions(provider, capability, value).map((option) => ({
      value: option.value,
      label: modelOptionLabel(option)
    }));
  const currentIsMissing =
    usesInstalledModels &&
    Boolean(value) &&
    hasReliableInstalledList &&
    !(ollamaState?.models ?? []).some((model) => model.name === value);

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
        {usesInstalledModels && (
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
        )}
      </div>
      {usesInstalledModels && (
        <OllamaModelStatus
          state={ollamaState}
          currentIsMissing={currentIsMissing}
        />
      )}
    </div>
  );
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

function LanguageSelector({ compact }: { compact?: boolean }) {
  const { locale, setLocale, localeOptions, t } = useI18n();
  const selectedBase = locale.toLowerCase().split('-')[0] || 'en';
  const selectedOption = localeOptions.find((option) => option.tag === selectedBase);
  return (
    <label className={`language-selector${compact ? ' compact' : ''}`}>
      <span>{t('language.selector.label')}</span>
      <select
        value={selectedOption?.tag ?? selectedBase}
        aria-label={t('language.selector.label')}
        onChange={(event) => setLocale(event.target.value)}
      >
        {localeOptions.map((option) => (
          <option key={option.tag} value={option.tag}>
            {option.uiName === option.nativeName
              ? `${option.uiName} · ${t(`language.status.${option.status}`)}`
              : `${option.uiName} · ${option.nativeName} · ${t(`language.status.${option.status}`)}`}
          </option>
        ))}
      </select>
      {selectedOption?.status === 'fallback' && <small>{t('language.fallback.hint')}</small>}
    </label>
  );
}

function NavButton({ icon, label, active, onClick }: { icon: ReactNode; label: string; active: boolean; onClick: () => void }) {
  return <button className={active ? 'active' : ''} onClick={onClick}>{icon}{label}</button>;
}

