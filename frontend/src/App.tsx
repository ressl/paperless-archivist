import { Suspense, lazy, useEffect, useState, type ReactNode } from 'react';
import {
  Activity,
  Archive,
  Bug,
  ClipboardList,
  KeyRound,
  ListChecks,
  LogOut,
  MessageSquare,
  Settings,
  Shield,
  UserPlus,
  X
} from 'lucide-react';
import { api, Me, OidcConfig } from './api/client';
import { buildInfo, buildInfoLabel } from './buildInfo';
import { useI18n } from './i18n/I18nProvider';
import { Dashboard } from './dashboard/Dashboard';
import { Inventory } from './inventory/Inventory';
import { Reviews } from './reviews/Reviews';
import { ErrorBoundary } from './lib/ErrorBoundary';
import { PageHeader, localizedErrorMessage } from './lib/ui';
import { LanguageSelector } from './lib/LanguageSelector';

const SettingsPage = lazy(() => import('./settings/SettingsPage').then((mod) => ({ default: mod.SettingsPage })));
const Prompts = lazy(() => import('./prompts/Prompts').then((mod) => ({ default: mod.Prompts })));
const Audit = lazy(() => import('./audit/Audit').then((mod) => ({ default: mod.Audit })));
const Users = lazy(() => import('./users/Users').then((mod) => ({ default: mod.Users })));
const DocumentChat = lazy(() => import('./chat/DocumentChat').then((mod) => ({ default: mod.DocumentChat })));
const DebugConsole = lazy(() => import('./debug/DebugConsole').then((mod) => ({ default: mod.DebugConsole })));

type Tab = 'dashboard' | 'inventory' | 'chat' | 'reviews' | 'settings' | 'prompts' | 'audit' | 'users' | 'debug';

export function App() {
  const { t } = useI18n();
  const [me, setMe] = useState<Me | null>(null);
  const [tab, setTab] = useState<Tab>('dashboard');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [debugConsoleEnabled, setDebugConsoleEnabled] = useState(false);

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

  const canUseChat = me.roles.some((role) => role === 'admin' || role === 'reviewer' || role === 'operator');
  const canManageSettings = me.roles.some((role) => role === 'admin');

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
          <NavButton icon={<Activity />} label={t('nav.dashboard')} active={tab === 'dashboard'} onClick={() => setTab('dashboard')} />
          <NavButton icon={<Archive />} label={t('nav.inventory')} active={tab === 'inventory'} onClick={() => setTab('inventory')} />
          {canUseChat && <NavButton icon={<MessageSquare />} label={t('nav.chat')} active={tab === 'chat'} onClick={() => setTab('chat')} />}
          <NavButton icon={<ListChecks />} label={t('nav.review')} active={tab === 'reviews'} onClick={() => setTab('reviews')} />
          <NavButton icon={<Settings />} label={t('nav.settings')} active={tab === 'settings'} onClick={() => setTab('settings')} />
          <NavButton icon={<ClipboardList />} label={t('nav.prompts')} active={tab === 'prompts'} onClick={() => setTab('prompts')} />
          <NavButton icon={<Shield />} label={t('nav.audit')} active={tab === 'audit'} onClick={() => setTab('audit')} />
          <NavButton icon={<UserPlus />} label={t('nav.users')} active={tab === 'users'} onClick={() => setTab('users')} />
          {debugConsoleEnabled && (
            <NavButton icon={<Bug />} label={t('nav.debug')} active={tab === 'debug'} onClick={() => setTab('debug')} />
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
            <Dashboard
              setError={setError}
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
                  nextTab === 'dashboard' || nextTab === 'inventory' ||
                  nextTab === 'chat' || nextTab === 'reviews' ||
                  nextTab === 'settings' || nextTab === 'prompts' ||
                  nextTab === 'audit' || nextTab === 'users' ||
                  nextTab === 'debug'
                ) {
                  setTab(nextTab);
                }
              }}
            />
          </ErrorBoundary>
        )}
        {tab === 'inventory' && (
          <ErrorBoundary>
            <Inventory setError={setError} />
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
            <Reviews setError={setError} />
          </ErrorBoundary>
        )}
        {tab === 'settings' && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <SettingsPage setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'prompts' && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <Prompts setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'audit' && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <Audit setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'users' && (
          <ErrorBoundary>
            <Suspense fallback={lazyFallback}>
              <Users setError={setError} />
            </Suspense>
          </ErrorBoundary>
        )}
        {tab === 'debug' && debugConsoleEnabled && (
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

function NavButton({ icon, label, active, onClick }: { icon: ReactNode; label: string; active: boolean; onClick: () => void }) {
  return <button className={active ? 'active' : ''} onClick={onClick}>{icon}{label}</button>;
}
