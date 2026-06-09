import { Suspense, lazy, useEffect, useState, type ReactNode } from 'react';
import {
  Activity,
  Archive,
  BarChart3,
  Bug,
  ClipboardList,
  KeyRound,
  ListChecks,
  LogOut,
  MessageSquare,
  Settings,
  Shield,
  UserPlus
} from 'lucide-react';
import { api, Me, OidcConfig, setUnauthorizedHandler } from './api/client';
import { buildInfo, buildInfoLabel } from './buildInfo';
import { useI18n } from './i18n/I18nProvider';
import { ErrorBoundary } from './lib/ErrorBoundary';
import { Banner, PageHeader, localizedErrorMessage } from './lib/ui';
import { LanguageSelector } from './lib/LanguageSelector';

// Dashboard pulls in Recharts; keep it (and the other tab pages) out of the
// critical shell/login chunk by loading them lazily on first navigation.
const Dashboard = lazy(() => import('./dashboard/Dashboard').then((mod) => ({ default: mod.Dashboard })));
const Statistics = lazy(() => import('./statistics/Statistics').then((mod) => ({ default: mod.Statistics })));
const Inventory = lazy(() => import('./inventory/Inventory').then((mod) => ({ default: mod.Inventory })));
const Reviews = lazy(() => import('./reviews/Reviews').then((mod) => ({ default: mod.Reviews })));
const SettingsPage = lazy(() => import('./settings/SettingsPage').then((mod) => ({ default: mod.SettingsPage })));
const Prompts = lazy(() => import('./prompts/Prompts').then((mod) => ({ default: mod.Prompts })));
const Audit = lazy(() => import('./audit/Audit').then((mod) => ({ default: mod.Audit })));
const Users = lazy(() => import('./users/Users').then((mod) => ({ default: mod.Users })));
const DocumentChat = lazy(() => import('./chat/DocumentChat').then((mod) => ({ default: mod.DocumentChat })));
const DebugConsole = lazy(() => import('./debug/DebugConsole').then((mod) => ({ default: mod.DebugConsole })));

type Tab = 'dashboard' | 'statistics' | 'inventory' | 'chat' | 'reviews' | 'settings' | 'prompts' | 'audit' | 'users' | 'debug';

export function App() {
  const { t } = useI18n();
  const [me, setMe] = useState<Me | null>(null);
  const [tab, setTab] = useState<Tab>('dashboard');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);
  const [debugConsoleEnabled, setDebugConsoleEnabled] = useState(false);

  useEffect(() => {
    // When any request sees a 401 (expired session), drop back to the login
    // screen; this also unmounts the pollers so they stop spamming errors.
    setUnauthorizedHandler(() => {
      setMe(null);
      setError(null);
    });
    return () => setUnauthorizedHandler(null);
  }, []);

  useEffect(() => {
    api
      .me()
      .then(setMe)
      .catch(() => setMe(null))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    if (!me) return;
    // Pull the UI toggle independently of the rest of the boot flow — it only
    // controls Debug-tab visibility and we don't want a slow /api/settings to
    // delay the rest of the shell.
    let cancelled = false;
    api
      .settings()
      .then((settings) => {
        if (!cancelled) setDebugConsoleEnabled(Boolean(settings.ui?.debug_console_enabled));
      })
      .catch(() => {
        if (!cancelled) setDebugConsoleEnabled(false);
      });
    return () => {
      cancelled = true;
    };
  }, [me]);

  if (loading) return <div className="boot">{t('app.loading')}</div>;
  if (!me) return <Login onLogin={setMe} />;

  const canReadDashboard = me.permissions.read_dashboard;
  const canUseChat = me.roles.some((role) => role === 'admin' || role === 'reviewer' || role === 'operator');
  const canManageSettings = me.roles.some((role) => role === 'admin');
  const canReadSettings = me.permissions.read_settings;
  const canReadAudit = me.permissions.read_audit;
  const canManageUsers = me.permissions.manage_users;

  // Switch tabs and drop any stale global error so a banner from one tab does
  // not bleed into the next one.
  const selectTab = (next: Tab) => {
    setError(null);
    setSuccess(null);
    setTab(next);
  };

  const lazyFallback = (
    <section className="page">
      <PageHeader title={t('app.loading')} />
    </section>
  );

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
          {/* Fixed-order, labelled groups so nav positions never shift by role (#235). */}
          <div className="nav-group">
            <span className="nav-group-label">{t('nav.group.operations')}</span>
            <NavButton icon={<Activity />} label={t('nav.dashboard')} active={tab === 'dashboard'} onClick={() => selectTab('dashboard')} />
            {canReadDashboard && <NavButton icon={<BarChart3 />} label={t('nav.statistics')} active={tab === 'statistics'} onClick={() => selectTab('statistics')} />}
            <NavButton icon={<Archive />} label={t('nav.inventory')} active={tab === 'inventory'} onClick={() => selectTab('inventory')} />
            <NavButton icon={<ListChecks />} label={t('nav.review')} active={tab === 'reviews'} onClick={() => selectTab('reviews')} />
            {canUseChat && <NavButton icon={<MessageSquare />} label={t('nav.chat')} active={tab === 'chat'} onClick={() => selectTab('chat')} />}
          </div>
          {(canReadSettings || canManageUsers) && (
            <div className="nav-group">
              <span className="nav-group-label">{t('nav.group.configuration')}</span>
              {canReadSettings && <NavButton icon={<Settings />} label={t('nav.settings')} active={tab === 'settings'} onClick={() => selectTab('settings')} />}
              {canReadSettings && <NavButton icon={<ClipboardList />} label={t('nav.prompts')} active={tab === 'prompts'} onClick={() => selectTab('prompts')} />}
              {canManageUsers && <NavButton icon={<UserPlus />} label={t('nav.users')} active={tab === 'users'} onClick={() => selectTab('users')} />}
            </div>
          )}
          {(canReadAudit || debugConsoleEnabled) && (
            <div className="nav-group">
              <span className="nav-group-label">{t('nav.group.system')}</span>
              {canReadAudit && <NavButton icon={<Shield />} label={t('nav.audit')} active={tab === 'audit'} onClick={() => selectTab('audit')} />}
              {debugConsoleEnabled && canReadAudit && <NavButton icon={<Bug />} label={t('nav.debug')} active={tab === 'debug'} onClick={() => selectTab('debug')} />}
            </div>
          )}
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
            // Clear the session client-side regardless of the request outcome:
            // cookie invalidation is server-side, and a failed logout call
            // shouldn't strand the user in a logged-in UI. (#272)
            try {
              await api.logout();
            } finally {
              setMe(null);
            }
          }}
        >
          <LogOut size={18} /> {t('nav.logout')}
        </button>
      </aside>

      <main className="workspace">
        {error && <Banner tone="error" message={error} onDismiss={() => setError(null)} />}
        {success && <Banner tone="success" message={success} onDismiss={() => setSuccess(null)} />}
        {tab === 'dashboard' && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
            <Dashboard
              setError={setError}
              setSuccess={setSuccess}
              canManageSettings={canManageSettings}
              permissions={me.permissions}
              onNavigate={(nextTab, search) => {
                // Cross-tab navigation. Push the optional query-string into
                // window.history before switching tabs so the destination
                // component (e.g. Inventory) reads its filter state from
                // window.location.search on mount. If no search is provided,
                // wipe any stale query string so the destination tab starts
                // clean.
                const nextSearch = search ?? '';
                window.history.replaceState(
                  null,
                  '',
                  `${window.location.pathname}${nextSearch}${window.location.hash}`
                );
                if (
                  nextTab === 'dashboard' || nextTab === 'statistics' ||
                  nextTab === 'inventory' ||
                  nextTab === 'chat' || nextTab === 'reviews' ||
                  nextTab === 'settings' || nextTab === 'prompts' ||
                  nextTab === 'audit' || nextTab === 'users' ||
                  nextTab === 'debug'
                ) {
                  setTab(nextTab);
                }
              }}
            />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'statistics' && canReadDashboard && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <Statistics setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'inventory' && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <Inventory setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'chat' && canUseChat && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <DocumentChat setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'reviews' && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <Reviews setError={setError} setSuccess={setSuccess} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'settings' && canReadSettings && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <SettingsPage setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'prompts' && canReadSettings && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <Prompts setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'audit' && canReadAudit && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <Audit setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'users' && canManageUsers && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <Users setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'debug' && debugConsoleEnabled && canReadAudit && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <DebugConsole setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
      </main>
    </div>
    </ErrorBoundary>
  );
}

function Login({ onLogin }: { onLogin: (me: Me) => void }) {
  const { t } = useI18n();
  const [username, setUsername] = useState('');
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

function NavButton({ icon, label, active, onClick }: { icon: ReactNode; label: string; active: boolean; onClick: () => void }) {
  return <button className={active ? 'active' : ''} onClick={onClick}>{icon}{label}</button>;
}
