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
  ProcessingMode,
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
  Legend,
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
const workflowModeOptions: Array<{ value: ProcessingMode; labelKey: Parameters<TFunction>[0]; descriptionKey: Parameters<TFunction>[0] }> = [
  {
    value: 'manual_review',
    labelKey: 'workflow.mode.manual.label',
    descriptionKey: 'workflow.mode.manual.description'
  },
  {
    value: 'auto_select_review',
    labelKey: 'workflow.mode.auto_select_review.label',
    descriptionKey: 'workflow.mode.auto_select_review.description'
  },
  {
    value: 'full_auto',
    labelKey: 'workflow.mode.full_auto.label',
    descriptionKey: 'workflow.mode.full_auto.description'
  }
];

const workflowModeLabel = (mode: ProcessingMode, t: TFunction) => {
  const option = workflowModeOptions.find((entry) => entry.value === mode);
  return option ? t(option.labelKey) : mode;
};

const workflowModeDescription = (mode: ProcessingMode, t: TFunction) => {
  const option = workflowModeOptions.find((entry) => entry.value === mode);
  return option ? t(option.descriptionKey) : '';
};

const defaultCounts: Counts = {
  total_documents: 0,
  complete: 0,
  missing_ocr: 0,
  missing_tagging: 0,
  missing_title: 0,
  missing_correspondent: 0,
  missing_document_type: 0,
  missing_document_date: 0,
  missing_fields: 0,
  waiting_review: 0,
  failed: 0,
  running: 0,
  never_processed: 0
};

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
        {tab === 'dashboard' && <Dashboard setError={setError} canManageSettings={canManageSettings} />}
        {tab === 'inventory' && <Inventory setError={setError} />}
        {tab === 'chat' && canUseChat && <DocumentChat setError={setError} />}
        {tab === 'reviews' && <Reviews setError={setError} />}
        {tab === 'settings' && <SettingsPage setError={setError} />}
        {tab === 'prompts' && <Prompts setError={setError} />}
        {tab === 'audit' && <Audit setError={setError} />}
        {tab === 'users' && <Users setError={setError} />}
      </main>
    </div>
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

function AlertsBar({
  items,
  onAction
}: {
  items: NeedsAttentionItem[];
  onAction: (item: NeedsAttentionItem) => void;
}) {
  const { t, formatNumber } = useI18n();
  if (items.length === 0) return null;
  return (
    <section className="alerts-bar" role="region" aria-label={t('dashboard.alerts.title')}>
      <header>
        <AlertTriangle size={16} />
        <strong>{t('dashboard.alerts.title')}</strong>
      </header>
      <ul>
        {items.map((item, idx) => (
          <li key={`${item.kind}-${idx}`} className={`alert-item severity-${item.severity}`}>
            <div className="alert-text">
              <strong>{item.title}</strong>
              <span>{item.description}</span>
            </div>
            {item.count != null && (
              <span className="alert-count" aria-label="count">{formatNumber(item.count)}</span>
            )}
            {item.action_key && (
              <button type="button" onClick={() => onAction(item)} className="alert-action">
                {localizedMessage(item.action_key, t, item.action_key)}
              </button>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

function HealthBadge({ score, generatedAt }: { score: number | null; generatedAt: string | null }) {
  const { t, formatRelativeTime } = useI18n();
  const tone = healthTone(score);
  const label = score == null
    ? t('dashboard.health.score_label')
    : tone === 'success'
      ? t('dashboard.health.healthy')
      : tone === 'warning'
        ? t('dashboard.health.degraded')
        : t('dashboard.health.unhealthy');
  return (
    <div className={`health-badge ${tone}`} role="status">
      <span className="health-score">{score ?? '-'}</span>
      <div>
        <strong>{label}</strong>
        <em>{generatedAt ? formatRelativeTime(generatedAt) : '-'}</em>
      </div>
    </div>
  );
}

type StageMatrixRow = {
  stage: string;
  queued: number;
  running: number;
  failed: number;
  complete: number;
  avg_ms: number;
  p95_ms: number;
  throughput_per_hour: number;
  bottleneck_score: number;
};

type StageMatrixSortKey = 'stage' | 'queued' | 'running' | 'failed' | 'avg_ms' | 'p95_ms' | 'throughput_per_hour';

function buildStageMatrix(stats: DashboardStats | null): StageMatrixRow[] {
  const stages = stats?.stage_status?.length ? stats.stage_status : defaultStageStatus;
  const usageByStage = new Map<string, { total_avg: number; count: number; max_p95: number }>();
  for (const usage of stats?.provider_usage ?? []) {
    const slot = usageByStage.get(usage.stage) ?? { total_avg: 0, count: 0, max_p95: 0 };
    slot.total_avg += usage.avg_duration_ms * usage.request_count;
    slot.count += usage.request_count;
    slot.max_p95 = Math.max(slot.max_p95, usage.p95_duration_ms);
    usageByStage.set(usage.stage, slot);
  }
  return stages.map((stage) => {
    const usage = usageByStage.get(stage.stage);
    const avg_ms = usage && usage.count > 0 ? usage.total_avg / usage.count : 0;
    const p95_ms = usage?.max_p95 ?? 0;
    const throughput_per_hour = avg_ms > 0 ? 3_600_000 / avg_ms : 0;
    const queued = stage.pending;
    const bottleneck_score = throughput_per_hour > 0 ? queued / throughput_per_hour : queued > 0 ? Number.POSITIVE_INFINITY : 0;
    return {
      stage: stage.stage,
      queued,
      running: stage.running,
      failed: stage.failed,
      complete: stage.complete,
      avg_ms,
      p95_ms,
      throughput_per_hour,
      bottleneck_score
    };
  });
}

function StageMatrix({ stats, onStageSelect }: { stats: DashboardStats | null; onStageSelect: (stage: string) => void }) {
  const { t, formatNumber } = useI18n();
  const [sortKey, setSortKey] = useState<StageMatrixSortKey>('queued');
  const [sortDir, setSortDir] = useState<'asc' | 'desc'>('desc');
  const rows = useMemo(() => buildStageMatrix(stats), [stats]);
  const sorted = useMemo(() => {
    const arr = [...rows];
    arr.sort((a, b) => {
      const va = a[sortKey];
      const vb = b[sortKey];
      if (typeof va === 'string' && typeof vb === 'string') {
        return sortDir === 'asc' ? va.localeCompare(vb) : vb.localeCompare(va);
      }
      const diff = (va as number) - (vb as number);
      return sortDir === 'asc' ? diff : -diff;
    });
    return arr;
  }, [rows, sortKey, sortDir]);
  const bottleneckStage = useMemo(() => {
    let pick: StageMatrixRow | null = null;
    for (const row of rows) {
      if (row.bottleneck_score > 0 && (!pick || row.bottleneck_score > pick.bottleneck_score)) {
        pick = row;
      }
    }
    return pick?.queued && pick.bottleneck_score > 1 ? pick.stage : null;
  }, [rows]);

  const handleSort = (key: StageMatrixSortKey) => {
    if (sortKey === key) {
      setSortDir((dir) => (dir === 'asc' ? 'desc' : 'asc'));
    } else {
      setSortKey(key);
      setSortDir('desc');
    }
  };

  const arrow = (key: StageMatrixSortKey) => (sortKey === key ? (sortDir === 'asc' ? ' ▲' : ' ▼') : '');

  return (
    <ChartPanel title={t('dashboard.stage_matrix.title')} wide>
      <p className="chart-panel-subtitle">{t('dashboard.stage_matrix.subtitle')}</p>
      <div className="table-wrap compact-table stage-matrix-table">
        <table>
          <thead>
            <tr>
              <th><button type="button" onClick={() => handleSort('stage')}>{t('dashboard.stage_matrix.stage')}{arrow('stage')}</button></th>
              <th><button type="button" onClick={() => handleSort('queued')}>{t('dashboard.stage_matrix.queued')}{arrow('queued')}</button></th>
              <th><button type="button" onClick={() => handleSort('running')}>{t('dashboard.stage_matrix.running')}{arrow('running')}</button></th>
              <th><button type="button" onClick={() => handleSort('failed')}>{t('dashboard.stage_matrix.failed')}{arrow('failed')}</button></th>
              <th><button type="button" onClick={() => handleSort('avg_ms')}>{t('dashboard.stage_matrix.avg')}{arrow('avg_ms')}</button></th>
              <th><button type="button" onClick={() => handleSort('p95_ms')}>{t('dashboard.stage_matrix.p95')}{arrow('p95_ms')}</button></th>
              <th><button type="button" onClick={() => handleSort('throughput_per_hour')}>{t('dashboard.stage_matrix.throughput_per_hour')}{arrow('throughput_per_hour')}</button></th>
              <th>{t('dashboard.stage_matrix.complete')}</th>
              <th>{' '}</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((row) => (
              <tr key={row.stage} className={bottleneckStage === row.stage ? 'is-bottleneck' : ''}>
                <td>
                  <button className="link-button" type="button" onClick={() => onStageSelect(row.stage)}>
                    {stageLabel(row.stage, t)}
                  </button>
                </td>
                <td>{formatNumber(row.queued)}</td>
                <td>{formatNumber(row.running)}</td>
                <td className={row.failed > 0 ? 'cell-danger' : ''}>{formatNumber(row.failed)}</td>
                <td>{formatMs(row.avg_ms)}</td>
                <td>{formatMs(row.p95_ms)}</td>
                <td>{row.throughput_per_hour > 0 ? formatNumber(Math.round(row.throughput_per_hour)) : '-'}</td>
                <td>{formatNumber(row.complete)}</td>
                <td>{bottleneckStage === row.stage && <span className="bottleneck-badge">{t('dashboard.stage_matrix.bottleneck')}</span>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </ChartPanel>
  );
}

type TimelineEntry = {
  id: string;
  at: string;
  kind: 'llm' | 'failure';
  stage: string;
  primary: string;
  secondary?: string;
};

function ActivityTimeline({ live }: { live: DashboardLiveStatus | null }) {
  const { t, formatRelativeTime } = useI18n();
  const entries = useMemo<TimelineEntry[]>(() => {
    const llm = (live?.recent_llm_events ?? []).map<TimelineEntry>((event) => ({
      id: `llm-${event.id}`,
      at: event.created_at,
      kind: 'llm',
      stage: event.stage,
      primary: t('dashboard.timeline.llm_event', { provider: event.provider, model: event.model, stage: stageLabel(event.stage, t) }),
      secondary: event.duration_ms ? formatMs(event.duration_ms) : undefined
    }));
    const failures = (live?.recent_failures ?? []).map<TimelineEntry>((failure) => ({
      id: `failure-${failure.id}`,
      at: failure.updated_at,
      kind: 'failure',
      stage: failure.stage,
      primary: t('dashboard.timeline.failure', { document: failure.paperless_document_id, stage: stageLabel(failure.stage, t) }),
      secondary: failure.error_message
    }));
    return [...llm, ...failures].sort((a, b) => new Date(b.at).getTime() - new Date(a.at).getTime()).slice(0, 30);
  }, [live, t]);
  return (
    <section className="activity-timeline">
      <header>
        <History size={16} />
        <strong>{t('dashboard.timeline.title')}</strong>
      </header>
      {entries.length === 0 ? (
        <p className="empty-state compact">{t('dashboard.timeline.empty')}</p>
      ) : (
        <ol>
          {entries.map((entry) => (
            <li key={entry.id} className={`timeline-entry kind-${entry.kind}`}>
              <time>{formatRelativeTime(entry.at)}</time>
              <div>
                <strong>{entry.primary}</strong>
                {entry.secondary && <span>{entry.secondary}</span>}
              </div>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}

function MaintenanceDrawer({
  open,
  onClose,
  recovery,
  consistency,
  reconcile,
  recoveryBusy,
  maintenanceBusy,
  onRefreshRecovery,
  onRecoverLeases,
  onRecoverRuns,
  onCheckConsistency,
  onDryRunReconcile,
  onApplyReconcile,
  queueBusy,
  onQueueSync,
  onQueueOcr,
  onQueueTags,
  onQueueFull
}: {
  open: boolean;
  onClose: () => void;
  recovery: { older_than_seconds: number; items: RecoveryCandidate[] } | null;
  consistency: PaperlessConsistencyResult | null;
  reconcile: CompletionTagReconcileResult | null;
  recoveryBusy: boolean;
  maintenanceBusy: boolean;
  onRefreshRecovery: () => void;
  onRecoverLeases: () => void;
  onRecoverRuns: () => void;
  onCheckConsistency: () => void;
  onDryRunReconcile: () => void;
  onApplyReconcile: () => void;
  queueBusy: boolean;
  onQueueSync: () => void;
  onQueueOcr: () => void;
  onQueueTags: () => void;
  onQueueFull: () => void;
}) {
  const { t } = useI18n();
  useEffect(() => {
    if (!open) return;
    const handler = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <div className="drawer-root" role="dialog" aria-modal="true" aria-label={t('dashboard.maintenance.title')}>
      <div className="drawer-backdrop" onClick={onClose} />
      <aside className="drawer">
        <header>
          <strong>{t('dashboard.maintenance.title')}</strong>
          <button type="button" className="drawer-close" onClick={onClose} aria-label={t('dashboard.maintenance.close')}>
            <X size={18} />
          </button>
        </header>
        <RecoveryPanel
          recovery={recovery}
          busy={recoveryBusy}
          onRefresh={onRefreshRecovery}
          onRecoverLeases={onRecoverLeases}
          onRecoverRuns={onRecoverRuns}
        />
        <PaperlessMaintenancePanel
          consistency={consistency}
          reconcile={reconcile}
          busy={maintenanceBusy}
          onCheckConsistency={onCheckConsistency}
          onDryRunReconcile={onDryRunReconcile}
          onApplyReconcile={onApplyReconcile}
        />
        <section className="drawer-section">
          <strong>{t('dashboard.maintenance.queue_section')}</strong>
          <div className="toolbar">
            <ActionButton icon={<RefreshCw />} label={t('dashboard.action.sync')} busy={queueBusy} onClick={onQueueSync} />
            <ActionButton icon={<FileText />} label={t('dashboard.action.queue_ocr')} busy={queueBusy} onClick={onQueueOcr} />
            <ActionButton icon={<Tags />} label={t('dashboard.action.queue_tags')} busy={queueBusy} onClick={onQueueTags} />
            <ActionButton icon={<Play />} label={t('dashboard.action.queue_full')} busy={queueBusy} onClick={onQueueFull} />
          </div>
        </section>
      </aside>
    </div>
  );
}

function QuotaBar({
  label,
  remaining,
  limit
}: {
  label: string;
  remaining: number | null | undefined;
  limit: number | null | undefined;
}) {
  const { t, formatNumber } = useI18n();
  if (!limit || limit <= 0) {
    return (
      <div className="quota-bar unlimited">
        <span>{label}</span>
        <em>{t('dashboard.workflow.quota_unlimited')}</em>
      </div>
    );
  }
  const safeRemaining = remaining ?? limit;
  const used = Math.max(0, limit - safeRemaining);
  const pct = Math.min(100, Math.round((used / limit) * 100));
  const tone = pct >= 90 ? 'danger' : pct >= 70 ? 'warning' : 'success';
  return (
    <div className={`quota-bar ${tone}`}>
      <header>
        <span>{label}</span>
        <em>{t('dashboard.workflow.quota_used', { used: formatNumber(used), limit: formatNumber(limit) })}</em>
      </header>
      <div className="quota-track" role="progressbar" aria-valuenow={used} aria-valuemin={0} aria-valuemax={limit}>
        <div className="quota-fill" style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}

function Dashboard({ setError, canManageSettings }: { setError: (error: string | null) => void; canManageSettings: boolean }) {
  const { t, formatNumber, formatPercent, formatRelativeTime: formatRelative } = useI18n();
  const [counts, setCounts] = useState<Counts>(defaultCounts);
  const [stats, setStats] = useState<DashboardStats | null>(null);
  const [live, setLive] = useState<DashboardLiveStatus | null>(null);
  const [range, setRange] = useState<DashboardRange>('24h');
  const [busy, setBusy] = useState(false);
  const [modeBusy, setModeBusy] = useState(false);
  const [recoveryBusy, setRecoveryBusy] = useState(false);
  const [paperlessToolsBusy, setPaperlessToolsBusy] = useState(false);
  const [recovery, setRecovery] = useState<{ older_than_seconds: number; items: RecoveryCandidate[] } | null>(null);
  const [consistency, setConsistency] = useState<PaperlessConsistencyResult | null>(null);
  const [reconcile, setReconcile] = useState<CompletionTagReconcileResult | null>(null);
  const [lastLoadedAt, setLastLoadedAt] = useState<string | null>(null);
  const [drawerOpen, setDrawerOpen] = useState(() => {
    if (typeof window === 'undefined') return false;
    return window.localStorage.getItem('dashboard.drawer_open') === 'true';
  });
  useEffect(() => {
    if (typeof window === 'undefined') return;
    window.localStorage.setItem('dashboard.drawer_open', String(drawerOpen));
  }, [drawerOpen]);
  const load = () =>
    api
      .dashboard(range)
      .then((data) => {
        setCounts(data.counts);
        setStats(data.stats);
        setLastLoadedAt(new Date().toISOString());
      })
      .catch((err) => setError(localizedErrorMessage(err, t)));
  const loadLive = () =>
    api
      .dashboardLive()
      .then(setLive)
      .catch((err) => setError(localizedErrorMessage(err, t)));
  const loadRecovery = () =>
    api
      .recoveryStatus()
      .then(setRecovery)
      .catch((err) => setError(localizedErrorMessage(err, t)));

  useEffect(() => {
    void load();
    const timer = window.setInterval(() => {
      void load();
    }, 30000);
    return () => window.clearInterval(timer);
  }, [range]);

  useEffect(() => {
    void loadLive();
    if (canManageSettings) void loadRecovery();
    const timer = window.setInterval(() => {
      void loadLive();
      if (canManageSettings) void loadRecovery();
    }, 5000);
    return () => window.clearInterval(timer);
  }, [canManageSettings]);

  const updateDashboardWorkflowMode = async (nextMode: ProcessingMode) => {
    const settings = await api.updateWorkflowMode(nextMode);
    setLive((current) =>
      current
        ? {
            ...current,
            workflow_mode: settings.workflow.mode,
            autopilot_enabled: settings.workflow.mode !== 'manual_review' && !settings.workflow.paused,
            workflow_safety: {
              ...current.workflow_safety,
              paused: settings.workflow.paused,
              dry_run: settings.workflow.dry_run,
              hourly_document_limit: settings.workflow.hourly_document_limit,
              daily_document_limit: settings.workflow.daily_document_limit
            }
          }
        : current
    );
    await loadLive();
  };
  const updateDashboardPause = async (paused: boolean) => {
    const settings = await api.updateWorkflowControls({ paused });
    setLive((current) =>
      current
        ? {
            ...current,
            autopilot_enabled: settings.workflow.mode !== 'manual_review' && !settings.workflow.paused,
            workflow_safety: {
              ...current.workflow_safety,
              paused: settings.workflow.paused,
              dry_run: settings.workflow.dry_run,
              hourly_document_limit: settings.workflow.hourly_document_limit,
              daily_document_limit: settings.workflow.daily_document_limit
            }
          }
        : current
    );
    await loadLive();
  };
  const recoverStaleLeases = async () => {
    await api.recoverStaleLeases(recovery?.older_than_seconds ?? 600);
    await Promise.all([loadLive(), loadRecovery()]);
  };
  const recoverStuckRuns = async () => {
    await api.recoverStuckRuns(recovery?.older_than_seconds ?? 600);
    await Promise.all([load(), loadLive(), loadRecovery()]);
  };
  const checkPaperlessConsistency = async () => {
    const result = await api.paperlessConsistency();
    setConsistency(result);
  };
  const reconcileCompletionTags = async (dryRun: boolean) => {
    const result = await api.reconcileCompletionTags({ dry_run: dryRun });
    setReconcile(result);
    await Promise.all([load(), loadLive()]);
  };

  const openBacklog = counts.total_documents - counts.complete;
  const stageData = (stats?.stage_status.length ? stats.stage_status : defaultStageStatus).map((stage) => ({
    stage: stageLabel(stage.stage, t),
    Complete: stage.complete,
    Pending: stage.pending,
    Review: stage.waiting_review,
    Running: stage.running,
    Failed: stage.failed
  }));
  const jobStatusData = statusChartData(stats?.job_status.length ? stats.job_status : defaultJobStatus, t);
  const runStatusData = statusChartData(stats?.run_status.length ? stats.run_status : defaultRunStatus, t);
  const comparison = stats?.comparison;
  const runningJobs = stats?.kpis.running_jobs ?? counts.running;

  const healthScore = computeHealthScore(stats, live);
  const heroMetric = {
    label: t('dashboard.metric.open_backlog'),
    value: stats?.kpis.open_backlog ?? openBacklog,
    tone: 'warning' as const,
    delta: comparison?.open_backlog_delta
  };
  const secondaryMetrics = [
    { label: t('dashboard.metric.throughput'), value: stats?.kpis.throughput ?? 0, tone: 'success', delta: comparison?.jobs_succeeded_delta },
    { label: t('dashboard.metric.completion'), value: formatPercent(stats?.kpis.completion_rate ?? 0), tone: 'neutral', delta: null },
    { label: t('dashboard.metric.mttc'), value: formatMttc(stats?.kpis.mttc_seconds), tone: 'neutral', delta: null },
    { label: t('dashboard.metric.cost', { range }), value: formatCost(stats?.kpis.cost_in_range_usd), tone: 'neutral', delta: null }
  ];
  const tertiaryMetrics = [
    { label: t('dashboard.metric.running_now'), value: runningJobs, tone: 'info', delta: null },
    { label: t('dashboard.metric.review_queue'), value: counts.waiting_review, tone: 'review', delta: null },
    { label: t('dashboard.metric.failed'), value: counts.failed, tone: 'danger', delta: comparison?.jobs_failed_delta },
    { label: t('dashboard.metric.p95_latency'), value: formatMs(stats?.kpis.p95_stage_duration_ms ?? 0), tone: 'neutral', delta: null }
  ];

  const onAlertAction = (item: NeedsAttentionItem) => {
    if (!canManageSettings) return;
    switch (item.kind) {
      case 'stuck_runs':
        void run(setRecoveryBusy, setError, recoverStuckRuns, t);
        break;
      case 'stale_leases':
        void run(setRecoveryBusy, setError, recoverStaleLeases, t);
        break;
      default:
        break;
    }
  };

  return (
    <section className="page dashboard-page">
      <div className="dashboard-heading">
        <div className="dashboard-heading-main">
          <PageHeader title={t('dashboard.title')} />
          <p>
            {t('dashboard.last_refresh', { time: lastLoadedAt ? formatRelative(lastLoadedAt) : '-' })}
          </p>
          <HealthBadge score={healthScore} generatedAt={stats?.generated_at ?? null} />
        </div>
        <div className="dashboard-heading-actions">
          <div className="range-tabs" aria-label={t('dashboard.range_label')}>
            {(stats?.available_ranges ?? defaultDashboardRanges).map((option) => (
              <button
                key={option.key}
                className={range === option.key ? 'active' : ''}
                onClick={() => setRange(option.key)}
              >
                {option.label}
              </button>
            ))}
          </div>
          <button
            className="primary-button"
            disabled={busy}
            onClick={() => void run(setBusy, setError, async () => Promise.all([load(), loadLive()]))}
          >
            <RefreshCw size={16} /> {busy ? t('generic.refreshing') : t('generic.refresh')}
          </button>
          {canManageSettings && (
            <button
              type="button"
              className="secondary-button drawer-toggle"
              onClick={() => setDrawerOpen((open) => !open)}
              aria-expanded={drawerOpen}
            >
              <Settings size={16} /> {t('dashboard.maintenance.toggle')}
            </button>
          )}
        </div>
      </div>

      <AlertsBar items={live?.needs_attention ?? []} onAction={onAlertAction} />

      <div className="operations-strip">
        <AutoProcessingCard
          enabled={live?.autopilot_enabled ?? false}
          mode={live?.workflow_mode ?? 'manual_review'}
          safety={live?.workflow_safety}
          nextSelectorScanAt={live?.next_selector_scan_at}
          busy={modeBusy}
          canToggle={canManageSettings}
          onModeChange={(mode) => void run(setModeBusy, setError, () => updateDashboardWorkflowMode(mode), t)}
          onPauseChange={(paused) => void run(setModeBusy, setError, () => updateDashboardPause(paused), t)}
        />
        <ServiceStatusCard label={t('dashboard.live.selector')} icon={<ListChecks size={18} />} status={live?.selector} />
        <ServiceStatusCard label="LLM" icon={<Activity size={18} />} status={live?.llm} />
        <ServiceStatusCard label="Paperless" icon={<Database size={18} />} status={live?.paperless} />
      </div>

      {canManageSettings && (
        <MaintenanceDrawer
          open={drawerOpen}
          onClose={() => setDrawerOpen(false)}
          recovery={recovery}
          consistency={consistency}
          reconcile={reconcile}
          recoveryBusy={recoveryBusy}
          maintenanceBusy={paperlessToolsBusy}
          onRefreshRecovery={() => void run(setRecoveryBusy, setError, loadRecovery, t)}
          onRecoverLeases={() => void run(setRecoveryBusy, setError, recoverStaleLeases, t)}
          onRecoverRuns={() => void run(setRecoveryBusy, setError, recoverStuckRuns, t)}
          onCheckConsistency={() => void run(setPaperlessToolsBusy, setError, checkPaperlessConsistency, t)}
          onDryRunReconcile={() => void run(setPaperlessToolsBusy, setError, () => reconcileCompletionTags(true), t)}
          onApplyReconcile={() => void run(setPaperlessToolsBusy, setError, () => reconcileCompletionTags(false), t)}
          queueBusy={busy}
          onQueueSync={() => void run(setBusy, setError, api.syncPaperless, t).then(load)}
          onQueueOcr={() => void run(setBusy, setError, api.queueOcr, t).then(load)}
          onQueueTags={() => void run(setBusy, setError, api.queueTags, t).then(load)}
          onQueueFull={() => void run(setBusy, setError, api.queueFull, t).then(load)}
        />
      )}

      <div className="kpi-grid">
        <div className={`metric hero ${heroMetric.tone}`}>
          <span>{heroMetric.label}</span>
          <strong>{typeof heroMetric.value === 'number' ? formatNumber(heroMetric.value) : heroMetric.value}</strong>
          {typeof heroMetric.delta === 'number' && (
            <em className={deltaTone(heroMetric.delta)}>{formatDelta(heroMetric.delta, t, formatNumber)}</em>
          )}
        </div>
        <div className="kpi-secondary">
          {secondaryMetrics.map(({ label, value, tone, delta }) => (
            <div className={`metric ${tone}`} key={label}>
              <span>{label}</span>
              <strong>{typeof value === 'number' ? formatNumber(value) : value}</strong>
              {typeof delta === 'number' && <em className={deltaTone(delta)}>{formatDelta(delta, t, formatNumber)}</em>}
            </div>
          ))}
        </div>
        <div className="kpi-tertiary">
          {tertiaryMetrics.map(({ label, value, tone, delta }) => (
            <div className={`metric ${tone}`} key={label}>
              <span>{label}</span>
              <strong>{typeof value === 'number' ? formatNumber(value) : value}</strong>
              {typeof delta === 'number' && <em className={deltaTone(delta)}>{formatDelta(delta, t, formatNumber)}</em>}
            </div>
          ))}
        </div>
      </div>

      <div className="dashboard-ops-grid">
        <div className="dashboard-analytics">
          <ChartPanel title={t('dashboard.chart.throughput', { range })} wide>
            <ResponsiveContainer width="100%" height={280}>
              <AreaChart data={stats?.throughput_series ?? []}>
                <CartesianGrid strokeDasharray="3 3" vertical={false} />
                <XAxis dataKey="label" />
                <YAxis allowDecimals={false} />
                <Tooltip />
                <Legend />
                <Area type="monotone" dataKey="jobs_created" name={t('dashboard.chart.created')} stroke="#28649b" fill="#dbe9f5" />
                <Area type="monotone" dataKey="jobs_succeeded" name={t('dashboard.chart.succeeded')} stroke="#147f7a" fill="#d9eeee" />
                <Area type="monotone" dataKey="jobs_failed" name={t('dashboard.chart.failed')} stroke="#a6403a" fill="#f5dddd" />
              </AreaChart>
            </ResponsiveContainer>
          </ChartPanel>
          <StageMatrix stats={stats} onStageSelect={() => undefined} />
          <div className="dashboard-grid compact">
            <ChartPanel title={t('dashboard.chart.backlog_trend')}>
              <ResponsiveContainer width="100%" height={240}>
                <AreaChart data={stats?.backlog_series ?? []}>
                  <CartesianGrid strokeDasharray="3 3" vertical={false} />
                  <XAxis dataKey="label" />
                  <YAxis allowDecimals={false} />
                  <Tooltip />
                  <Legend />
                  <Area type="monotone" dataKey="open_backlog" name={t('dashboard.chart.open')} stroke="#a9782b" fill="#f1e5d0" />
                  <Area type="monotone" dataKey="complete" name={t('dashboard.chart.complete')} stroke="#147f7a" fill="#d9eeee" />
                  <Area type="monotone" dataKey="failed" name={t('dashboard.chart.failed')} stroke="#a6403a" fill="#f5dddd" />
                </AreaChart>
              </ResponsiveContainer>
            </ChartPanel>
            <ChartPanel title={t('dashboard.chart.queue_state')}>
              <ResponsiveContainer width="100%" height={240}>
                <BarChart data={jobStatusData} layout="vertical" margin={{ left: 12 }}>
                  <CartesianGrid strokeDasharray="3 3" horizontal={false} />
                  <XAxis type="number" allowDecimals={false} />
                  <YAxis type="category" dataKey="label" width={92} />
                  <Tooltip />
                  <Bar dataKey="count" fill="#28649b" radius={[0, 4, 4, 0]} />
                </BarChart>
              </ResponsiveContainer>
            </ChartPanel>
          </div>
        </div>
        <LiveProcessingPanel live={live} />
      </div>

      <ActivityTimeline live={live} />

      <div className="quality-strip">
        <div>
          <span>{t('dashboard.quality.review_decisions')}</span>
          <strong>{formatNumber(stats?.quality.review_decisions ?? 0)}</strong>
        </div>
        <div>
          <span>{t('dashboard.quality.acceptance')}</span>
          <strong>{stats?.quality.acceptance_rate == null ? '-' : formatPercent(stats.quality.acceptance_rate)}</strong>
        </div>
        <div>
          <span>{t('dashboard.quality.edited')}</span>
          <strong>{formatNumber(stats?.quality.review_edited ?? 0)}</strong>
        </div>
        <div>
          <span>{t('dashboard.quality.rejected')}</span>
          <strong>{formatNumber(stats?.quality.review_rejected ?? 0)}</strong>
        </div>
        <div>
          <span>{t('dashboard.quality.uncertainty')}</span>
          <strong>{formatNumber(stats?.quality.uncertainty_reviews ?? 0)}</strong>
        </div>
      </div>

      <ChartPanel title={t('dashboard.chart.provider_usage')} wide>
        <div className="table-wrap compact-table">
          <table>
            <thead>
              <tr>
                <th>{t('dashboard.provider.provider')}</th>
                <th>{t('dashboard.provider.model')}</th>
                <th>{t('dashboard.provider.stage')}</th>
                <th>{t('dashboard.provider.requests')}</th>
                <th>{t('dashboard.provider.avg')}</th>
                <th>{t('dashboard.provider.p95')}</th>
                <th>{t('dashboard.provider.tokens')}</th>
                <th>{t('dashboard.provider.cost')}</th>
                <th>{t('dashboard.provider.feedback')}</th>
                <th>{t('dashboard.provider.acceptance')}</th>
              </tr>
            </thead>
            <tbody>
              {(stats?.provider_usage ?? []).length === 0 && (
                <tr><td colSpan={10}>{t('dashboard.provider.no_usage')}</td></tr>
              )}
              {(stats?.provider_usage ?? []).map((item) => (
                <tr key={`${item.provider}-${item.model}-${item.stage}`}>
                  <td>{item.provider}</td>
                  <td>{item.model}</td>
                  <td>{stageLabel(item.stage, t)}</td>
                  <td>{item.request_count}</td>
                  <td>{formatMs(item.avg_duration_ms)}</td>
                  <td>{formatMs(item.p95_duration_ms)}</td>
                  <td>{item.input_tokens + item.output_tokens}</td>
                  <td>{formatCost(item.estimated_cost_usd)}</td>
                  <td>{item.positive_feedback}/{item.negative_feedback}</td>
                  <td>{item.acceptance_rate == null ? '-' : formatPercent(item.acceptance_rate)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </ChartPanel>
    </section>
  );
}

function AutoProcessingCard({
  enabled,
  mode,
  safety,
  nextSelectorScanAt,
  busy,
  canToggle,
  onModeChange,
  onPauseChange
}: {
  enabled: boolean;
  mode: ProcessingMode;
  safety?: DashboardLiveStatus['workflow_safety'] | null;
  nextSelectorScanAt?: string | null;
  busy: boolean;
  canToggle: boolean;
  onModeChange: (mode: ProcessingMode) => void;
  onPauseChange: (paused: boolean) => void;
}) {
  const { t, formatRelativeTime: formatRelative } = useI18n();
  const paused = safety?.paused ?? false;
  const dryRun = safety?.dry_run ?? false;
  const hasQuota = !!(safety?.hourly_document_limit || safety?.daily_document_limit);
  return (
    <section className={`autopilot-card ${enabled ? 'enabled' : 'disabled'}`}>
      <div className="autopilot-body">
        <span>{t('dashboard.workflow.title')}</span>
        <strong>{workflowModeLabel(mode, t)}</strong>
        <p>{workflowModeDescription(mode, t)}</p>
        {dryRun && (
          <div className="dry-run-banner" role="alert">
            <AlertTriangle size={16} />
            <div>
              <strong>{t('dashboard.workflow.dry_run_banner_title')}</strong>
              <span>{t('dashboard.workflow.dry_run_banner_body')}</span>
            </div>
          </div>
        )}
        {hasQuota && (
          <div className="quota-bars">
            <QuotaBar
              label={t('dashboard.workflow.hourly_quota')}
              remaining={safety?.hourly_remaining}
              limit={safety?.hourly_document_limit}
            />
            <QuotaBar
              label={t('dashboard.workflow.daily_quota')}
              remaining={safety?.daily_remaining}
              limit={safety?.daily_document_limit}
            />
          </div>
        )}
        {nextSelectorScanAt && (
          <small>{t('dashboard.auto.next_scan', { time: formatRelative(nextSelectorScanAt) })}</small>
        )}
      </div>
      <div className="mode-button-group" role="group" aria-label={t('dashboard.auto.processing_mode')}>
        <button
          type="button"
          disabled={busy || !canToggle}
          aria-pressed={paused}
          onClick={() => onPauseChange(!paused)}
        >
          <Power size={16} /> {paused ? t('dashboard.auto.resume') : t('dashboard.auto.pause')}
        </button>
        {workflowModeOptions.map((option) => (
          <button
            key={option.value}
            className={mode === option.value ? 'active' : ''}
            type="button"
            disabled={busy || !canToggle || mode === option.value}
            aria-pressed={mode === option.value}
            onClick={() => onModeChange(option.value)}
            title={t(option.descriptionKey)}
          >
            {option.value === 'manual_review' ? <Power size={16} /> : <Play size={16} />}
            {mode === option.value && busy ? t('dashboard.auto.updating') : t(option.labelKey)}
          </button>
        ))}
        {!canToggle && <small>{t('generic.admin_only')}</small>}
      </div>
    </section>
  );
}

function ServiceStatusCard({
  label,
  icon,
  status
}: {
  label: string;
  icon: ReactNode;
  status?: DashboardLiveStatus['llm'] | null;
}) {
  const { t, formatRelativeTime: formatRelative } = useI18n();
  const state = status?.state ?? 'idle';
  return (
    <section className={`service-card ${state}`}>
      <header>
        <span>{icon}</span>
        <strong>{label}</strong>
        <Status value={state} />
      </header>
      <p>{status?.title ?? t('dashboard.service.loading')}</p>
      <small>{status?.description ?? t('dashboard.service.waiting')}</small>
      <em>{status?.last_event_at ? formatRelative(status.last_event_at) : t('dashboard.service.no_activity')}</em>
    </section>
  );
}

function RecoveryPanel({
  recovery,
  busy,
  onRefresh,
  onRecoverLeases,
  onRecoverRuns
}: {
  recovery: { older_than_seconds: number; items: RecoveryCandidate[] } | null;
  busy: boolean;
  onRefresh: () => void;
  onRecoverLeases: () => void;
  onRecoverRuns: () => void;
}) {
  const { t, formatNumber } = useI18n();
  const items = recovery?.items ?? [];
  const staleLeases = items.filter((item) => item.reason === 'stale_lease').length;
  const stuckRuns = items.filter((item) => item.reason === 'stuck_run_without_active_jobs').length;
  return (
    <section className="recovery-panel">
      <div>
        <strong>{t('dashboard.recovery.title')}</strong>
        <p>{t('dashboard.recovery.description')}</p>
      </div>
      <div className="recovery-counts">
        <span>{t('dashboard.recovery.stale_leases')}: {formatNumber(staleLeases)}</span>
        <span>{t('dashboard.recovery.stuck_runs')}: {formatNumber(stuckRuns)}</span>
      </div>
      <div className="toolbar">
        <button type="button" disabled={busy} onClick={onRefresh}>
          <RefreshCw size={16} /> {t('generic.refresh')}
        </button>
        <button type="button" disabled={busy || staleLeases === 0} onClick={onRecoverLeases}>
          <RotateCcw size={16} /> {t('dashboard.recovery.requeue_leases')}
        </button>
        <button type="button" disabled={busy || stuckRuns === 0} onClick={onRecoverRuns}>
          <AlertTriangle size={16} /> {t('dashboard.recovery.recover_runs')}
        </button>
      </div>
    </section>
  );
}

function PaperlessMaintenancePanel({
  consistency,
  reconcile,
  busy,
  onCheckConsistency,
  onDryRunReconcile,
  onApplyReconcile
}: {
  consistency: PaperlessConsistencyResult | null;
  reconcile: CompletionTagReconcileResult | null;
  busy: boolean;
  onCheckConsistency: () => void;
  onDryRunReconcile: () => void;
  onApplyReconcile: () => void;
}) {
  const { t, formatNumber } = useI18n();
  const mismatchCount = consistency?.mismatches.length ?? 0;
  const missingCount = consistency?.missing_local.length ?? 0;
  const staleCount = consistency?.stale_local.length ?? 0;
  const plannedCount = reconcile?.planned.length ?? 0;
  const appliedCount = reconcile?.applied.length ?? 0;
  return (
    <section className="recovery-panel paperless-maintenance-panel">
      <div>
        <strong>{t('dashboard.paperless_tools.title')}</strong>
        <p>{t('dashboard.paperless_tools.description')}</p>
      </div>
      <div className="recovery-counts">
        {consistency ? (
          <>
            <span>{t('dashboard.paperless_tools.checked')}: {formatNumber(consistency.documents_checked)}</span>
            <span>{t('dashboard.paperless_tools.missing')}: {formatNumber(missingCount)}</span>
            <span>{t('dashboard.paperless_tools.stale')}: {formatNumber(staleCount)}</span>
            <span>{t('dashboard.paperless_tools.mismatches')}: {formatNumber(mismatchCount)}</span>
          </>
        ) : (
          <span>{t('dashboard.paperless_tools.not_checked')}</span>
        )}
        {reconcile && (
          <span>
            {reconcile.dry_run
              ? t('dashboard.paperless_tools.planned')
              : t('dashboard.paperless_tools.applied')}: {formatNumber(reconcile.dry_run ? plannedCount : appliedCount)}
          </span>
        )}
      </div>
      <div className="toolbar">
        <button type="button" disabled={busy} onClick={onCheckConsistency}>
          <GitCompare size={16} /> {t('dashboard.paperless_tools.check')}
        </button>
        <button type="button" disabled={busy} onClick={onDryRunReconcile}>
          <Tags size={16} /> {t('dashboard.paperless_tools.dry_run')}
        </button>
        <button type="button" disabled={busy || !reconcile?.dry_run || plannedCount === 0} onClick={onApplyReconcile}>
          <Check size={16} /> {t('dashboard.paperless_tools.apply')}
        </button>
      </div>
    </section>
  );
}

function LiveProcessingPanel({ live }: { live: DashboardLiveStatus | null }) {
  const { t, formatNumber, formatRelativeTime: formatRelative } = useI18n();
  const activeJobs = live?.active_jobs ?? [];
  const activeRuns = live?.active_runs ?? [];
  const recentEvents = live?.recent_llm_events ?? [];
  const recentFailures = live?.recent_failures ?? [];
  const hardFailures = recentFailures.filter((failure) => liveFailureKind(failure) === 'failed').length;

  return (
    <aside className="live-processing-panel">
      <header>
        <div>
          <strong>{t('dashboard.live.title')}</strong>
          <span>{t('dashboard.live.subtitle')}</span>
        </div>
        <Status value={live?.workflow_mode ?? 'loading'} />
      </header>
      <div className="live-summary">
        <div>
          <span>{t('dashboard.live.runs')}</span>
          <strong>{formatNumber(activeRuns.length)}</strong>
        </div>
        <div>
          <span>{t('dashboard.live.jobs')}</span>
          <strong>{formatNumber(activeJobs.length)}</strong>
        </div>
        <div>
          <span>{t('dashboard.live.issues')}</span>
          <strong>{formatNumber(hardFailures || recentFailures.length)}</strong>
        </div>
      </div>

      <section className="live-debug-section">
        <h3>{t('dashboard.live.active_jobs')}</h3>
        {activeJobs.length === 0 && <p className="empty-state compact">{t('dashboard.live.no_active_jobs')}</p>}
        {activeJobs.slice(0, 8).map((job) => {
          const elapsedMs = Math.max(0, Date.now() - new Date(job.updated_at).getTime());
          return (
            <article className="live-job" key={job.id}>
              <div>
                <strong>{t('review.document', { id: job.paperless_document_id })}</strong>
                <span>
                  {stageLabel(job.stage, t)} · {t('dashboard.live.attempt', { attempts: job.attempts, max: job.max_attempts })}
                  {' · '}
                  {t('dashboard.live.elapsed', { time: formatMs(elapsedMs) })}
                </span>
              </div>
              <Status value={job.status} />
              <small>
                {job.lease_owner ? t('dashboard.live.worker', { worker: job.lease_owner }) : formatRelative(job.updated_at)}
                {' · '}
                {t('dashboard.live.trace', { trace: shortId(job.trace_id) })}
              </small>
            </article>
          );
        })}
      </section>

      <section className="live-debug-section">
        <h3>{t('dashboard.live.latest_llm')}</h3>
        {recentEvents.length === 0 && <p className="empty-state compact">{t('dashboard.live.no_llm')}</p>}
        {recentEvents.slice(0, 5).map((event) => (
          <article className="live-event" key={event.id}>
            <strong>{event.provider} / {event.model}</strong>
            <span>{stageLabel(event.stage, t)} · {formatMs(event.duration_ms ?? 0)} · {formatRelative(event.created_at)}</span>
          </article>
        ))}
      </section>

      <section className="live-debug-section">
        <h3>{t('dashboard.live.recent_failures')}</h3>
        {recentFailures.length === 0 && <p className="empty-state compact">{t('dashboard.live.no_failures')}</p>}
        {recentFailures.slice(0, 5).map((failure) => {
          const kind = liveFailureKind(failure);
          return (
            <article className={`live-failure ${kind !== 'failed' ? 'retry' : ''}`} key={failure.id}>
              <div className="failure-heading">
                <strong>{t('dashboard.live.document_stage', { document: failure.paperless_document_id, stage: stageLabel(failure.stage, t) })}</strong>
                <Status value={kind} />
              </div>
              <span>{failure.error_message}</span>
              <small>{liveFailureTiming(failure, kind, t, formatRelative)}</small>
            </article>
          );
        })}
      </section>
    </aside>
  );
}

const defaultDashboardRanges: Array<{ key: DashboardRange; label: string }> = [
  { key: '24h', label: '24h' },
  { key: '7d', label: '7d' },
  { key: '30d', label: '30d' },
  { key: '90d', label: '90d' },
  { key: '12m', label: '12m' },
  { key: 'all', label: 'All' }
];

const defaultStageStatus = ['ocr', 'title', 'document_type', 'correspondent', 'document_date', 'tags', 'fields'].map((stage) => ({
  stage,
  complete: 0,
  pending: 0,
  failed: 0,
  waiting_review: 0,
  running: 0
}));

const defaultJobStatus: DashboardStatusCount[] = [
  { status: 'queued', count: 0 },
  { status: 'running', count: 0 },
  { status: 'succeeded', count: 0 },
  { status: 'failed', count: 0 }
];

const defaultRunStatus: DashboardStatusCount[] = [
  { status: 'queued', count: 0 },
  { status: 'running', count: 0 },
  { status: 'waiting_review', count: 0 },
  { status: 'succeeded', count: 0 },
  { status: 'failed', count: 0 }
];

function ChartPanel({ title, wide, children }: { title: string; wide?: boolean; children: ReactNode }) {
  return (
    <section className={`chart-panel${wide ? ' wide' : ''}`}>
      <h3>{title}</h3>
      {children}
    </section>
  );
}

function statusChartData(items: DashboardStatusCount[], t: TFunction) {
  return items.map((item) => ({
    ...item,
    label: statusLabel(item.status, t)
  }));
}

function liveFailureKind(failure: DashboardLiveFailure) {
  return failure.failure_kind || (failure.status === 'failed' ? 'failed' : 'retry_scheduled');
}

function liveFailureTiming(
  failure: DashboardLiveFailure,
  kind: string,
  t: TFunction,
  formatRelative: (value?: string | null) => string
) {
  if (kind === 'retry_ready') return t('dashboard.live.retry_ready');
  if (failure.next_attempt_at) return t('dashboard.live.next_retry', { time: formatRelative(failure.next_attempt_at) });
  return t('dashboard.live.updated', { time: formatRelative(failure.updated_at) });
}

function stageLabel(stage: string, t?: TFunction) {
  const labels: Record<string, string> = {
    ocr: 'OCR',
    ocr_fix: 'OCR Fix',
    title: 'Title',
    document_type: 'Type',
    document_date: 'Date',
    correspondent: 'Correspondent',
    tags: 'Tags',
    fields: 'Fields',
    apply: 'Apply'
  };
  return t ? localizedMessage(`stage.${stage}`, t, labels[stage] ?? statusLabel(stage, t)) : labels[stage] ?? statusLabel(stage);
}

function statusLabel(value: string, t?: TFunction) {
  if (t) return localizedMessage(`status.${value}`, t, titleCaseStatus(value));
  return titleCaseStatus(value);
}

function titleCaseStatus(value: string) {
  return value
    .split('_')
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ');
}

function formatDelta(value: number, t: TFunction, formatNumber: (value: number) => string) {
  if (value === 0) return t('delta.zero');
  return t('delta.value', { value: `${value > 0 ? '+' : ''}${formatNumber(value)}` });
}

function formatPercent(value: number) {
  if (!Number.isFinite(value)) return '0%';
  return `${Math.round(value * 100)}%`;
}

function formatRelativeTime(value?: string | null) {
  if (!value) return '-';
  const timestamp = new Date(value).getTime();
  if (!Number.isFinite(timestamp)) return '-';
  const deltaSeconds = Math.round((Date.now() - timestamp) / 1000);
  const future = deltaSeconds < 0;
  const seconds = Math.abs(deltaSeconds);
  if (seconds < 10) return future ? 'in a few seconds' : 'just now';
  if (seconds < 60) return future ? `in ${seconds}s` : `${seconds}s ago`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return future ? `in ${minutes}m` : `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return future ? `in ${hours}h` : `${hours}h ago`;
  return new Date(value).toLocaleString();
}

function formatMs(value: number) {
  if (!Number.isFinite(value) || value <= 0) return '-';
  if (value >= 1000) return `${(value / 1000).toFixed(1)}s`;
  return `${Math.round(value)}ms`;
}

function shortId(value: string) {
  return value.slice(0, 8);
}

function formatCost(value?: number | null) {
  if (value == null) return '-';
  if (value === 0) return '$0.00';
  if (value < 0.01) return `<$0.01`;
  if (value >= 100) return `$${Math.round(value)}`;
  return `$${value.toFixed(2)}`;
}

function formatMttc(value?: number | null) {
  if (value == null || !Number.isFinite(value) || value <= 0) return '-';
  if (value < 60) return `${Math.round(value)}s`;
  if (value < 3600) {
    const minutes = Math.floor(value / 60);
    const seconds = Math.round(value % 60);
    return seconds > 0 ? `${minutes}m ${seconds}s` : `${minutes}m`;
  }
  const hours = Math.floor(value / 3600);
  const minutes = Math.round((value % 3600) / 60);
  return minutes > 0 ? `${hours}h ${minutes}m` : `${hours}h`;
}

function computeHealthScore(stats: DashboardStats | null, live: DashboardLiveStatus | null) {
  if (!stats) return null;
  let score = 100;
  score -= Math.min(50, stats.kpis.failure_rate * 100);
  const backlogDelta = stats.comparison?.open_backlog_delta ?? 0;
  if (backlogDelta > 0 && stats.kpis.open_backlog > 0) {
    const pct = (backlogDelta / stats.kpis.open_backlog) * 100;
    score -= Math.min(25, pct * 0.5);
  }
  const critical = live?.needs_attention?.filter((item) => item.severity === 'critical').length ?? 0;
  score -= critical * 15;
  return Math.max(0, Math.round(score));
}

function healthTone(score: number | null): 'success' | 'warning' | 'danger' | 'neutral' {
  if (score == null) return 'neutral';
  if (score >= 90) return 'success';
  if (score >= 70) return 'warning';
  return 'danger';
}

function formatLanguageDetection(item: InventoryItem, languages: ReturnType<typeof languageOptions>) {
  const tag = item.detected_language;
  if (!tag) return '-';
  const option = languages.find((language) => language.tag === tag);
  const label = option ? option.uiName : tag;
  const confidence = item.detected_language_confidence;
  if (confidence == null) return label;
  return `${label} ${Math.round(confidence * 100)}%`;
}

function deltaTone(value: number) {
  if (value > 0) return 'delta up';
  if (value < 0) return 'delta down';
  return 'delta';
}

function Inventory({ setError }: { setError: (error: string | null) => void }) {
  const { t, locale } = useI18n();
  const [items, setItems] = useState<InventoryItem[]>([]);
  const [busy, setBusy] = useState(false);
  const languages = useMemo(() => languageOptions(locale), [locale]);
  const load = () => api.inventory().then((data) => setItems(data.items)).catch((err) => setError(localizedErrorMessage(err, t)));

  useEffect(() => {
    void load();
  }, []);

  return (
    <section className="page">
      <PageHeader title={t('inventory.title')} />
      <div className="toolbar">
        <ActionButton icon={<RefreshCw />} label={t('generic.reload')} busy={busy} onClick={() => run(setBusy, setError, load, t)} />
      </div>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>{t('inventory.id')}</th>
              <th>{t('inventory.document_title')}</th>
              <th>{t('inventory.ocr')}</th>
              <th>{t('inventory.language')}</th>
              <th>{t('inventory.tags')}</th>
              <th>{t('stage.title')}</th>
              <th>{t('inventory.type')}</th>
              <th>{t('inventory.date')}</th>
              <th>{t('inventory.run')}</th>
              <th>{t('inventory.debug')}</th>
              <th>{t('inventory.actions')}</th>
            </tr>
          </thead>
          <tbody>
            {items.map((item) => (
              <tr key={item.paperless_document_id}>
                <td>{item.paperless_document_id}</td>
                <td>{item.title || item.original_file_name || t('inventory.untitled')}</td>
                <td><Status value={item.ocr_status} /></td>
                <td>{formatLanguageDetection(item, languages)}</td>
                <td><Status value={item.tagging_status} /></td>
                <td><Status value={item.title_status} /></td>
                <td><Status value={item.document_type_status} /></td>
                <td><Status value={item.document_date_status} /> {item.document_date ?? ''}</td>
                <td>{item.current_run_status || '-'}</td>
                <td><DebugContextDetails context={item.debug_context} compact /></td>
                <td className="row-actions">
                  <button title={t('inventory.trigger_ocr')} onClick={() => api.triggerDocument(item.paperless_document_id, ['ocr'], 'manual_review').then(load).catch((err) => setError(localizedErrorMessage(err, t)))}>
                    <FileText size={16} />
                  </button>
                  <button title={t('inventory.trigger_tags')} onClick={() => api.triggerDocument(item.paperless_document_id, ['tags'], 'manual_review').then(load).catch((err) => setError(localizedErrorMessage(err, t)))}>
                    <Tags size={16} />
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function DocumentChat({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime } = useI18n();
  const [sessions, setSessions] = useState<DocumentChatSession[]>([]);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [messages, setMessages] = useState<DocumentChatMessage[]>([]);
  const [sessionTitle, setSessionTitle] = useState('Document chat');
  const [question, setQuestion] = useState('');
  const [documentIds, setDocumentIds] = useState('');
  const [busy, setBusy] = useState(false);

  const loadSessions = () =>
    api.chatSessions().then((data) => {
      setSessions(data.items);
      setActiveSessionId((current) => current ?? data.items[0]?.id ?? null);
    }).catch((err) => setError(localizedErrorMessage(err, t)));

  const loadMessages = (sessionId: string) =>
    api.chatMessages(sessionId).then((data) => setMessages(data.items)).catch((err) => setError(localizedErrorMessage(err, t)));

  useEffect(() => {
    void loadSessions();
  }, []);

  useEffect(() => {
    if (activeSessionId) {
      void loadMessages(activeSessionId);
    } else {
      setMessages([]);
    }
  }, [activeSessionId]);

  const createSession = async () => {
    const created = await api.createChatSession(sessionTitle);
    setSessions((current) => [{ id: created.id, title: created.title, created_at: new Date().toISOString(), updated_at: new Date().toISOString() }, ...current]);
    setActiveSessionId(created.id);
    setMessages([]);
  };

  const sendMessage = async () => {
    const trimmed = question.trim();
    if (!trimmed) return;
    const ids = parseDocumentIds(documentIds);
    if (ids === false) {
      setError('Document IDs must be up to 50 comma-separated positive numbers');
      return;
    }

    const sessionId = activeSessionId ?? (await api.createChatSession(chatTitleFromQuestion(trimmed))).id;
    if (!activeSessionId) {
      setActiveSessionId(sessionId);
      await loadSessions();
    }

    await api.postChatMessage(sessionId, {
      question: trimmed,
      document_ids: ids,
      max_sources: 6
    });
    setQuestion('');
    await Promise.all([loadSessions(), loadMessages(sessionId)]);
  };

  return (
    <section className="page chat-page">
      <PageHeader title="Document Chat" />
      <div className="chat-layout">
        <aside className="chat-sessions">
          <form
            className="chat-session-form"
            onSubmit={(event) => {
              event.preventDefault();
              void run(setBusy, setError, createSession);
            }}
          >
            <input value={sessionTitle} onChange={(event) => setSessionTitle(event.target.value)} />
            <button title="New chat" disabled={busy}><MessageSquare size={16} /></button>
          </form>
          <div className="chat-session-list">
            {sessions.map((session) => (
              <button
                key={session.id}
                className={session.id === activeSessionId ? 'active' : ''}
                title={session.title}
                onClick={() => setActiveSessionId(session.id)}
              >
                <span>{session.title}</span>
                <small>{formatDateTime(session.updated_at)}</small>
              </button>
            ))}
          </div>
        </aside>
        <div className="chat-panel">
          <div className="chat-messages">
            {messages.length === 0 && <div className="empty-state">No messages</div>}
            {messages.map((message) => (
              <article className={`chat-message ${message.role}`} key={message.id}>
                <header>
                  <strong>{message.role === 'assistant' ? 'Archivist' : 'You'}</strong>
                  {message.model && <span>{message.provider} / {message.model}</span>}
                </header>
                <p>{message.content}</p>
                {message.sources.length > 0 && (
                  <div className="chat-sources">
                    {message.sources.map((source, index) => (
                      <details key={`${message.id}-${source.paperless_document_id}-${index}`}>
                        <summary>
                          Document {source.paperless_document_id}
                          {source.title ? ` - ${source.title}` : ''}
                        </summary>
                        <p>{source.snippet}</p>
                      </details>
                    ))}
                  </div>
                )}
              </article>
            ))}
          </div>
          <form
            className="chat-composer"
            onSubmit={(event) => {
              event.preventDefault();
              void run(setBusy, setError, sendMessage);
            }}
          >
            <label>
              Document IDs
              <input value={documentIds} onChange={(event) => setDocumentIds(event.target.value)} placeholder="12, 98" />
            </label>
            <label className="wide">
              Question
              <textarea value={question} onChange={(event) => setQuestion(event.target.value)} required />
            </label>
            <button className="primary-button" title="Send" disabled={busy || !question.trim()}>
              <Send size={16} /> Send
            </button>
          </form>
        </div>
      </div>
    </section>
  );
}

function parseDocumentIds(value: string): number[] | null | false {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const ids = trimmed.split(',').map((part) => Number(part.trim()));
  if (ids.some((id) => !Number.isInteger(id) || id <= 0)) return false;
  const uniqueIds = Array.from(new Set(ids));
  if (uniqueIds.length > 50) return false;
  return uniqueIds;
}

function chatTitleFromQuestion(question: string) {
  return question.length > 70 ? `${question.slice(0, 67)}...` : question;
}

function Reviews({ setError }: { setError: (error: string | null) => void }) {
  const { t } = useI18n();
  const [items, setItems] = useState<ReviewItem[]>([]);
  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const load = () => api.reviews().then((data) => setItems(data.items)).catch((err) => setError(localizedErrorMessage(err, t)));

  useEffect(() => {
    void load();
  }, []);

  const toggleSelected = (id: string) => {
    setSelected((current) => current.includes(id) ? current.filter((item) => item !== id) : [...current, id]);
  };
  const batch = async (decision: 'approve' | 'reject') => {
    await run(setBusy, setError, async () => {
      const result = await api.batchReview(selected, decision);
      if (result.failed.length > 0) {
        setError(t('review.failed_batch', { count: result.failed.length, error: result.failed[0].error }));
      }
      setSelected([]);
      await load();
    }, t);
  };

  return (
    <section className="page">
      <PageHeader title={t('review.title')} />
      <div className="toolbar">
        <button disabled={items.length === 0} onClick={() => setSelected(selected.length === items.length ? [] : items.map((item) => item.id))}>
          <ListChecks size={16} /> {selected.length === items.length ? t('review.clear_selection') : t('review.select_all')}
        </button>
        <button disabled={busy || selected.length === 0} onClick={() => void batch('approve')}>
          <Check size={16} /> {t('review.approve_selected')}
        </button>
        <button disabled={busy || selected.length === 0} onClick={() => void batch('reject')}>
          <X size={16} /> {t('review.reject_selected')}
        </button>
      </div>
      <div className="review-list">
        {items.map((item) => (
          <ReviewCard
            key={item.id}
            item={item}
            selected={selected.includes(item.id)}
            onSelect={() => toggleSelected(item.id)}
            onReload={load}
            setError={setError}
            t={t}
          />
        ))}
      </div>
    </section>
  );
}

type ReviewPatchRecord = Record<string, unknown> & {
  standard_metadata?: Record<string, unknown>;
};

type ReviewEditState = {
  title?: string;
  correspondent?: string;
  document_type?: string;
  created?: string;
};

function ReviewCard({
  item,
  selected,
  onSelect,
  onReload,
  setError,
  t
}: {
  item: ReviewItem;
  selected: boolean;
  onSelect: () => void;
  onReload: () => void;
  setError: (error: string | null) => void;
  t: TFunction;
}) {
  const patch = asReviewPatch(item.suggested_patch);
  const metadata = asReviewPatch(patch?.standard_metadata);
  const [edit, setEdit] = useState<ReviewEditState>(() => reviewEditStateFromPatch(patch));
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setEdit(reviewEditStateFromPatch(patch));
  }, [item.id]);

  const applyEdited = async () => {
    if (!patch) {
      setError(t('review.patch_not_editable'));
      return;
    }
    const editedPatch = buildEditedReviewPatch(patch, edit);
    if (editedPatch === null) {
      setError(t('review.invalid_numeric_id'));
      return;
    }
    await run(setBusy, setError, async () => {
      await api.editReview(item.id, editedPatch);
      onReload();
    }, t);
  };

  const warnings = reviewWarnings(item.validation_warnings);
  const rows = standardMetadataRows(item.stage, patch, metadata, t);

  return (
    <article className="review-item">
      <header>
        <label className="inline">
          <input type="checkbox" checked={selected} onChange={onSelect} />
          <strong>{t('review.document', { id: item.paperless_document_id })}</strong>
        </label>
        <span>{stageLabel(item.stage as Stage, t) ?? item.stage}</span>
      </header>

      {rows.length > 0 && (
        <div className="metadata-review">
          {rows.map((row) => (
            <div className={`metadata-review-row ${row.lowConfidence ? 'low-confidence' : ''}`} key={row.field}>
              <div>
                <strong>{row.label}</strong>
                <small>{t('review.current', { value: row.current ?? t('generic.empty') })}</small>
              </div>
              <div>
                <span>{t('review.suggestion', { value: row.suggested ?? t('generic.empty') })}</span>
                {row.confidence !== null && <small>{t('review.confidence', { value: `${(row.confidence * 100).toFixed(0)}%` })}</small>}
                {row.evidence && <small>{t('review.evidence', { value: row.evidence })}</small>}
              </div>
              {row.editableKey && (
                <label>
                  {t('review.edit')}
                  <input
                    type={row.editableKey === 'created' ? 'date' : 'text'}
                    value={edit[row.editableKey] ?? ''}
                    onChange={(event) => setEdit((current) => ({ ...current, [row.editableKey!]: event.target.value }))}
                    placeholder={row.placeholder}
                  />
                </label>
              )}
            </div>
          ))}
        </div>
      )}

      {warnings.length > 0 && (
        <div className="review-warnings">
          {warnings.map((warning) => (
            <span key={warning}><AlertTriangle size={14} /> {warning}</span>
          ))}
        </div>
      )}

      <DebugContextDetails context={item.debug_context} />

      <details>
        <summary>{t('review.raw_patch')}</summary>
        <pre>{JSON.stringify(item.suggested_patch, null, 2)}</pre>
      </details>

      <footer>
        <button title={t('review.approve')} disabled={busy} onClick={() => api.approveReview(item.id).then(onReload).catch((err) => setError(localizedErrorMessage(err, t)))}>
          <Check size={16} /> {t('review.approve')}
        </button>
        {patch && Object.keys(edit).length > 0 && (
          <button title={t('review.apply_edited')} disabled={busy} onClick={() => void applyEdited()}>
            <Save size={16} /> {t('review.apply_edited')}
          </button>
        )}
        <button title={t('review.reject')} disabled={busy} onClick={() => api.rejectReview(item.id).then(onReload).catch((err) => setError(localizedErrorMessage(err, t)))}>
          <X size={16} /> {t('review.reject')}
        </button>
      </footer>
    </article>
  );
}

function asReviewPatch(value: unknown): ReviewPatchRecord | null {
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    return value as ReviewPatchRecord;
  }
  return null;
}

function reviewEditStateFromPatch(patch: ReviewPatchRecord | null): ReviewEditState {
  if (!patch) return {};
  const state: ReviewEditState = {};
  if (Object.prototype.hasOwnProperty.call(patch, 'title')) state.title = String(patch.title ?? '');
  if (Object.prototype.hasOwnProperty.call(patch, 'correspondent')) state.correspondent = String(patch.correspondent ?? '');
  if (Object.prototype.hasOwnProperty.call(patch, 'document_type')) state.document_type = String(patch.document_type ?? '');
  if (Object.prototype.hasOwnProperty.call(patch, 'created')) state.created = String(patch.created ?? '');
  return state;
}

function buildEditedReviewPatch(patch: ReviewPatchRecord, edit: ReviewEditState): Record<string, unknown> | null {
  const edited: Record<string, unknown> = { ...patch };
  delete edited.standard_metadata;
  if (edit.title !== undefined) edited.title = edit.title.trim();
  if (edit.created !== undefined) edited.created = edit.created.trim();
  for (const key of ['correspondent', 'document_type'] as const) {
    if (edit[key] === undefined) continue;
    const trimmed = edit[key]!.trim();
    if (trimmed === '') {
      edited[key] = null;
      continue;
    }
    const numeric = Number(trimmed);
    if (!Number.isInteger(numeric) || numeric <= 0) return null;
    edited[key] = numeric;
  }
  return edited;
}

function DebugContextDetails({
  context,
  compact
}: {
  context?: InventoryItem['debug_context'] | ReviewItem['debug_context'] | null;
  compact?: boolean;
}) {
  const { t } = useI18n();
  if (!context) return <span className="field-hint">-</span>;
  const reason = context.selector_reason ?? context.next_required_stage ?? 'unknown';
  const promptLanguage = context.prompt_language ?? context.detected_language ?? 'und';
  const tagLanguage = context.tag_output_language ?? '-';
  return (
    <details className={`debug-context${compact ? ' compact' : ''}`}>
      <summary>
        {compact
          ? t('inventory.debug_summary', { reason, promptLanguage, tagLanguage })
          : t('review.debug_summary', { reason, promptLanguage, tagLanguage })}
      </summary>
      <dl>
        <div><dt>{t('inventory.language')}</dt><dd>{promptLanguage}</dd></div>
        <div><dt>{t('settings.workflow.tag_output_language')}</dt><dd>{tagLanguage}</dd></div>
        <div><dt>{t('settings.workflow.mode')}</dt><dd>{context.workflow_mode ?? '-'}</dd></div>
        <div><dt>{t('settings.workflow.paused')}</dt><dd>{context.workflow_paused ? t('generic.yes') : t('generic.no')}</dd></div>
        <div><dt>{t('settings.workflow.dry_run')}</dt><dd>{context.dry_run ? t('generic.yes') : t('generic.no')}</dd></div>
        <div><dt>{t('inventory.run')}</dt><dd>{context.current_run_status ?? '-'}</dd></div>
        <div><dt>{t('inventory.debug')}</dt><dd>{reason}</dd></div>
      </dl>
    </details>
  );
}

function reviewWarnings(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.map((warning) => typeof warning === 'string' ? warning : JSON.stringify(warning)).slice(0, 10);
}

function standardMetadataRows(stage: string, patch: ReviewPatchRecord | null, metadata: ReviewPatchRecord | null, t: TFunction) {
  if (!patch) return [];
  const rows: Array<{
    field: string;
    label: string;
    current?: string;
    suggested?: string;
    confidence: number | null;
    evidence?: string;
    lowConfidence: boolean;
    editableKey?: keyof ReviewEditState;
    placeholder?: string;
  }> = [];
  const confidence = numericMetadata(metadata?.confidence);
  if (stage === 'correspondent' && Object.prototype.hasOwnProperty.call(patch, 'correspondent')) {
    rows.push({
      field: 'correspondent',
      label: t('stage.correspondent'),
      current: metadataValue(metadata?.current_correspondent),
      suggested: metadataValue(metadata?.suggested_name) ?? metadataValue(patch.correspondent),
      confidence,
      evidence: metadataValue(metadata?.evidence),
      lowConfidence: confidence !== null && confidence < 0.7,
      editableKey: 'correspondent',
      placeholder: t('review.placeholder.correspondent')
    });
  }
  if (stage === 'document_type' && Object.prototype.hasOwnProperty.call(patch, 'document_type')) {
    rows.push({
      field: 'document_type',
      label: t('stage.document_type'),
      current: metadataValue(metadata?.current_document_type),
      suggested: metadataValue(metadata?.suggested_name) ?? metadataValue(patch.document_type),
      confidence,
      evidence: metadataValue(metadata?.evidence),
      lowConfidence: confidence !== null && confidence < 0.7,
      editableKey: 'document_type',
      placeholder: t('review.placeholder.document_type')
    });
  }
  if (stage === 'document_date' && Object.prototype.hasOwnProperty.call(patch, 'created')) {
    rows.push({
      field: 'document_date',
      label: t('stage.document_date'),
      current: metadataValue(metadata?.current_date),
      suggested: metadataValue(metadata?.suggested_date) ?? metadataValue(patch.created),
      confidence,
      evidence: metadataValue(metadata?.evidence),
      lowConfidence: confidence !== null && confidence < 0.7,
      editableKey: 'created',
      placeholder: t('review.placeholder.document_date')
    });
  }
  return rows;
}

function numericMetadata(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function metadataValue(value: unknown): string | undefined {
  if (value === null || value === undefined || value === '') return undefined;
  if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') return String(value);
  return JSON.stringify(value);
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

function Prompts({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime } = useI18n();
  const [items, setItems] = useState<Prompt[]>([]);
  const [usage, setUsage] = useState<PromptUsage[]>([]);
  const [selectedStage, setSelectedStage] = useState<Stage>('tags');
  const [selectedPromptId, setSelectedPromptId] = useState<string | null>(null);
  const [comparePromptId, setComparePromptId] = useState<string | null>(null);
  const [editorName, setEditorName] = useState('default');
  const [editorContent, setEditorContent] = useState('');
  const [activate, setActivate] = useState(true);
  const [sampleText, setSampleText] = useState('');
  const [sampleDocumentId, setSampleDocumentId] = useState('');
  const [testResult, setTestResult] = useState<PromptTestResponse | null>(null);
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [activating, setActivating] = useState(false);
  const [loading, setLoading] = useState(true);
  const usageByPromptId = useMemo(() => {
    const byId = new Map<string, PromptUsage>();
    usage.forEach((entry) => byId.set(entry.prompt_id, entry));
    return byId;
  }, [usage]);
  const stagePrompts = useMemo(
    () =>
      items
        .filter((prompt) => prompt.stage === selectedStage)
        .sort((left, right) => {
          if (left.name !== right.name) return left.name.localeCompare(right.name);
          return right.version - left.version;
        }),
    [items, selectedStage]
  );
  const activePrompt = useMemo(
    () =>
      [...stagePrompts]
        .filter((prompt) => prompt.active)
        .sort((left, right) => new Date(right.created_at).getTime() - new Date(left.created_at).getTime())[0] ?? null,
    [stagePrompts]
  );
  const selectedPrompt =
    stagePrompts.find((prompt) => prompt.id === selectedPromptId) ?? activePrompt ?? stagePrompts[0] ?? null;
  const comparePrompt = comparePromptId ? stagePrompts.find((prompt) => prompt.id === comparePromptId) ?? null : null;
  const selectedUsage = selectedPrompt ? usageByPromptId.get(selectedPrompt.id) : undefined;
  const promptDirty =
    selectedPrompt == null ||
    editorName.trim() !== selectedPrompt.name ||
    editorContent.trimEnd() !== selectedPrompt.content.trimEnd();
  const stageHelp = promptStageHelp[selectedStage];
  const promptStats = promptTextStats(editorContent);
  const diffStats = comparePrompt && selectedPrompt ? promptDiffStats(comparePrompt.content, editorContent) : null;
  const load = async () => {
    setLoading(true);
    try {
      const [promptData, usageData] = await Promise.all([
        api.prompts(),
        api.promptUsage().catch(() => ({ items: [] as PromptUsage[] }))
      ]);
      setItems(promptData.items);
      setUsage(usageData.items);
    } catch (err) {
      setError(localizedErrorMessage(err, t, 'Could not load prompts'));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void load();
  }, []);

  useEffect(() => {
    if (stagePrompts.length === 0) {
      setSelectedPromptId(null);
      return;
    }
    if (!selectedPromptId || !stagePrompts.some((prompt) => prompt.id === selectedPromptId)) {
      setSelectedPromptId(activePrompt?.id ?? stagePrompts[0].id);
    }
  }, [activePrompt?.id, selectedPromptId, stagePrompts]);

  useEffect(() => {
    if (selectedPrompt) {
      setEditorName(selectedPrompt.name);
      setEditorContent(selectedPrompt.content);
      setActivate(true);
    } else {
      setEditorName('default');
      setEditorContent('');
      setActivate(true);
    }
    setComparePromptId((current) => {
      if (current && stagePrompts.some((prompt) => prompt.id === current && prompt.id !== selectedPrompt?.id)) return current;
      if (activePrompt && activePrompt.id !== selectedPrompt?.id) return activePrompt.id;
      return stagePrompts.find((prompt) => prompt.id !== selectedPrompt?.id)?.id ?? null;
    });
    setTestResult(null);
  }, [activePrompt, selectedPrompt?.id, stagePrompts]);

  return (
    <section className="page">
      <div className="prompt-heading">
        <PageHeader title="Prompt Workbench" />
        <p>
          Review active prompts, tune stage-specific behavior, test outputs, and roll back safely. Edits create a new
          immutable version; older versions remain available.
        </p>
      </div>
      <div className="prompt-workbench">
        <aside className="prompt-stage-rail" aria-label="Prompt stages">
          <header>
            <strong>Pipeline Stages</strong>
            <span>{items.length} versions</span>
          </header>
          {promptStageOrder.map((entry) => {
            const help = promptStageHelp[entry];
            const prompts = items.filter((prompt) => prompt.stage === entry);
            const active = prompts.find((prompt) => prompt.active);
            const usageCount = prompts.reduce((sum, prompt) => sum + (usageByPromptId.get(prompt.id)?.run_count ?? 0), 0);
            return (
              <button
                type="button"
                key={entry}
                className={selectedStage === entry ? 'active' : ''}
                onClick={() => {
                  setSelectedStage(entry);
                  setSelectedPromptId(null);
                  setComparePromptId(null);
                }}
              >
                <span>
                  <strong>{help.label}</strong>
                  <em>{active ? `${active.name} v${active.version}` : 'No prompt yet'}</em>
                </span>
                <small>{prompts.length} versions · {usageCount} runs</small>
              </button>
            );
          })}
        </aside>
        <section className="prompt-editor-card">
          <header className="prompt-card-header">
            <div>
              <div className="prompt-title-row">
                <h3>{stageHelp.label}</h3>
                <PromptInfoTooltip label={`${stageHelp.label} guidance`} help={stageHelp} />
              </div>
              <p>{stageHelp.purpose}</p>
            </div>
            <div className="prompt-header-status">
              {selectedPrompt?.active ? <Status value="active" /> : <Status value="draft" />}
              {promptDirty && <span className="dirty-pill">unsaved edits</span>}
            </div>
          </header>
          {loading ? (
            <div className="empty-state">Loading prompts...</div>
          ) : (
            <>
              <div className="prompt-editor-grid">
                <label>
                  Version
                  <select
                    value={selectedPrompt?.id ?? ''}
                    onChange={(event) => setSelectedPromptId(event.target.value || null)}
                  >
                    {stagePrompts.length === 0 && <option value="">New prompt</option>}
                    {stagePrompts.map((prompt) => (
                      <option key={prompt.id} value={prompt.id}>
                        {promptOptionLabel(prompt)}
                      </option>
                    ))}
                  </select>
                </label>
                <label>
                  Prompt name
                  <input value={editorName} onChange={(event) => setEditorName(event.target.value)} />
                </label>
                <label className="inline prompt-activate-check">
                  <input type="checkbox" checked={activate} onChange={(event) => setActivate(event.target.checked)} />
                  Activate after save
                </label>
              </div>
              <label className="prompt-editor-field">
                Prompt content
                <textarea
                  value={editorContent}
                  onChange={(event) => setEditorContent(event.target.value)}
                  required
                  spellCheck={false}
                />
              </label>
              <div className="prompt-editor-actions">
                <button
                  className="primary-button"
                  disabled={saving || !editorName.trim() || !editorContent.trim()}
                  onClick={() =>
                    run(setSaving, setError, async () => {
                      const result = await api.createPrompt({
                        stage: selectedStage,
                        name: editorName.trim(),
                        content: editorContent.trimEnd(),
                        output_schema: selectedPrompt?.output_schema,
                        activate
                      });
                      await load();
                      setSelectedPromptId(result.id);
                    })
                  }
                >
                  <Save size={16} /> {saving ? 'Saving...' : 'Save New Version'}
                </button>
                <button
                  disabled={!selectedPrompt || !promptDirty}
                  onClick={() => {
                    setEditorName(selectedPrompt?.name ?? 'default');
                    setEditorContent(selectedPrompt?.content ?? '');
                  }}
                >
                  <RotateCcw size={16} /> Reset
                </button>
                <button
                  disabled={activating || !selectedPrompt || selectedPrompt.active}
                  onClick={() =>
                    selectedPrompt &&
                    run(setActivating, setError, async () => {
                      await api.activatePrompt(selectedPrompt.id);
                      await load();
                    })
                  }
                >
                  <Check size={16} /> {activating ? 'Activating...' : 'Activate Selected'}
                </button>
              </div>
              <div className="prompt-stats-grid">
                <PromptStat label="Lines" value={promptStats.lines} />
                <PromptStat label="Words" value={promptStats.words} />
                <PromptStat label="Characters" value={promptStats.characters} />
                <PromptStat label="Runs" value={selectedUsage?.run_count ?? 0} />
              </div>
            </>
          )}
        </section>
        <aside className="prompt-lab-card">
          <section>
            <div className="prompt-section-title">
              <strong>Stage Guide</strong>
              <PromptInfoTooltip label="Prompt editing rules" help={stageHelp} compact />
            </div>
            <p>{stageHelp.expectedOutput}</p>
            <ul>
              {stageHelp.safety.map((item) => <li key={item}>{item}</li>)}
            </ul>
          </section>
          <section>
            <div className="prompt-section-title">
              <strong>Usage</strong>
              <History size={16} />
            </div>
            {selectedUsage ? (
              <dl className="prompt-usage">
                <div><dt>Runs</dt><dd>{selectedUsage.run_count}</dd></div>
                <div><dt>Jobs</dt><dd>{selectedUsage.job_count}</dd></div>
                <div><dt>Last used</dt><dd>{selectedUsage.last_used_at ? formatDateTime(selectedUsage.last_used_at) : '-'}</dd></div>
                <div><dt>Model</dt><dd>{[selectedUsage.last_provider, selectedUsage.last_model].filter(Boolean).join(' / ') || '-'}</dd></div>
                <div><dt>Avg duration</dt><dd>{formatMs(selectedUsage.avg_duration_ms)}</dd></div>
              </dl>
            ) : (
              <p className="field-hint">This prompt version has not been used by a worker run yet.</p>
            )}
          </section>
          <section>
            <div className="prompt-section-title">
              <strong>Version History</strong>
              <span>{stagePrompts.length}</span>
            </div>
            <div className="prompt-version-list">
              {stagePrompts.map((prompt) => (
                <button
                  key={prompt.id}
                  type="button"
                  className={prompt.id === selectedPrompt?.id ? 'active' : ''}
                  onClick={() => setSelectedPromptId(prompt.id)}
                >
                  <span>{prompt.name} v{prompt.version}</span>
                  <small>{prompt.active ? 'active' : formatDateTime(prompt.created_at)}</small>
                </button>
              ))}
              {stagePrompts.length === 0 && <p className="field-hint">No prompt exists for this stage yet.</p>}
            </div>
          </section>
        </aside>
      </div>
      <div className="prompt-lab-grid">
        <section className="prompt-test-card">
          <header className="prompt-section-title">
            <strong>Prompt Test Runner</strong>
            <span>{stageHelp.shortLabel}</span>
          </header>
          <div className="prompt-test-grid">
            <label>
              Test document ID
              <input value={sampleDocumentId} onChange={(event) => setSampleDocumentId(event.target.value)} placeholder="optional" />
            </label>
            <label className="wide">
              Test sample text
              <textarea
                value={sampleText}
                onChange={(event) => setSampleText(event.target.value)}
                placeholder="Optional; overrides document ID for prompt tests."
              />
            </label>
          </div>
          <button
            className="primary-button"
            type="button"
            disabled={testing || !editorContent.trim()}
            onClick={() => run(setTesting, setError, async () => {
              const documentId = sampleDocumentId.trim() ? Number(sampleDocumentId) : null;
              const result = await api.testPrompt({
                stage: selectedStage,
                content: editorContent,
                sample_text: sampleText.trim() || undefined,
                paperless_document_id: documentId && Number.isFinite(documentId) ? documentId : null
              });
              setTestResult(result);
            })}
          >
            <Play size={16} /> {testing ? 'Testing...' : 'Test Current Editor'}
          </button>
          {testResult && (
            <section className="test-result">
              <header>
                <strong>{testResult.provider} / {testResult.model}</strong>
                <span>{formatMs(testResult.duration_ms)}</span>
                <Status value={testResult.validation_errors.length === 0 ? 'valid' : 'failed'} />
              </header>
              {testResult.validation_errors.length > 0 && (
                <ul>
                  {testResult.validation_errors.map((error) => <li key={error}>{error}</li>)}
                </ul>
              )}
              {testResult.warnings.length > 0 && (
                <ul className="prompt-warning-list">
                  {testResult.warnings.map((warning) => <li key={warning}><AlertTriangle size={14} /> {warning}</li>)}
                </ul>
              )}
              <details open>
                <summary>Parsed output</summary>
                <pre>{JSON.stringify(testResult.parsed ?? null, null, 2)}</pre>
              </details>
              <details>
                <summary>Raw model response</summary>
                <pre>{testResult.raw_text}</pre>
              </details>
            </section>
          )}
        </section>
        <section className="prompt-compare-card">
          <header className="prompt-section-title">
            <strong>Version Compare</strong>
            <GitCompare size={16} />
          </header>
          <label>
            Compare against
            <select value={comparePromptId ?? ''} onChange={(event) => setComparePromptId(event.target.value || null)}>
              <option value="">No comparison</option>
              {stagePrompts
                .filter((prompt) => prompt.id !== selectedPrompt?.id)
                .map((prompt) => (
                  <option key={prompt.id} value={prompt.id}>{promptOptionLabel(prompt)}</option>
                ))}
            </select>
          </label>
          {diffStats ? (
            <>
              <div className="prompt-diff-summary">
                <PromptStat label="Changed lines" value={diffStats.changedLines} />
                <PromptStat label="Added lines" value={diffStats.addedLines} />
                <PromptStat label="Removed lines" value={diffStats.removedLines} />
              </div>
              <div className="prompt-diff">
                <div>
                  <strong>{comparePrompt?.name} v{comparePrompt?.version}</strong>
                  <pre>{comparePrompt?.content}</pre>
                </div>
                <div>
                  <strong>Current editor</strong>
                  <pre>{editorContent}</pre>
                </div>
              </div>
            </>
          ) : (
            <p className="field-hint">Select another version to compare it with the current editor content.</p>
          )}
        </section>
      </div>
    </section>
  );
}

function promptOptionLabel(prompt: Prompt) {
  return `${prompt.name} v${prompt.version}${prompt.active ? ' (active)' : ''}`;
}

function promptTextStats(value: string) {
  const trimmed = value.trim();
  return {
    lines: value ? value.split(/\r?\n/).length : 0,
    words: trimmed ? trimmed.split(/\s+/).length : 0,
    characters: value.length
  };
}

function promptDiffStats(before: string, after: string) {
  const beforeLines = before.split(/\r?\n/);
  const afterLines = after.split(/\r?\n/);
  const max = Math.max(beforeLines.length, afterLines.length);
  let changedLines = 0;
  for (let index = 0; index < max; index += 1) {
    if ((beforeLines[index] ?? '') !== (afterLines[index] ?? '')) changedLines += 1;
  }
  return {
    changedLines,
    addedLines: Math.max(afterLines.length - beforeLines.length, 0),
    removedLines: Math.max(beforeLines.length - afterLines.length, 0)
  };
}

function PromptStat({ label, value }: { label: string; value: number }) {
  return (
    <div className="prompt-stat">
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function PromptInfoTooltip({
  label,
  help,
  compact
}: {
  label: string;
  help: PromptStageHelp;
  compact?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const tooltipId = useId();
  const shellRef = useRef<HTMLSpanElement | null>(null);

  useEffect(() => {
    if (!open) return undefined;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false);
    };
    const onPointerDown = (event: MouseEvent | TouchEvent) => {
      if (shellRef.current && !shellRef.current.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener('keydown', onKeyDown);
    document.addEventListener('mousedown', onPointerDown);
    document.addEventListener('touchstart', onPointerDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
      document.removeEventListener('mousedown', onPointerDown);
      document.removeEventListener('touchstart', onPointerDown);
    };
  }, [open]);

  return (
    <span
      className="tooltip-shell prompt-tooltip-shell"
      ref={shellRef}
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
    >
      <button
        type="button"
        className="info-button"
        aria-label={label}
        aria-describedby={open ? tooltipId : undefined}
        onClick={() => setOpen((value) => !value)}
        onFocus={() => setOpen(true)}
      >
        <Info size={16} />
      </button>
      {open && (
        <span className={`prompt-info-tooltip${compact ? ' compact' : ''}`} id={tooltipId} role="tooltip">
          <strong>{help.label}</strong>
          <span>{help.purpose}</span>
          {!compact && (
            <>
              <em>{help.expectedOutput}</em>
              <ul>
                {help.safety.map((item) => <li key={item}>{item}</li>)}
              </ul>
            </>
          )}
        </span>
      )}
    </span>
  );
}

function Audit({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime, formatNumber } = useI18n();
  const [items, setItems] = useState<AuditEvent[]>([]);
  const [integrity, setIntegrity] = useState<AuditIntegrityReport | null>(null);
  const [retentionResult, setRetentionResult] = useState<RetentionResult | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    Promise.all([api.audit(), api.auditIntegrity()])
      .then(([auditData, integrityData]) => {
        setItems(auditData.items);
        setIntegrity(integrityData);
      })
      .catch((err) => setError(localizedErrorMessage(err, t)));
  }, [setError, t]);
  const refreshIntegrity = () => api.auditIntegrity()
    .then(setIntegrity)
    .catch((err) => setError(localizedErrorMessage(err, t)));
  return (
    <section className="page">
      <PageHeader title="Audit Log" />
      <div className="toolbar">
        <a className="button-link" href="/api/audit/export.csv">
          <FileText size={16} /> Export CSV
        </a>
        <button onClick={refreshIntegrity}>
          <Shield size={16} /> Verify chain
        </button>
        <ActionButton
          icon={<Archive />}
          label="Apply retention"
          busy={busy}
          onClick={() => run(setBusy, setError, () => api.applyAuditRetention().then((result) => {
            setRetentionResult(result);
            return Promise.all([api.audit(), api.auditIntegrity()]).then(([auditData, integrityData]) => {
              setItems(auditData.items);
              setIntegrity(integrityData);
            });
          }), t)}
        />
      </div>
      {integrity && (
        <div className={`connection-feedback ${integrity.ok ? 'success' : 'error'}`}>
          <header>
            {integrity.ok ? <Check size={16} /> : <X size={16} />}
            <strong>{integrity.ok ? 'Audit chain verified' : 'Audit chain problem'}</strong>
          </header>
          <p>
            Checked {formatNumber(integrity.checked_events)} hashed events.
            {integrity.legacy_events > 0 ? ` ${formatNumber(integrity.legacy_events)} legacy events predate hash-chain tracking.` : ''}
            {integrity.broken_reason ? ` ${integrity.broken_reason}` : ''}
          </p>
        </div>
      )}
      {retentionResult && (
        <div className="connection-feedback success">
          <header><Check size={16} /><strong>Retention applied</strong></header>
          <p>
            Deleted {formatNumber(retentionResult.ai_artifacts_deleted)} AI artifacts and {formatNumber(retentionResult.audit_events_deleted)} audit events outside retention.
          </p>
        </div>
      )}
      <div className="table-wrap">
        <table>
          <thead>
            <tr><th>Time</th><th>Event</th><th>Actor</th><th>Document</th><th>Outcome</th><th>Hash</th></tr>
          </thead>
          <tbody>
            {items.map((item) => (
              <tr key={item.id}>
                <td>{formatDateTime(item.created_at)}</td>
                <td>{item.event_type}</td>
                <td>{item.actor_type}</td>
                <td>{item.paperless_document_id || '-'}</td>
                <td><Status value={item.outcome} /></td>
                <td>{item.event_hash ? `${item.event_hash.slice(0, 12)}...` : 'legacy'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function Users({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime } = useI18n();
  const [users, setUsers] = useState<UserItem[]>([]);
  const [sessions, setSessions] = useState<SessionItem[]>([]);
  const [tokens, setTokens] = useState<ApiToken[]>([]);
  const [newToken, setNewToken] = useState<string | null>(null);
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [role, setRole] = useState<Role>('viewer');
  const [tokenName, setTokenName] = useState('');
  const [tokenScopes, setTokenScopes] = useState('runs:read, inventory:read');
  const [tokenExpiresInDays, setTokenExpiresInDays] = useState(90);
  const [resetPasswords, setResetPasswords] = useState<Record<string, string>>({});
  const load = () =>
    Promise.all([api.users(), api.sessions(), api.apiTokens()])
      .then(([userData, sessionData, tokenData]) => {
        setUsers(userData.items);
        setSessions(sessionData.items);
        setTokens(tokenData.items);
      })
      .catch((err) => setError(localizedErrorMessage(err, t)));

  useEffect(() => {
    void load();
  }, []);

  return (
    <section className="page">
      <PageHeader title="Users" />
      <form className="compact-form" onSubmit={(event) => {
        event.preventDefault();
        api.createUser({ username, password, roles: [role] }).then(() => {
          setUsername('');
          setPassword('');
          load();
        }).catch((err) => setError(localizedErrorMessage(err, t)));
      }}>
        <input value={username} onChange={(event) => setUsername(event.target.value)} placeholder="username" aria-label="username" />
        <input value={password} onChange={(event) => setPassword(event.target.value)} placeholder="password" type="password" aria-label="password" />
        <select value={role} aria-label="new user role" onChange={(event) => setRole(event.target.value as Role)}>
          <option value="viewer">viewer</option>
          <option value="reviewer">reviewer</option>
          <option value="operator">operator</option>
          <option value="auditor">auditor</option>
          <option value="admin">admin</option>
        </select>
        <button><UserPlus size={16} /> Create</button>
      </form>
      <div className="table-wrap">
        <table>
          <thead><tr><th>User</th><th>Roles</th><th>Status</th><th>Password</th><th>Actions</th></tr></thead>
          <tbody>
            {users.map((user) => (
              <tr key={user.id}>
                <td>{user.username}</td>
                <td>
                  <select
                    value={user.roles[0] ?? 'viewer'}
                    aria-label={`roles for ${user.username}`}
                    onChange={(event) => api.updateUserRoles(user.id, [event.target.value as Role]).then(load).catch((err) => setError(localizedErrorMessage(err, t)))}
                  >
                    <option value="viewer">viewer</option>
                    <option value="reviewer">reviewer</option>
                    <option value="operator">operator</option>
                    <option value="auditor">auditor</option>
                    <option value="admin">admin</option>
                  </select>
                </td>
                <td>{user.enabled ? 'enabled' : 'disabled'}</td>
                <td className="inline-edit">
                  <input
                    value={resetPasswords[user.id] ?? ''}
                    onChange={(event) => setResetPasswords((current) => ({ ...current, [user.id]: event.target.value }))}
                    type="password"
                    placeholder="new password"
                    aria-label={`new password for ${user.username}`}
                  />
                  <button
                    title="Reset password"
                    aria-label={`Reset password for ${user.username}`}
                    disabled={!resetPasswords[user.id]}
                    onClick={() => api.resetPassword(user.id, resetPasswords[user.id] ?? '').then(() => {
                      setResetPasswords((current) => ({ ...current, [user.id]: '' }));
                      load();
                    }).catch((err) => setError(localizedErrorMessage(err, t)))}
                  >
                    <RotateCcw size={16} />
                  </button>
                </td>
                <td>
                  {user.enabled ? (
                    <button title="Disable user" aria-label={`Disable ${user.username}`} onClick={() => api.disableUser(user.id).then(load).catch((err) => setError(localizedErrorMessage(err, t)))}><Power size={16} /> Disable</button>
                  ) : (
                    <button title="Enable user" aria-label={`Enable ${user.username}`} onClick={() => api.enableUser(user.id).then(load).catch((err) => setError(localizedErrorMessage(err, t)))}><Power size={16} /> Enable</button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <PageHeader title="Sessions" />
      <div className="table-wrap">
        <table>
          <thead><tr><th>User</th><th>Created</th><th>Last Seen</th><th>Expires</th><th>Status</th><th>Action</th></tr></thead>
          <tbody>
            {sessions.map((session) => (
              <tr key={session.id}>
                <td>{session.username}</td>
                <td>{formatDateTime(session.created_at)}</td>
                <td>{session.last_seen_at ? formatDateTime(session.last_seen_at) : '-'}</td>
                <td>{formatDateTime(session.expires_at)}</td>
                <td>{session.revoked_at ? 'revoked' : 'active'}</td>
                <td>
                  {!session.revoked_at && <button title="Revoke session" aria-label={`Revoke session for ${session.username}`} onClick={() => api.revokeSession(session.id).then(load).catch((err) => setError(localizedErrorMessage(err, t)))}><X size={16} /></button>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <PageHeader title="API Tokens" />
      <form className="compact-form" onSubmit={(event) => {
        event.preventDefault();
        api.createApiToken({ name: tokenName, scopes: splitTags(tokenScopes), expires_in_days: tokenExpiresInDays }).then((created) => {
          setNewToken(created.token);
          setTokenName('');
          load();
        }).catch((err) => setError(localizedErrorMessage(err, t)));
      }}>
        <input value={tokenName} onChange={(event) => setTokenName(event.target.value)} placeholder="token name" aria-label="token name" />
        <input value={tokenScopes} onChange={(event) => setTokenScopes(event.target.value)} placeholder="scopes, comma separated" aria-label="token scopes" />
        <input
          type="number"
          min="1"
          max="3650"
          value={tokenExpiresInDays}
          onChange={(event) => setTokenExpiresInDays(Number(event.target.value))}
          aria-label="token expiry days"
        />
        <button><KeyRound size={16} /> Create Token</button>
      </form>
      {newToken && <pre className="token-once">{newToken}</pre>}
      <div className="table-wrap">
        <table>
          <thead><tr><th>Name</th><th>Scopes</th><th>Expires</th><th>Last Used</th><th>Status</th><th>Action</th></tr></thead>
          <tbody>
            {tokens.map((token) => (
              <tr key={token.id}>
                <td>{token.name}</td>
                <td>{token.scopes.join(', ')}</td>
                <td>{token.expires_at ? formatDateTime(token.expires_at) : '-'}</td>
                <td>{token.last_used_at ? formatDateTime(token.last_used_at) : '-'}</td>
                <td>{token.revoked_at ? 'revoked' : 'active'}</td>
                <td>
                  {!token.revoked_at && (
                    <>
                      <button
                        title="Rotate token"
                        aria-label={`Rotate token ${token.name}`}
                        onClick={() => api.rotateApiToken(token.id, { expires_in_days: tokenExpiresInDays }).then((created) => {
                          setNewToken(created.token);
                          load();
                        }).catch((err) => setError(localizedErrorMessage(err, t)))}
                      >
                        <RotateCcw size={16} />
                      </button>
                      <button title="Revoke token" aria-label={`Revoke token ${token.name}`} onClick={() => api.revokeApiToken(token.id).then(load).catch((err) => setError(localizedErrorMessage(err, t)))}><X size={16} /></button>
                    </>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
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

function PageHeader({ title }: { title: string }) {
  return <header className="page-header"><h2>{title}</h2></header>;
}

function Status({ value }: { value: string }) {
  const { t } = useI18n();
  const tone = useMemo(() => {
    if (['succeeded', 'success', 'complete'].includes(value)) return 'success';
    if (['failed', 'error'].includes(value)) return 'danger';
    if (['running', 'queued', 'applying', 'retry_scheduled', 'retry_ready'].includes(value)) return 'info';
    if (['waiting_review', 'review'].includes(value)) return 'review';
    return 'neutral';
  }, [value]);
  return <span className={`status ${tone}`}>{statusLabel(value, t)}</span>;
}

function ActionButton({ icon, label, busy, onClick }: { icon: ReactNode; label: string; busy: boolean; onClick: () => void | Promise<void> }) {
  return <button className="primary-button" title={label} disabled={busy} onClick={onClick}>{icon}{label}</button>;
}

async function run(
  setBusy: (value: boolean) => void,
  setError: (value: string | null) => void,
  action: () => Promise<unknown> | unknown,
  t?: TFunction
) {
  setBusy(true);
  setError(null);
  try {
    await action();
  } catch (err) {
    setError(t ? localizedErrorMessage(err, t) : errorToString(err));
  } finally {
    setBusy(false);
  }
}

function localizedErrorMessage(err: unknown, t: TFunction, fallback = t('generic.request_failed')) {
  const message = errorToString(err);
  const lower = message.toLowerCase();
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return `${t('generic.unauthorized')} ${message}`;
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return `${t('generic.timeout')} ${message}`;
  }
  if (lower.includes('failed to fetch') || lower.includes('network') || lower.includes('connect')) {
    return `${t('generic.network_error')} ${message}`;
  }
  if (!message || message === 'Request failed') return fallback;
  return message;
}

function errorToString(err: unknown) {
  return err instanceof Error ? err.message : typeof err === 'string' ? err : 'Request failed';
}
