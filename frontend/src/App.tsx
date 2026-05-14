import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  Activity,
  Archive,
  Check,
  ClipboardList,
  Database,
  FileText,
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
  Counts,
  DashboardRange,
  DashboardStats,
  DashboardStatusCount,
  DocumentChatMessage,
  DocumentChatSession,
  InventoryItem,
  Me,
  OidcConfig,
  OllamaInstalledModel,
  Prompt,
  PromptTestResponse,
  ReviewItem,
  Role,
  RuntimeSettings,
  SessionItem,
  Stage,
  UserItem
} from './api/client';
import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  CartesianGrid,
  Cell,
  Legend,
  Pie,
  PieChart,
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

const defaultCounts: Counts = {
  total_documents: 0,
  complete: 0,
  missing_ocr: 0,
  missing_tagging: 0,
  missing_title: 0,
  missing_correspondent: 0,
  missing_document_type: 0,
  missing_fields: 0,
  waiting_review: 0,
  failed: 0,
  running: 0,
  never_processed: 0
};

export function App() {
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

  if (loading) return <div className="boot">Paperless Archivist</div>;
  if (!me) return <Login onLogin={setMe} />;

  const canUseChat = me.roles.some((role) => role === 'admin' || role === 'reviewer' || role === 'operator');

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <img src="/assets/brand/paperless-archivist-logo.png" alt="" />
          <div>
            <strong>Paperless Archivist</strong>
            <span>{me.username}</span>
          </div>
        </div>
        <nav>
          <NavButton icon={<Activity />} label="Dashboard" active={tab === 'dashboard'} onClick={() => setTab('dashboard')} />
          <NavButton icon={<Archive />} label="Inventory" active={tab === 'inventory'} onClick={() => setTab('inventory')} />
          {canUseChat && <NavButton icon={<MessageSquare />} label="Chat" active={tab === 'chat'} onClick={() => setTab('chat')} />}
          <NavButton icon={<ListChecks />} label="Review" active={tab === 'reviews'} onClick={() => setTab('reviews')} />
          <NavButton icon={<Settings />} label="Settings" active={tab === 'settings'} onClick={() => setTab('settings')} />
          <NavButton icon={<ClipboardList />} label="Prompts" active={tab === 'prompts'} onClick={() => setTab('prompts')} />
          <NavButton icon={<Shield />} label="Audit" active={tab === 'audit'} onClick={() => setTab('audit')} />
          <NavButton icon={<UserPlus />} label="Users" active={tab === 'users'} onClick={() => setTab('users')} />
        </nav>
        <button
          className="ghost-button"
          title="Logout"
          onClick={async () => {
            await api.logout();
            setMe(null);
          }}
        >
          <LogOut size={18} /> Logout
        </button>
      </aside>

      <main className="workspace">
        {error && (
          <div className="banner error">
            <span>{error}</span>
            <button title="Dismiss" onClick={() => setError(null)}>
              <X size={16} />
            </button>
          </div>
        )}
        {tab === 'dashboard' && <Dashboard setError={setError} />}
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
      setError(err instanceof Error ? err.message : 'Login failed');
    } finally {
      setLoginBusy(false);
    }
  };

  return (
    <main className="login">
      <section className="login-panel">
        <img src="/assets/brand/paperless-archivist-logo.png" alt="" />
        <h1>Paperless Archivist</h1>
        {oidc?.enabled && oidc.login_url && (
          <a className="sso-button" href={oidc.login_url}>
            <KeyRound size={18} /> Login with {oidc.provider ?? 'SSO'}
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
            Username
            <input value={username} onChange={(event) => setUsername(event.target.value)} autoComplete="username" />
          </label>
          <label>
            Password
            <input
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              type="password"
              autoComplete="current-password"
            />
          </label>
          {error && <p className="form-error">{error}</p>}
          <button className="primary-button" disabled={loginBusy}>
            <KeyRound size={18} /> {loginBusy ? 'Login...' : 'Login'}
          </button>
          {oidc?.paperless_login_enabled && (
            <button type="button" className="secondary-button" disabled={loginBusy} onClick={() => void submitLogin('paperless')}>
              <Archive size={18} /> Login with Paperless-ngx
            </button>
          )}
        </form>
      </section>
    </main>
  );
}

function Dashboard({ setError }: { setError: (error: string | null) => void }) {
  const [counts, setCounts] = useState<Counts>(defaultCounts);
  const [stats, setStats] = useState<DashboardStats | null>(null);
  const [range, setRange] = useState<DashboardRange>('30d');
  const [busy, setBusy] = useState(false);
  const load = () =>
    api
      .dashboard(range)
      .then((data) => {
        setCounts(data.counts);
        setStats(data.stats);
      })
      .catch((err) => setError(err.message));

  useEffect(() => {
    void load();
    const timer = window.setInterval(() => {
      void load();
    }, 30000);
    return () => window.clearInterval(timer);
  }, [range]);

  const openBacklog = counts.total_documents - counts.complete;
  const completionEmpty = counts.total_documents === 0;
  const completionData = completionEmpty
    ? [{ name: 'No Documents', value: 1 }]
    : [
        { name: 'Complete', value: counts.complete },
        { name: 'Open', value: Math.max(openBacklog, 0) }
      ];
  const completionColors = completionEmpty ? ['#d8e0e6'] : ['#147f7a', '#a9782b'];
  const stageData = (stats?.stage_status.length ? stats.stage_status : defaultStageStatus).map((stage) => ({
    stage: stageLabel(stage.stage),
    Complete: stage.complete,
    Pending: stage.pending,
    Review: stage.waiting_review,
    Running: stage.running,
    Failed: stage.failed
  }));
  const jobStatusData = statusChartData(stats?.job_status.length ? stats.job_status : defaultJobStatus);
  const runStatusData = statusChartData(stats?.run_status.length ? stats.run_status : defaultRunStatus);
  const comparison = stats?.comparison;

  const metrics = [
    ['Total Documents', counts.total_documents, 'neutral', null],
    ['Complete', counts.complete, 'success', comparison?.jobs_succeeded_delta],
    ['Open Backlog', openBacklog, 'warning', comparison?.open_backlog_delta],
    ['Review Load', counts.waiting_review, 'review', null],
    ['Failed', counts.failed, 'danger', comparison?.jobs_failed_delta],
    ['Running', counts.running, 'info', null],
    ['Throughput', stats?.kpis.throughput ?? 0, 'success', comparison?.jobs_succeeded_delta],
    ['Queued Jobs', jobStatusData.find((item) => item.status === 'queued')?.count ?? 0, 'neutral', comparison?.jobs_created_delta]
  ] as const;

  return (
    <section className="page">
      <div className="dashboard-heading">
        <PageHeader title="Backlog Dashboard" />
        <div className="range-tabs" aria-label="Dashboard range">
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
      </div>
      <div className="metric-grid">
        {metrics.map(([label, value, tone, delta]) => (
          <div className={`metric ${tone}`} key={label}>
            <span>{label}</span>
            <strong>{value}</strong>
            {typeof delta === 'number' && <em className={deltaTone(delta)}>{formatDelta(delta)}</em>}
          </div>
        ))}
      </div>
      <div className="dashboard-grid">
        <ChartPanel title="Completion">
          <ResponsiveContainer width="100%" height={260}>
            <PieChart>
              <Pie data={completionData} dataKey="value" nameKey="name" innerRadius={62} outerRadius={96} paddingAngle={4}>
                {completionData.map((entry, index) => (
                  <Cell key={entry.name} fill={completionColors[index] ?? '#d8e0e6'} />
                ))}
              </Pie>
              <Tooltip />
              <Legend />
            </PieChart>
          </ResponsiveContainer>
        </ChartPanel>
        <ChartPanel title="Stage Health" wide>
          <ResponsiveContainer width="100%" height={260}>
            <BarChart data={stageData}>
              <CartesianGrid strokeDasharray="3 3" vertical={false} />
              <XAxis dataKey="stage" />
              <YAxis allowDecimals={false} />
              <Tooltip />
              <Legend />
              <Bar dataKey="Complete" stackId="stage" fill="#147f7a" />
              <Bar dataKey="Pending" stackId="stage" fill="#a9782b" />
              <Bar dataKey="Review" stackId="stage" fill="#28649b" />
              <Bar dataKey="Running" stackId="stage" fill="#5b8fb9" />
              <Bar dataKey="Failed" stackId="stage" fill="#a6403a" />
            </BarChart>
          </ResponsiveContainer>
        </ChartPanel>
        <ChartPanel title="Throughput" wide>
          <ResponsiveContainer width="100%" height={280}>
            <AreaChart data={stats?.throughput_series ?? []}>
              <CartesianGrid strokeDasharray="3 3" vertical={false} />
              <XAxis dataKey="label" />
              <YAxis allowDecimals={false} />
              <Tooltip />
              <Legend />
              <Area type="monotone" dataKey="jobs_created" name="Created" stroke="#28649b" fill="#dbe9f5" />
              <Area type="monotone" dataKey="jobs_succeeded" name="Succeeded" stroke="#147f7a" fill="#d9eeee" />
              <Area type="monotone" dataKey="jobs_failed" name="Failed" stroke="#a6403a" fill="#f5dddd" />
            </AreaChart>
          </ResponsiveContainer>
        </ChartPanel>
        <ChartPanel title="Backlog Trend" wide>
          <ResponsiveContainer width="100%" height={280}>
            <AreaChart data={stats?.backlog_series ?? []}>
              <CartesianGrid strokeDasharray="3 3" vertical={false} />
              <XAxis dataKey="label" />
              <YAxis allowDecimals={false} />
              <Tooltip />
              <Legend />
              <Area type="monotone" dataKey="open_backlog" name="Open" stroke="#a9782b" fill="#f1e5d0" />
              <Area type="monotone" dataKey="complete" name="Complete" stroke="#147f7a" fill="#d9eeee" />
              <Area type="monotone" dataKey="failed" name="Failed" stroke="#a6403a" fill="#f5dddd" />
            </AreaChart>
          </ResponsiveContainer>
        </ChartPanel>
        <ChartPanel title="Job Status">
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
        <ChartPanel title="Run Status">
          <ResponsiveContainer width="100%" height={240}>
            <BarChart data={runStatusData} layout="vertical" margin={{ left: 12 }}>
              <CartesianGrid strokeDasharray="3 3" horizontal={false} />
              <XAxis type="number" allowDecimals={false} />
              <YAxis type="category" dataKey="label" width={92} />
              <Tooltip />
              <Bar dataKey="count" fill="#147f7a" radius={[0, 4, 4, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </ChartPanel>
      </div>
      <ChartPanel title="Provider Usage, Tokens, And Latency" wide>
        <div className="table-wrap compact-table">
          <table>
            <thead>
              <tr>
                <th>Provider</th>
                <th>Model</th>
                <th>Stage</th>
                <th>Requests</th>
                <th>Avg</th>
                <th>P95</th>
                <th>Tokens</th>
                <th>Cost</th>
              </tr>
            </thead>
            <tbody>
              {(stats?.provider_usage ?? []).length === 0 && (
                <tr><td colSpan={8}>No provider usage recorded for this range.</td></tr>
              )}
              {(stats?.provider_usage ?? []).map((item) => (
                <tr key={`${item.provider}-${item.model}-${item.stage}`}>
                  <td>{item.provider}</td>
                  <td>{item.model}</td>
                  <td>{stageLabel(item.stage)}</td>
                  <td>{item.request_count}</td>
                  <td>{formatMs(item.avg_duration_ms)}</td>
                  <td>{formatMs(item.p95_duration_ms)}</td>
                  <td>{item.input_tokens + item.output_tokens}</td>
                  <td>{formatCost(item.estimated_cost_usd)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </ChartPanel>
      <div className="toolbar">
        <ActionButton icon={<RefreshCw />} label="Refresh" busy={busy} onClick={() => run(setBusy, setError, load)} />
        <ActionButton icon={<RefreshCw />} label="Sync" busy={busy} onClick={() => run(setBusy, setError, api.syncPaperless).then(load)} />
        <ActionButton icon={<FileText />} label="Queue OCR" busy={busy} onClick={() => run(setBusy, setError, api.queueOcr).then(load)} />
        <ActionButton icon={<Tags />} label="Queue Tags" busy={busy} onClick={() => run(setBusy, setError, api.queueTags).then(load)} />
        <ActionButton icon={<Play />} label="Queue Full" busy={busy} onClick={() => run(setBusy, setError, api.queueFull).then(load)} />
      </div>
    </section>
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

const defaultStageStatus = ['ocr', 'title', 'document_type', 'correspondent', 'tags', 'fields'].map((stage) => ({
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

function statusChartData(items: DashboardStatusCount[]) {
  return items.map((item) => ({
    ...item,
    label: statusLabel(item.status)
  }));
}

function stageLabel(stage: string) {
  const labels: Record<string, string> = {
    ocr: 'OCR',
    title: 'Title',
    document_type: 'Type',
    correspondent: 'Correspondent',
    tags: 'Tags',
    fields: 'Fields'
  };
  return labels[stage] ?? statusLabel(stage);
}

function statusLabel(value: string) {
  return value
    .split('_')
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ');
}

function formatDelta(value: number) {
  if (value === 0) return '0 vs previous';
  return `${value > 0 ? '+' : ''}${value} vs previous`;
}

function formatMs(value: number) {
  if (!Number.isFinite(value) || value <= 0) return '-';
  if (value >= 1000) return `${(value / 1000).toFixed(1)}s`;
  return `${Math.round(value)}ms`;
}

function formatCost(value?: number | null) {
  if (value == null) return '-';
  if (value === 0) return '$0.00';
  if (value < 0.01) return `<$0.01`;
  return `$${value.toFixed(2)}`;
}

function deltaTone(value: number) {
  if (value > 0) return 'delta up';
  if (value < 0) return 'delta down';
  return 'delta';
}

function Inventory({ setError }: { setError: (error: string | null) => void }) {
  const [items, setItems] = useState<InventoryItem[]>([]);
  const [busy, setBusy] = useState(false);
  const load = () => api.inventory().then((data) => setItems(data.items)).catch((err) => setError(err.message));

  useEffect(() => {
    void load();
  }, []);

  return (
    <section className="page">
      <PageHeader title="Document Inventory" />
      <div className="toolbar">
        <ActionButton icon={<RefreshCw />} label="Reload" busy={busy} onClick={() => run(setBusy, setError, load)} />
      </div>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>ID</th>
              <th>Title</th>
              <th>OCR</th>
              <th>Tags</th>
              <th>Title</th>
              <th>Type</th>
              <th>Run</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody>
            {items.map((item) => (
              <tr key={item.paperless_document_id}>
                <td>{item.paperless_document_id}</td>
                <td>{item.title || item.original_file_name || 'Untitled'}</td>
                <td><Status value={item.ocr_status} /></td>
                <td><Status value={item.tagging_status} /></td>
                <td><Status value={item.title_status} /></td>
                <td><Status value={item.document_type_status} /></td>
                <td>{item.current_run_status || '-'}</td>
                <td className="row-actions">
                  <button title="Trigger OCR" onClick={() => api.triggerDocument(item.paperless_document_id, ['ocr'], 'review').then(load).catch((err) => setError(err.message))}>
                    <FileText size={16} />
                  </button>
                  <button title="Trigger tagging" onClick={() => api.triggerDocument(item.paperless_document_id, ['tags'], 'review').then(load).catch((err) => setError(err.message))}>
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
    }).catch((err) => setError(err.message));

  const loadMessages = (sessionId: string) =>
    api.chatMessages(sessionId).then((data) => setMessages(data.items)).catch((err) => setError(err.message));

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
                <small>{new Date(session.updated_at).toLocaleString()}</small>
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
  const [items, setItems] = useState<ReviewItem[]>([]);
  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const load = () => api.reviews().then((data) => setItems(data.items)).catch((err) => setError(err.message));

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
        setError(`${result.failed.length} review items failed. First error: ${result.failed[0].error}`);
      }
      setSelected([]);
      await load();
    });
  };

  return (
    <section className="page">
      <PageHeader title="Review Queue" />
      <div className="toolbar">
        <button disabled={items.length === 0} onClick={() => setSelected(selected.length === items.length ? [] : items.map((item) => item.id))}>
          <ListChecks size={16} /> {selected.length === items.length ? 'Clear selection' : 'Select all'}
        </button>
        <button disabled={busy || selected.length === 0} onClick={() => void batch('approve')}>
          <Check size={16} /> Approve selected
        </button>
        <button disabled={busy || selected.length === 0} onClick={() => void batch('reject')}>
          <X size={16} /> Reject selected
        </button>
      </div>
      <div className="review-list">
        {items.map((item) => (
          <article className="review-item" key={item.id}>
            <header>
              <label className="inline">
                <input type="checkbox" checked={selected.includes(item.id)} onChange={() => toggleSelected(item.id)} />
                <strong>Document {item.paperless_document_id}</strong>
              </label>
              <span>{item.stage}</span>
            </header>
            <pre>{JSON.stringify(item.suggested_patch, null, 2)}</pre>
            <footer>
              <button title="Approve" onClick={() => api.approveReview(item.id).then(load).catch((err) => setError(err.message))}>
                <Check size={16} /> Approve
              </button>
              <button title="Reject" onClick={() => api.rejectReview(item.id).then(load).catch((err) => setError(err.message))}>
                <X size={16} /> Reject
              </button>
            </footer>
          </article>
        ))}
      </div>
    </section>
  );
}

function SettingsPage({ setError }: { setError: (error: string | null) => void }) {
  const [settings, setSettings] = useState<RuntimeSettings | null>(null);
  const [token, setToken] = useState('');
  const [providerSecrets, setProviderSecrets] = useState<Record<string, string>>({});
  const [ollamaModels, setOllamaModels] = useState<Record<string, OllamaModelLoadState>>({});
  const [paperlessTest, setPaperlessTest] = useState<ConnectionTestState | null>(null);
  const [providerTest, setProviderTest] = useState<ConnectionTestState | null>(null);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);

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
            error: 'Ollama-Modelle konnten nicht geladen werden. Prüfe, ob Ollama erreichbar ist, und lade erneut.'
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
        refreshInstalledOllamaModels(nextSettings);
      })
      .catch((err) => setError(err.message));
  }, [setError]);

  if (!settings) return <section className="page"><PageHeader title="Settings" /></section>;

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
    setPaperlessTest({
      status: 'running',
      title: 'Paperless-Test läuft',
      description: 'Archivist prüft gerade die gespeicherte Paperless REST-Verbindung.',
      hints: ['Der Test nutzt die gespeicherte Base URL und den gespeicherten API-Token.']
    });
    api
      .testPaperless()
      .then((data) => {
        setPaperlessTest(data.ok ? paperlessTestSuccess() : paperlessTestFailure(data.error));
      })
      .catch((err) => {
        setPaperlessTest(paperlessTestFailure(err.message));
      });
  };
  const runProviderTest = () => {
    setProviderTest({
      status: 'running',
      title: 'Provider-Test läuft',
      description: `Archivist prüft gerade Provider '${selectedDefaultProvider.name}' mit dem gespeicherten Textmodell.`,
      hints: ['Der Test nutzt gespeicherte Provider-Settings und gespeicherte API-Key-Referenzen.']
    });
    api
      .testProvider()
      .then((data) => {
        setProviderTest(data.ok ? providerTestSuccess(selectedDefaultProvider) : providerTestFailure(selectedDefaultProvider, data.error));
      })
      .catch((err) => {
        setProviderTest(providerTestFailure(selectedDefaultProvider, err.message));
      });
  };

  return (
    <section className="page">
      <PageHeader title="Runtime Settings" />
      <div className="settings-grid">
        <fieldset>
          <legend>Paperless</legend>
          <label>
            Base URL
            <input value={settings.paperless.base_url} onChange={(event) => update((s) => ({ ...s, paperless: { ...s.paperless, base_url: event.target.value } }))} />
          </label>
          <label>
            API token
            <input value={token} type="password" onChange={(event) => setToken(event.target.value)} placeholder={settings.paperless.token_secret_id ? 'Configured' : ''} />
          </label>
          <label className="inline">
            <input
              type="checkbox"
              checked={settings.paperless.login_bridge_enabled}
              onChange={(event) => update((s) => ({ ...s, paperless: { ...s.paperless, login_bridge_enabled: event.target.checked } }))}
            />
            Allow Paperless-ngx login bridge
          </label>
          <button title="Test Paperless" disabled={paperlessTest?.status === 'running'} onClick={runPaperlessTest}>
            <Database size={16} /> {paperlessTest?.status === 'running' ? 'Test läuft...' : 'Test'}
          </button>
          <ConnectionTestFeedback state={paperlessTest} />
        </fieldset>
        <fieldset>
          <legend>AI Defaults</legend>
          <label>
            Default provider
            <select value={settings.ai.default_provider} onChange={(event) => selectDefaultProvider(event.target.value)}>
              {settings.ai.providers.map((provider) => (
                <option key={provider.name} value={provider.name}>{provider.name}</option>
              ))}
            </select>
          </label>
          <label>
            Legacy Ollama URL
            <input value={settings.ai.ollama_base_url} onChange={(event) => update((s) => ({ ...s, ai: { ...s.ai, ollama_base_url: event.target.value } }))} />
          </label>
          <div className="settings-field">
            Fallback text model
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
            Fallback vision model
            <ProviderModelSelect
              capability="vision"
              provider={selectedDefaultProvider}
              value={settings.ai.default_vision_model}
              ollamaState={ollamaModels[selectedDefaultProvider.name]}
              onChange={(value) => update((s) => ({ ...s, ai: { ...s.ai, default_vision_model: value } }))}
              onRefresh={() => loadOllamaModels(selectedDefaultProvider.name)}
            />
          </div>
          <button title="Test provider" disabled={providerTest?.status === 'running'} onClick={runProviderTest}>
            <Activity size={16} /> {providerTest?.status === 'running' ? 'Test läuft...' : 'Test'}
          </button>
          <ConnectionTestFeedback state={providerTest} />
        </fieldset>
        <fieldset>
          <legend>Workflow</legend>
          <label>
            Mode
            <select value={settings.workflow.mode} onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, mode: event.target.value as RuntimeSettings['workflow']['mode'] } }))}>
              <option value="review">review</option>
              <option value="autopilot">autopilot</option>
            </select>
          </label>
          <label>
            OCR pages
            <input type="number" min="1" max="20" value={settings.ocr.page_limit} onChange={(event) => update((s) => ({ ...s, ocr: { ...s.ocr, page_limit: Number(event.target.value) } }))} />
          </label>
          <label>
            Max tags
            <input type="number" min="1" max="20" value={settings.tagging.max_tags} onChange={(event) => update((s) => ({ ...s, tagging: { ...s.tagging, max_tags: Number(event.target.value) } }))} />
          </label>
          <label>
            Tag confidence
            <input type="number" min="0" max="1" step="0.05" value={settings.tagging.confidence_threshold} onChange={(event) => update((s) => ({ ...s, tagging: { ...s.tagging, confidence_threshold: Number(event.target.value) } }))} />
          </label>
          <label>
            Max fields
            <input type="number" min="1" max="50" value={settings.fields.max_fields} onChange={(event) => update((s) => ({ ...s, fields: { ...s.fields, max_fields: Number(event.target.value) } }))} />
          </label>
          <label>
            Field confidence
            <input type="number" min="0" max="1" step="0.05" value={settings.fields.confidence_threshold} onChange={(event) => update((s) => ({ ...s, fields: { ...s.fields, confidence_threshold: Number(event.target.value) } }))} />
          </label>
          <label className="inline">
            <input type="checkbox" checked={settings.tagging.allow_new_tags} onChange={(event) => update((s) => ({ ...s, tagging: { ...s.tagging, allow_new_tags: event.target.checked } }))} />
            Allow new tags
          </label>
          <label>
            Include tags
            <input
              value={settings.workflow.rules.include_tags.join(', ')}
              onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, rules: { ...s.workflow.rules, include_tags: splitTags(event.target.value) } } }))}
              placeholder="optional, comma separated"
            />
          </label>
          <label>
            Exclude tags
            <input
              value={settings.workflow.rules.exclude_tags.join(', ')}
              onChange={(event) => update((s) => ({ ...s, workflow: { ...s.workflow, rules: { ...s.workflow.rules, exclude_tags: splitTags(event.target.value) } } }))}
              placeholder="optional, comma separated"
            />
          </label>
        </fieldset>
      </div>
      <PageHeader title="Model Providers" />
      <div className="provider-list">
        {settings.ai.providers.map((provider, index) => (
          <fieldset key={`${provider.name}-${index}`}>
            <legend>{provider.name || 'Provider'}</legend>
            <label>
              Name
              <input value={provider.name} onChange={(event) => updateProvider(index, { name: event.target.value })} />
            </label>
            <label>
              Kind
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
              Base URL
              <input value={provider.base_url} onChange={(event) => updateProvider(index, { base_url: event.target.value })} />
            </label>
            <label>
              Input $/1M tokens
              <input
                type="number"
                min="0"
                step="0.01"
                value={provider.cost_per_1m_input_tokens_usd ?? ''}
                onChange={(event) => updateProvider(index, { cost_per_1m_input_tokens_usd: optionalNumber(event.target.value) })}
              />
            </label>
            <label>
              Output $/1M tokens
              <input
                type="number"
                min="0"
                step="0.01"
                value={provider.cost_per_1m_output_tokens_usd ?? ''}
                onChange={(event) => updateProvider(index, { cost_per_1m_output_tokens_usd: optionalNumber(event.target.value) })}
              />
            </label>
            <div className="settings-field">
              Text model
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
              Vision model
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
              API key
              <input
                type="password"
                value={providerSecrets[provider.name] ?? ''}
                placeholder={provider.secret_id ? 'Configured' : ''}
                onChange={(event) => setProviderSecrets((current) => ({ ...current, [provider.name]: event.target.value }))}
              />
            </label>
            <label className="inline">
              <input type="checkbox" checked={provider.enabled} onChange={(event) => updateProvider(index, { enabled: event.target.checked })} />
              Enabled
            </label>
          </fieldset>
        ))}
      </div>
      <div className="toolbar">
        <button title="Add provider" onClick={addProvider}>
          <UserPlus size={16} /> Add Provider
        </button>
        <ActionButton
          icon={<Save />}
          label="Save"
          busy={busy}
          onClick={() => run(setBusy, setError, () => api.saveSettings(settings, token, providerSecrets).then((saved) => {
            const nextSettings = withModelDefaults(saved);
            setSettings(nextSettings);
            setToken('');
            setProviderSecrets({});
            setResult('Saved');
            refreshInstalledOllamaModels(nextSettings);
          }))}
        />
        {result && <span className="result">{result}</span>}
      </div>
    </section>
  );
}

function ConnectionTestFeedback({ state }: { state: ConnectionTestState | null }) {
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
          <summary>Technische Details</summary>
          <code>{state.details}</code>
        </details>
      )}
    </div>
  );
}

function paperlessTestSuccess(): ConnectionTestState {
  return {
    status: 'success',
    title: 'Paperless-Verbindung funktioniert',
    description: 'Archivist konnte die Paperless REST API mit den gespeicherten Einstellungen erreichen.',
    hints: ['Du kannst jetzt die Inventar-Synchronisierung starten oder Jobs queueen.']
  };
}

function paperlessTestFailure(error?: string): ConnectionTestState {
  const details = sanitizeConnectionDetail(error || 'Paperless test failed');
  return {
    status: 'error',
    title: 'Paperless-Verbindung fehlgeschlagen',
    description: paperlessProblemDescription(details),
    hints: paperlessProblemHints(details),
    details
  };
}

function providerTestSuccess(provider: ModelProviderDescriptor): ConnectionTestState {
  const providerName = provider.name || provider.kind;
  const isOllama = provider.kind === 'ollama';
  return {
    status: 'success',
    title: 'Provider-Verbindung funktioniert',
    description: isOllama
      ? `Archivist konnte '${providerName}' erreichen und das konfigurierte Textmodell prüfen.`
      : `Archivist konnte '${providerName}' erreichen und eine kurze Testanfrage ausführen.`,
    hints: isOllama
      ? ['Wenn du das OCR/Vision-Modell separat geändert hast, prüfe zusätzlich die installierte Ollama-Modellliste.']
      : ['Der Provider ist einsatzbereit. Prüfe Review-Ergebnisse trotzdem zuerst im Review-Modus.']
  };
}

function providerTestFailure(provider: ModelProviderDescriptor, error?: string): ConnectionTestState {
  const details = sanitizeConnectionDetail(error || 'Provider test failed');
  return {
    status: 'error',
    title: 'Provider-Verbindung fehlgeschlagen',
    description: providerProblemDescription(provider, details),
    hints: providerProblemHints(provider, details),
    details
  };
}

function paperlessProblemDescription(details: string) {
  const lower = details.toLowerCase();
  if (lower.includes('api token') || lower.includes('secret') || lower.includes('token')) {
    return 'Archivist konnte keinen gültigen Paperless API-Token verwenden.';
  }
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return 'Paperless hat die Anfrage abgelehnt. Der API-Token ist wahrscheinlich ungültig oder hat zu wenig Rechte.';
  }
  if (lower.includes('404')) {
    return 'Die Paperless API wurde unter der konfigurierten Base URL nicht gefunden.';
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return 'Paperless hat nicht rechtzeitig geantwortet.';
  }
  if (lower.includes('connect') || lower.includes('dns') || lower.includes('resolve') || lower.includes('refused')) {
    return 'Archivist konnte Paperless über das Netzwerk nicht erreichen.';
  }
  return 'Der Paperless-Test ist fehlgeschlagen. Die technischen Details enthalten die Rückmeldung des Backends.';
}

function paperlessProblemHints(details: string) {
  const lower = details.toLowerCase();
  if (lower.includes('api token') || lower.includes('secret') || lower.includes('token') || lower.includes('401') || lower.includes('403')) {
    return [
      'Erzeuge in Paperless einen neuen API-Token für einen berechtigten User.',
      'Trage den Token in Settings ein und speichere die Settings vor dem erneuten Test.',
      'Prüfe, ob der User Dokumente lesen und Metadaten aktualisieren darf.'
    ];
  }
  if (lower.includes('404')) {
    return [
      'Prüfe die Paperless Base URL. Sie muss auf die Paperless-Instanz zeigen, nicht auf eine Unterseite.',
      'Teste die URL aus Sicht des Archivist-Containers oder API-Prozesses.',
      'Beispiel in Docker Compose: http://paperless:8000'
    ];
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return [
      'Prüfe, ob Paperless läuft und nicht überlastet ist.',
      'Prüfe Netzwerk, DNS und Reverse Proxy zwischen Archivist und Paperless.',
      'Erhöhe den Paperless Timeout in den Settings, wenn die Instanz langsam antwortet.'
    ];
  }
  return [
    'Prüfe, ob die Paperless Base URL aus Sicht des Archivist-Backends erreichbar ist.',
    'Prüfe Container-Netzwerk, DNS-Namen, Protokoll http/https und Proxy-Konfiguration.',
    'Speichere geänderte Settings vor dem nächsten Test.'
  ];
}

function providerProblemDescription(provider: ModelProviderDescriptor, details: string) {
  const lower = details.toLowerCase();
  if (provider.kind === 'ollama') {
    if (lower.includes('model') && lower.includes('not listed')) {
      return 'Ollama ist erreichbar, aber das konfigurierte Textmodell ist nicht installiert.';
    }
    if (lower.includes('timeout') || lower.includes('timed out')) {
      return 'Ollama hat nicht rechtzeitig geantwortet.';
    }
    if (lower.includes('connect') || lower.includes('dns') || lower.includes('resolve') || lower.includes('refused')) {
      return 'Archivist konnte den Ollama-Service nicht erreichen.';
    }
    return 'Der Ollama-Test ist fehlgeschlagen.';
  }
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return 'Der Provider hat die Anfrage abgelehnt. API-Key, Berechtigungen oder Modellzugriff stimmen wahrscheinlich nicht.';
  }
  if (lower.includes('model')) {
    return 'Der Provider konnte das konfigurierte Modell nicht verwenden.';
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return 'Der Provider hat nicht rechtzeitig geantwortet.';
  }
  return 'Der Provider-Test ist fehlgeschlagen.';
}

function providerProblemHints(provider: ModelProviderDescriptor, details: string) {
  const lower = details.toLowerCase();
  if (provider.kind === 'ollama') {
    if (lower.includes('model') && lower.includes('not listed')) {
      return [
        'Installiere das Modell mit ollama pull oder wähle ein installiertes Modell aus dem Dropdown.',
        'Klicke danach auf Refresh in der Modellliste und speichere die Settings.',
        'Prüfe, ob Textmodell und Vision/OCR-Modell getrennt korrekt gesetzt sind.'
      ];
    }
    return [
      'Prüfe, ob der Ollama-Service läuft.',
      'Prüfe die Provider Base URL aus Sicht des Archivist-Backends, z.B. http://ollama:11434 in Docker Compose.',
      'Prüfe Firewall, DNS und ob der Ollama-Endpunkt /api/tags erreichbar ist.'
    ];
  }
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return [
      'Prüfe den API-Key und speichere ihn erneut in Settings.',
      'Prüfe beim Anbieter, ob der Key Zugriff auf das ausgewählte Modell hat.',
      'Prüfe, ob die Provider Base URL zum Anbieter passt.'
    ];
  }
  if (lower.includes('model')) {
    return [
      'Wähle ein vom Provider unterstütztes Textmodell aus dem Dropdown.',
      'Prüfe, ob das Modell für deinen API-Key freigeschaltet ist.',
      'Speichere die Settings und starte den Test erneut.'
    ];
  }
  return [
    'Prüfe Provider Base URL, API-Key und Netzwerkverbindung.',
    'Prüfe, ob der Provider erreichbar ist und keine Rate Limits greifen.',
    'Speichere geänderte Settings vor dem nächsten Test.'
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

function splitTags(value: string) {
  return value
    .split(',')
    .map((tag) => tag.trim())
    .filter(Boolean);
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
  const usesInstalledModels = provider.kind === 'ollama' && !isOllamaCloudProvider(provider);
  const hasReliableInstalledList = Boolean(ollamaState?.loaded && !ollamaState.error);
  const options = usesInstalledModels
    ? installedOllamaModelOptions(
      ollamaState?.models ?? [],
      value,
      hasReliableInstalledList,
      ollamaSelectPlaceholder(ollamaState)
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
        <select value={value} onChange={(event) => onChange(event.target.value)}>
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
            title="Reload installed Ollama models"
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
  placeholder: string
) {
  const options = models.map((model) => ({
    value: model.name,
    label: installedOllamaModelLabel(model)
  }));
  const hasCurrent = models.some((model) => model.name === current);
  if (current && !loaded && !hasCurrent) {
    return [{ value: current, label: current }, ...options];
  }
  if (current && loaded && !hasCurrent) {
    return [{ value: current, label: `⚠ ${current} · nicht installiert` }, ...options];
  }
  if (!current && loaded && options.length === 0) {
    return [{ value: '', label: 'Keine installierten Modelle' }];
  }
  if (!current && !loaded) {
    return [{ value: '', label: placeholder }];
  }
  return options;
}

function ollamaSelectPlaceholder(state?: OllamaModelLoadState) {
  if (state?.error) return 'Keine Modellliste verfügbar';
  if (state?.loading) return 'Installierte Modelle werden geladen';
  return 'Installierte Modelle laden';
}

function installedOllamaModelLabel(model: OllamaInstalledModel) {
  return [
    model.name,
    model.parameter_size || 'unbekannte Parameter',
    model.quantization_level || 'unbekannte Quantisierung',
    formatModelSize(model.size_bytes)
  ].join(' · ');
}

function formatModelSize(sizeBytes?: number | null) {
  if (!sizeBytes || sizeBytes <= 0) return 'unbekannte Größe';
  return `${(sizeBytes / 1024 ** 3).toFixed(sizeBytes >= 10 * 1024 ** 3 ? 1 : 2)} GB`;
}

function OllamaModelStatus({
  state,
  currentIsMissing
}: {
  state?: OllamaModelLoadState;
  currentIsMissing: boolean;
}) {
  if (state?.loading) {
    return <p className="field-hint">Installierte Ollama-Modelle werden geladen...</p>;
  }
  if (state?.error) {
    return <p className="field-hint error">{state.error}</p>;
  }
  if (state?.loaded && state.models.length === 0) {
    return <p className="field-hint warning">Keine installierten Ollama-Modelle gefunden.</p>;
  }
  if (currentIsMissing) {
    return <p className="field-hint warning">Gespeichertes Modell ist aktuell nicht installiert.</p>;
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
  const [items, setItems] = useState<Prompt[]>([]);
  const [stage, setStage] = useState<Stage>('tags');
  const [name, setName] = useState('default');
  const [content, setContent] = useState('');
  const [activate, setActivate] = useState(true);
  const [sampleText, setSampleText] = useState('');
  const [sampleDocumentId, setSampleDocumentId] = useState('');
  const [testResult, setTestResult] = useState<PromptTestResponse | null>(null);
  const [testing, setTesting] = useState(false);
  const stages: Stage[] = ['ocr', 'tags', 'title', 'correspondent', 'document_type', 'fields'];
  const load = () => api.prompts().then((data) => setItems(data.items)).catch((err) => setError(err.message));

  useEffect(() => {
    void load();
  }, []);

  return (
    <section className="page">
      <PageHeader title="Prompts" />
      <form className="prompt-form" onSubmit={(event) => {
        event.preventDefault();
        api.createPrompt({ stage, name, content, activate }).then(() => {
          setContent('');
          load();
        }).catch((err) => setError(err.message));
      }}>
        <label>
          Stage
          <select value={stage} onChange={(event) => setStage(event.target.value as Stage)}>
            {stages.map((entry) => <option key={entry} value={entry}>{entry}</option>)}
          </select>
        </label>
        <label>
          Name
          <input value={name} onChange={(event) => setName(event.target.value)} />
        </label>
        <label className="wide">
          Prompt content
          <textarea value={content} onChange={(event) => setContent(event.target.value)} required />
        </label>
        <label>
          Test document ID
          <input value={sampleDocumentId} onChange={(event) => setSampleDocumentId(event.target.value)} placeholder="optional" />
        </label>
        <label className="wide">
          Test sample text
          <textarea value={sampleText} onChange={(event) => setSampleText(event.target.value)} placeholder="Optional; overrides document ID for prompt tests." />
        </label>
        <label className="inline">
          <input type="checkbox" checked={activate} onChange={(event) => setActivate(event.target.checked)} />
          Activate
        </label>
        <button className="primary-button"><Save size={16} /> Save Prompt</button>
        <button
          type="button"
          disabled={testing || !content.trim()}
          onClick={() => run(setTesting, setError, async () => {
            const documentId = sampleDocumentId.trim() ? Number(sampleDocumentId) : null;
            const result = await api.testPrompt({
              stage,
              content,
              sample_text: sampleText.trim() || undefined,
              paperless_document_id: documentId && Number.isFinite(documentId) ? documentId : null
            });
            setTestResult(result);
          })}
        >
          <Play size={16} /> {testing ? 'Testing...' : 'Test Prompt'}
        </button>
      </form>
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
      <div className="table-wrap">
        <table>
          <thead><tr><th>Stage</th><th>Name</th><th>Version</th><th>Status</th><th>Created</th><th>Action</th></tr></thead>
          <tbody>
            {items.map((prompt) => (
              <tr key={prompt.id}>
                <td>{prompt.stage}</td>
                <td>{prompt.name}</td>
                <td>{prompt.version}</td>
                <td><Status value={prompt.active ? 'active' : 'inactive'} /></td>
                <td>{new Date(prompt.created_at).toLocaleString()}</td>
                <td>
                  {!prompt.active && <button title="Activate prompt" onClick={() => api.activatePrompt(prompt.id).then(load).catch((err) => setError(err.message))}><Check size={16} /> Activate</button>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function Audit({ setError }: { setError: (error: string | null) => void }) {
  const [items, setItems] = useState<AuditEvent[]>([]);
  useEffect(() => {
    api.audit().then((data) => setItems(data.items)).catch((err) => setError(err.message));
  }, [setError]);
  return (
    <section className="page">
      <PageHeader title="Audit Log" />
      <div className="toolbar">
        <a className="button-link" href="/api/audit/export.csv">
          <FileText size={16} /> Export CSV
        </a>
      </div>
      <div className="table-wrap">
        <table>
          <thead>
            <tr><th>Time</th><th>Event</th><th>Actor</th><th>Document</th><th>Outcome</th></tr>
          </thead>
          <tbody>
            {items.map((item) => (
              <tr key={item.id}>
                <td>{new Date(item.created_at).toLocaleString()}</td>
                <td>{item.event_type}</td>
                <td>{item.actor_type}</td>
                <td>{item.paperless_document_id || '-'}</td>
                <td><Status value={item.outcome} /></td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function Users({ setError }: { setError: (error: string | null) => void }) {
  const [users, setUsers] = useState<UserItem[]>([]);
  const [sessions, setSessions] = useState<SessionItem[]>([]);
  const [tokens, setTokens] = useState<ApiToken[]>([]);
  const [newToken, setNewToken] = useState<string | null>(null);
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [role, setRole] = useState<Role>('viewer');
  const [tokenName, setTokenName] = useState('');
  const [resetPasswords, setResetPasswords] = useState<Record<string, string>>({});
  const load = () =>
    Promise.all([api.users(), api.sessions(), api.apiTokens()])
      .then(([userData, sessionData, tokenData]) => {
        setUsers(userData.items);
        setSessions(sessionData.items);
        setTokens(tokenData.items);
      })
      .catch((err) => setError(err.message));

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
        }).catch((err) => setError(err.message));
      }}>
        <input value={username} onChange={(event) => setUsername(event.target.value)} placeholder="username" />
        <input value={password} onChange={(event) => setPassword(event.target.value)} placeholder="password" type="password" />
        <select value={role} onChange={(event) => setRole(event.target.value as Role)}>
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
                    onChange={(event) => api.updateUserRoles(user.id, [event.target.value as Role]).then(load).catch((err) => setError(err.message))}
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
                  />
                  <button
                    title="Reset password"
                    disabled={!resetPasswords[user.id]}
                    onClick={() => api.resetPassword(user.id, resetPasswords[user.id] ?? '').then(() => {
                      setResetPasswords((current) => ({ ...current, [user.id]: '' }));
                      load();
                    }).catch((err) => setError(err.message))}
                  >
                    <RotateCcw size={16} />
                  </button>
                </td>
                <td>
                  {user.enabled ? (
                    <button title="Disable user" onClick={() => api.disableUser(user.id).then(load).catch((err) => setError(err.message))}><Power size={16} /> Disable</button>
                  ) : (
                    <button title="Enable user" onClick={() => api.enableUser(user.id).then(load).catch((err) => setError(err.message))}><Power size={16} /> Enable</button>
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
                <td>{new Date(session.created_at).toLocaleString()}</td>
                <td>{session.last_seen_at ? new Date(session.last_seen_at).toLocaleString() : '-'}</td>
                <td>{new Date(session.expires_at).toLocaleString()}</td>
                <td>{session.revoked_at ? 'revoked' : 'active'}</td>
                <td>
                  {!session.revoked_at && <button title="Revoke session" onClick={() => api.revokeSession(session.id).then(load).catch((err) => setError(err.message))}><X size={16} /></button>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <PageHeader title="API Tokens" />
      <form className="compact-form" onSubmit={(event) => {
        event.preventDefault();
        api.createApiToken({ name: tokenName, scopes: ['runs:read', 'inventory:read'] }).then((created) => {
          setNewToken(created.token);
          setTokenName('');
          load();
        }).catch((err) => setError(err.message));
      }}>
        <input value={tokenName} onChange={(event) => setTokenName(event.target.value)} placeholder="token name" />
        <button><KeyRound size={16} /> Create Token</button>
      </form>
      {newToken && <pre className="token-once">{newToken}</pre>}
      <div className="table-wrap">
        <table>
          <thead><tr><th>Name</th><th>Scopes</th><th>Last Used</th><th>Status</th><th>Action</th></tr></thead>
          <tbody>
            {tokens.map((token) => (
              <tr key={token.id}>
                <td>{token.name}</td>
                <td>{token.scopes.join(', ')}</td>
                <td>{token.last_used_at ? new Date(token.last_used_at).toLocaleString() : '-'}</td>
                <td>{token.revoked_at ? 'revoked' : 'active'}</td>
                <td>
                  {!token.revoked_at && <button title="Revoke token" onClick={() => api.revokeApiToken(token.id).then(load).catch((err) => setError(err.message))}><X size={16} /></button>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function NavButton({ icon, label, active, onClick }: { icon: ReactNode; label: string; active: boolean; onClick: () => void }) {
  return <button className={active ? 'active' : ''} onClick={onClick}>{icon}{label}</button>;
}

function PageHeader({ title }: { title: string }) {
  return <header className="page-header"><h2>{title}</h2></header>;
}

function Status({ value }: { value: string }) {
  const tone = useMemo(() => {
    if (['succeeded', 'success', 'complete'].includes(value)) return 'success';
    if (['failed', 'error'].includes(value)) return 'danger';
    if (['running', 'queued', 'applying'].includes(value)) return 'info';
    if (['waiting_review', 'review'].includes(value)) return 'review';
    return 'neutral';
  }, [value]);
  return <span className={`status ${tone}`}>{value}</span>;
}

function ActionButton({ icon, label, busy, onClick }: { icon: ReactNode; label: string; busy: boolean; onClick: () => void | Promise<void> }) {
  return <button className="primary-button" title={label} disabled={busy} onClick={onClick}>{icon}{label}</button>;
}

async function run(setBusy: (value: boolean) => void, setError: (value: string | null) => void, action: () => Promise<unknown> | unknown) {
  setBusy(true);
  setError(null);
  try {
    await action();
  } catch (err) {
    setError(err instanceof Error ? err.message : 'Request failed');
  } finally {
    setBusy(false);
  }
}
