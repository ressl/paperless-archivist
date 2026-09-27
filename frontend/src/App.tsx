import { Suspense, lazy, useCallback, useEffect, useMemo, useState, type MouseEvent, type ReactNode } from 'react';
import {
  Activity,
  Archive,
  BarChart3,
  Bug,
  ClipboardList,
  KeyRound,
  ListChecks,
  LogOut,
  Menu,
  MessageSquare,
  Settings,
  Shield,
  UserPlus,
  X
} from 'lucide-react';
import { api, Me, OidcConfig, setUnauthorizedHandler, type Permissions } from './api/client';
import { buildInfo, buildInfoLabel } from './buildInfo';
import { useI18n } from './i18n/I18nProvider';
import { ErrorBoundary } from './lib/ErrorBoundary';
import { PageHeader, localizedErrorMessage } from './lib/ui';
import { ToastProvider, useToast } from './lib/toast';
import { ThemeSelector } from './lib/theme';
import { LanguageSelector } from './lib/LanguageSelector';
import { isTab, navigate, parseRoute, replaceLocation, routePath, useLocation, type Route, type Tab } from './lib/router';

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

const MAIN_CONTENT_ID = 'main-content';
const SIDEBAR_PANEL_ID = 'sidebar-panel';

/**
 * Which tabs the signed-in user may render. Every gate reads the
 * server-computed permissions, never role names, so the UI cannot drift from
 * the backend's role→permission mapping (#433). `debugConsoleEnabled` is null
 * while /api/settings is still loading.
 */
function viewableTabs(permissions: Permissions, debugConsoleEnabled: boolean | null): Record<Tab, boolean> {
  return {
    // dashboard, inventory, reviews are ungated for any signed-in user.
    dashboard: true,
    inventory: true,
    reviews: true,
    statistics: permissions.read_dashboard,
    chat: permissions.use_chat,
    settings: permissions.read_settings,
    prompts: permissions.read_settings,
    audit: permissions.read_audit,
    users: permissions.manage_users,
    debug: debugConsoleEnabled === true && permissions.read_audit
  };
}

/**
 * The tab to render for `route`: the requested one if visible, else the
 * dashboard. null while the requested tab's gate is still loading (Debug waits
 * on /api/settings), so a /debug deep link is not bounced to the dashboard.
 */
function resolveActiveTab(route: Route, viewable: Record<Tab, boolean>, debugConsoleEnabled: boolean | null): Tab | null {
  if (!route.known) return 'dashboard';
  if (viewable[route.tab]) return route.tab;
  if (route.tab === 'debug' && debugConsoleEnabled === null) return null;
  return 'dashboard';
}

export function App() {
  // #450: every page reports through the toast queue instead of one banner.
  return (
    <ToastProvider>
      <AppShell />
    </ToastProvider>
  );
}

function AppShell() {
  const { t } = useI18n();
  const toast = useToast();
  // The URL is the source of truth for the current page, so reload, back /
  // forward and deep links all work (#424).
  const route = parseRoute(useLocation().pathname);
  const [me, setMe] = useState<Me | null>(null);
  const [loading, setLoading] = useState(true);
  // Pages keep their `setError` / `setSuccess` props (#450): a message is
  // queued as a toast (repeats fold into one), and `null` clears only the
  // page's own toasts of that tone, exactly like clearing the old banner did,
  // so budget or other app-level notifications are never dropped by a page.
  const setError = useCallback(
    (message: string | null) =>
      message ? toast.notify({ tone: 'error', message, scope: 'page' }) : toast.clear('page', 'error'),
    [toast]
  );
  const setSuccess = useCallback(
    (message: string | null) =>
      message ? toast.notify({ tone: 'success', message, scope: 'page' }) : toast.clear('page', 'success'),
    [toast]
  );
  const [debugConsoleEnabled, setDebugConsoleEnabled] = useState<boolean | null>(null);
  const [menuOpen, setMenuOpen] = useState(false);

  useEffect(() => {
    // When any request sees a 401 (expired session), drop back to the login
    // screen; this also unmounts the pollers so they stop spamming errors.
    setUnauthorizedHandler(() => {
      setMe(null);
      setError(null);
    });
    return () => setUnauthorizedHandler(null);
  }, [setError]);

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

  const viewable = useMemo(() => (me ? viewableTabs(me.permissions, debugConsoleEnabled) : null), [me, debugConsoleEnabled]);
  const activeTab = viewable ? resolveActiveTab(route, viewable, debugConsoleEnabled) : null;

  // Canonicalise the URL once the gates are known: unknown paths and tabs the
  // user can't see fall back to the dashboard (#296), and `/inventory/<id>`
  // becomes the inventory page filtered to that document.
  useEffect(() => {
    if (!viewable || activeTab === null) return;
    if (route.tab === 'inventory' && route.id !== undefined) {
      const documentId = Number(route.id);
      replaceLocation(
        Number.isInteger(documentId) && documentId > 0 ? `${routePath('inventory')}?id=${documentId}` : routePath('inventory')
      );
      return;
    }
    if (activeTab !== route.tab || !route.known) replaceLocation(routePath('dashboard'));
  }, [viewable, activeTab, route.tab, route.id, route.known]);

  // A new page starts without the previous page's banners and with the mobile
  // menu collapsed — also on browser back/forward.
  useEffect(() => {
    toast.clear('page');
    setMenuOpen(false);
  }, [activeTab, toast]);

  if (loading) return <div className="boot">{t('app.loading')}</div>;
  if (!me || !viewable)
    return (
      <Login
        onLogin={(loggedIn) => {
          // Clear any banner left over from the 401 that sent us here, so a
          // fresh login doesn't mount the workspace with a stale
          // "Unauthorized" error. (#287)
          setError(null);
          setSuccess(null);
          setMe(loggedIn);
        }}
      />
    );

  const canManageSettings = me.permissions.write_settings;

  // Switch tabs through the router so the URL, history and any registered
  // beforeNavigate guard (e.g. unsaved changes) all see the navigation. A tab
  // the user can't render falls back to the dashboard (#296). `search` carries
  // cross-tab state such as inventory filters.
  const selectTab = (next: Tab, search = '') => {
    const target = viewable[next] ? next : 'dashboard';
    if (navigate(`${routePath(target)}${search}`)) {
      setError(null);
      setSuccess(null);
      setMenuOpen(false);
    }
  };

  const lazyFallback = (
    <section className="page">
      <PageHeader title={t('app.loading')} />
    </section>
  );

  const page = (tab: Tab, content: ReactNode) =>
    activeTab === tab && (
      <ErrorBoundary>
        <Suspense fallback={lazyFallback}>{content}</Suspense>
      </ErrorBoundary>
    );

  const navItem = (tab: Tab, icon: ReactNode, label: string) =>
    viewable[tab] && <NavLink tab={tab} icon={icon} label={label} active={activeTab === tab} onSelect={selectTab} />;

  return (
    <ErrorBoundary>
    <div className="app-shell">
      <a className="skip-link" href={`#${MAIN_CONTENT_ID}`} onClick={focusMainContent}>
        {t('nav.skip_to_content')}
      </a>
      <aside className={menuOpen ? 'sidebar sidebar--open' : 'sidebar'}>
        <div className="brand">
          <img src="/assets/brand/paperless-archivist-logo.png" alt="" />
          <div>
            <strong>{t('app.name')}</strong>
            <span>{me.username}</span>
          </div>
          {/* Only displayed on narrow screens, where the navigation collapses
              so the page content is reachable without scrolling past it (#431). */}
          <button
            type="button"
            className="ghost-button sidebar-toggle"
            aria-expanded={menuOpen}
            aria-controls={SIDEBAR_PANEL_ID}
            onClick={() => setMenuOpen((open) => !open)}
          >
            {menuOpen ? <X size={18} aria-hidden="true" /> : <Menu size={18} aria-hidden="true" />}
            {t('nav.menu')}
          </button>
        </div>
        <div className="sidebar-panel" id={SIDEBAR_PANEL_ID}>
        <nav aria-label={t('nav.main_label')}>
          {/* Fixed-order, labelled groups so nav positions never shift by role (#235). */}
          <div className="nav-group" role="group" aria-label={t('nav.group.operations')}>
            <span className="nav-group-label" aria-hidden="true">{t('nav.group.operations')}</span>
            {navItem('dashboard', <Activity />, t('nav.dashboard'))}
            {navItem('statistics', <BarChart3 />, t('nav.statistics'))}
            {navItem('inventory', <Archive />, t('nav.inventory'))}
            {navItem('reviews', <ListChecks />, t('nav.review'))}
            {navItem('chat', <MessageSquare />, t('nav.chat'))}
          </div>
          {(viewable.settings || viewable.users) && (
            <div className="nav-group" role="group" aria-label={t('nav.group.configuration')}>
              <span className="nav-group-label" aria-hidden="true">{t('nav.group.configuration')}</span>
              {navItem('settings', <Settings />, t('nav.settings'))}
              {navItem('prompts', <ClipboardList />, t('nav.prompts'))}
              {navItem('users', <UserPlus />, t('nav.users'))}
            </div>
          )}
          {(viewable.audit || viewable.debug) && (
            <div className="nav-group" role="group" aria-label={t('nav.group.system')}>
              <span className="nav-group-label" aria-hidden="true">{t('nav.group.system')}</span>
              {navItem('audit', <Shield />, t('nav.audit'))}
              {navItem('debug', <Bug />, t('nav.debug'))}
            </div>
          )}
        </nav>
        <LanguageSelector />
        <ThemeSelector />
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
            } catch {
              // Best-effort: the cookie is invalidated server-side and we clear
              // client state below regardless. Swallow so a failed logout call
              // doesn't surface as an unhandled promise rejection. (#296)
            } finally {
              setMe(null);
            }
          }}
        >
          <LogOut size={18} /> {t('nav.logout')}
        </button>
        </div>
      </aside>

      <main className="workspace" id={MAIN_CONTENT_ID} tabIndex={-1}>
        {activeTab === null && lazyFallback}
        {page(
          'dashboard',
          <Dashboard
            setError={setError}
            setSuccess={setSuccess}
            canManageSettings={canManageSettings}
            permissions={me.permissions}
            onNavigate={(nextTab, search) => {
              // Cross-tab navigation (dashboard alerts, stage matrix). The
              // optional query string travels in the URL, so the destination
              // (e.g. Inventory filters) reads it on mount and Back returns
              // to the dashboard.
              if (isTab(nextTab)) selectTab(nextTab, search ?? '');
            }}
          />
        )}
        {page('statistics', <Statistics setError={setError} />)}
        {page(
          'inventory',
          <Inventory
            setError={setError}
            // #449: hand the selected documents to the chat as its filter.
            onAskInChat={viewable.chat ? (ids) => selectTab('chat', `?documents=${ids.join(',')}`) : undefined}
          />
        )}
        {page('chat', <DocumentChat setError={setError} />)}
        {page('reviews', <Reviews setError={setError} setSuccess={setSuccess} focusReviewId={route.tab === 'reviews' ? route.id : undefined} />)}
        {page('settings', <SettingsPage setError={setError} />)}
        {page('prompts', <Prompts setError={setError} />)}
        {page('audit', <Audit setError={setError} />)}
        {page('users', <Users setError={setError} />)}
        {page('debug', <DebugConsole setError={setError} />)}
      </main>
    </div>
    </ErrorBoundary>
  );
}

/** Skip link target: move focus to the main landmark without touching the URL. */
function focusMainContent(event: MouseEvent<HTMLAnchorElement>) {
  const main = document.getElementById(MAIN_CONTENT_ID);
  if (!main) return;
  event.preventDefault();
  main.focus();
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

/**
 * Sidebar entry. A real link (so it can be opened in a new tab or copied) whose
 * plain left-click is routed in-app; the current page carries
 * `aria-current="page"` (#431).
 */
function NavLink({
  tab,
  icon,
  label,
  active,
  onSelect
}: {
  tab: Tab;
  icon: ReactNode;
  label: string;
  active: boolean;
  onSelect: (tab: Tab) => void;
}) {
  return (
    <a
      href={routePath(tab)}
      className={active ? 'nav-link active' : 'nav-link'}
      aria-current={active ? 'page' : undefined}
      onClick={(event) => {
        // Let the browser handle new-tab / new-window / download gestures.
        if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
        event.preventDefault();
        onSelect(tab);
      }}
    >
      {icon}
      {label}
    </a>
  );
}
