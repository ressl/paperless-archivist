import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { api, type Me, type Permissions } from './api/client';
import { App } from './App';
import { I18nProvider } from './i18n/I18nProvider';
import { registerBeforeNavigate } from './lib/router';

expect.extend(toHaveNoViolations);

vi.mock('./buildInfo', () => ({
  buildInfo: { version: '1.0.0', commitSha: '', buildNumber: '' },
  buildInfoLabel: 'Version 1.0.0'
}));

// Replace every lazily loaded page with a stub so these tests exercise only the
// shell (navigation gates, routing, landmarks).
vi.mock('./dashboard/Dashboard', () => ({
  Dashboard: ({
    canManageSettings,
    onNavigate
  }: {
    canManageSettings: boolean;
    onNavigate: (tab: string, search?: string) => void;
  }) => (
    <>
      <h2>Dashboard page{canManageSettings ? ' (manage)' : ''}</h2>
      <button type="button" onClick={() => onNavigate('inventory', '?has_error=true')}>
        Show failed documents
      </button>
      <button type="button" onClick={() => onNavigate('users')}>
        Open users
      </button>
    </>
  )
}));
vi.mock('./statistics/Statistics', () => ({ Statistics: () => <h2>Statistics page</h2> }));
vi.mock('./inventory/Inventory', () => ({
  Inventory: () => <h2>Inventory page {window.location.search}</h2>
}));
vi.mock('./reviews/Reviews', () => ({
  Reviews: ({ focusReviewId }: { focusReviewId?: string }) => (
    <h2>Reviews page{focusReviewId ? ` focus=${focusReviewId}` : ''}</h2>
  )
}));
vi.mock('./settings/SettingsPage', () => ({ SettingsPage: () => <h2>Settings page</h2> }));
vi.mock('./prompts/Prompts', () => ({ Prompts: () => <h2>Prompts page</h2> }));
vi.mock('./audit/Audit', () => ({ Audit: () => <h2>Audit page</h2> }));
vi.mock('./users/Users', () => ({ Users: () => <h2>Users page</h2> }));
vi.mock('./chat/DocumentChat', () => ({ DocumentChat: () => <h2>Chat page</h2> }));
vi.mock('./debug/DebugConsole', () => ({ DebugConsole: () => <h2>Debug page</h2> }));

vi.mock('./api/client', async () => {
  const actual = await vi.importActual<typeof import('./api/client')>('./api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      me: vi.fn(),
      settings: vi.fn(),
      oidcConfig: vi.fn(async () => ({ enabled: false, paperless_login_enabled: false }))
    }
  };
});

const NO_PERMISSIONS: Permissions = {
  read_dashboard: false,
  read_runs: false,
  write_runs: false,
  read_inventory: false,
  write_batches: false,
  use_chat: false,
  read_reviews: false,
  write_reviews: false,
  read_settings: false,
  write_settings: false,
  manage_users: false,
  read_audit: false
};

const ADMIN_PERMISSIONS: Permissions = Object.fromEntries(
  Object.keys(NO_PERMISSIONS).map((key) => [key, true])
) as Permissions;

function makeMe(permissions: Partial<Permissions>, roles: Me['roles'] = ['viewer']): Me {
  return { username: 'alice', roles, permissions: { ...NO_PERMISSIONS, ...permissions } };
}

async function renderApp(me: Me, { debugConsole = false }: { debugConsole?: boolean } = {}) {
  vi.mocked(api.me).mockResolvedValue(me);
  vi.mocked(api.settings).mockResolvedValue({ ui: { debug_console_enabled: debugConsole } } as never);
  const view = render(
    <I18nProvider>
      <App />
    </I18nProvider>
  );
  await screen.findByRole('navigation', { name: 'Main navigation' }, { timeout: 2_000 });
  return view;
}

/** Simulate the browser's back/forward button landing on `path`. */
function browserHistoryTo(path: string) {
  act(() => {
    window.history.replaceState(null, '', path);
    window.dispatchEvent(new PopStateEvent('popstate'));
  });
}

const heading = (name: string | RegExp) => screen.findByRole('heading', { name }, { timeout: 2_000 });

beforeEach(() => {
  window.localStorage.clear();
  window.history.replaceState(null, '', '/');
});

afterEach(() => cleanup());

describe('<App> navigation gates use permissions, not roles (#433)', () => {
  it('shows Chat for a role that is granted use_chat, even if not a built-in chat role', async () => {
    await renderApp(makeMe({ use_chat: true }, ['viewer']));
    expect(screen.getByRole('link', { name: 'Chat' })).toBeInTheDocument();
  });

  it('hides Chat for an admin whose permissions do not include use_chat', async () => {
    await renderApp(makeMe({ read_settings: true }, ['admin']));
    expect(screen.queryByRole('link', { name: 'Chat' })).not.toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Settings' })).toBeInTheDocument();
  });

  it('derives canManageSettings from write_settings', async () => {
    await renderApp(makeMe({ write_settings: true }, ['operator']));
    expect(await heading('Dashboard page (manage)')).toBeInTheDocument();
    cleanup();
    await renderApp(makeMe({ read_settings: true }, ['admin']));
    expect(await heading('Dashboard page')).toBeInTheDocument();
  });
});

describe('<App> URL routing (#424)', () => {
  it('renders the page named by the URL on load (reload keeps the page)', async () => {
    window.history.replaceState(null, '', '/prompts');
    await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']));
    expect(await heading('Prompts page')).toBeInTheDocument();
    expect(window.location.pathname).toBe('/prompts');
  });

  it('nav clicks push history entries and back/forward switch pages', async () => {
    await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']));
    await heading(/Dashboard page/);
    fireEvent.click(screen.getByRole('link', { name: 'Settings' }));
    expect(await heading('Settings page')).toBeInTheDocument();
    expect(window.location.pathname).toBe('/settings');

    browserHistoryTo('/');
    expect(await heading(/Dashboard page/)).toBeInTheDocument();
    browserHistoryTo('/settings');
    expect(await heading('Settings page')).toBeInTheDocument();
  });

  it('carries cross-tab query strings from the dashboard into the URL', async () => {
    await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']));
    fireEvent.click(await screen.findByRole('button', { name: 'Show failed documents' }));
    expect(await heading('Inventory page ?has_error=true')).toBeInTheDocument();
    expect(`${window.location.pathname}${window.location.search}`).toBe('/inventory?has_error=true');
  });

  it('falls back to the dashboard for tabs the user cannot see (#296)', async () => {
    await renderApp(makeMe({}, ['viewer']));
    fireEvent.click(await screen.findByRole('button', { name: 'Open users' }));
    expect(window.location.pathname).toBe('/');

    cleanup();
    window.history.replaceState(null, '', '/users');
    await renderApp(makeMe({}, ['viewer']));
    expect(await heading(/Dashboard page/)).toBeInTheDocument();
    await waitFor(() => expect(window.location.pathname).toBe('/'));
  });

  it('canonicalises unknown paths to the dashboard', async () => {
    window.history.replaceState(null, '', '/does/not/exist');
    await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']));
    expect(await heading(/Dashboard page/)).toBeInTheDocument();
    await waitFor(() => expect(window.location.pathname).toBe('/'));
  });

  it('waits for the settings gate before resolving a /debug deep link', async () => {
    window.history.replaceState(null, '', '/debug');
    await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']), { debugConsole: true });
    expect(await heading('Debug page')).toBeInTheDocument();
    expect(window.location.pathname).toBe('/debug');
  });

  it('deep-links a review and an inventory document', async () => {
    window.history.replaceState(null, '', '/reviews/rev-7');
    await renderApp(makeMe({}, ['viewer']));
    expect(await heading('Reviews page focus=rev-7')).toBeInTheDocument();

    cleanup();
    window.history.replaceState(null, '', '/inventory/42');
    await renderApp(makeMe({}, ['viewer']));
    await waitFor(() => expect(`${window.location.pathname}${window.location.search}`).toBe('/inventory?id=42'));
    expect(await heading('Inventory page ?id=42')).toBeInTheDocument();
  });

  it('lets a beforeNavigate guard veto tab switches', async () => {
    await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']));
    await heading(/Dashboard page/);
    const unregister = registerBeforeNavigate(() => false);
    fireEvent.click(screen.getByRole('link', { name: 'Audit' }));
    expect(window.location.pathname).toBe('/');
    expect(screen.queryByRole('heading', { name: 'Audit page' })).not.toBeInTheDocument();
    unregister();
    fireEvent.click(screen.getByRole('link', { name: 'Audit' }));
    expect(await heading('Audit page')).toBeInTheDocument();
  });
});

describe('<App> navigation accessibility (#431)', () => {
  it('marks only the current entry with aria-current="page"', async () => {
    window.history.replaceState(null, '', '/reviews');
    await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']));
    await heading('Reviews page');
    const current = screen.getAllByRole('link').filter((link) => link.getAttribute('aria-current') === 'page');
    expect(current).toHaveLength(1);
    expect(current[0]).toHaveAccessibleName('Review');
    expect(current[0]).toHaveAttribute('href', '/reviews');
  });

  it('offers a skip link as the first focusable element that focuses <main>', async () => {
    await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']));
    const focusable = document.querySelectorAll<HTMLElement>('a[href], button, input, select, textarea, [tabindex]:not([tabindex="-1"])');
    const skip = screen.getByRole('link', { name: 'Skip to main content' });
    expect(focusable[0]).toBe(skip);
    skip.focus();
    expect(skip).toHaveFocus();
    fireEvent.click(skip);
    expect(screen.getByRole('main')).toHaveFocus();
    expect(window.location.hash).toBe('');
  });

  it('collapses the navigation behind a Menu toggle and closes it after navigating', async () => {
    await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']));
    const toggle = screen.getByRole('button', { name: 'Menu' });
    const panel = document.getElementById(toggle.getAttribute('aria-controls') ?? '');
    expect(panel).not.toBeNull();
    expect(panel).toContainElement(screen.getByRole('navigation', { name: 'Main navigation' }));
    expect(toggle).toHaveAttribute('aria-expanded', 'false');
    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute('aria-expanded', 'true');
    expect(toggle.closest('aside')).toHaveClass('sidebar--open');
    fireEvent.click(screen.getByRole('link', { name: 'Inventory' }));
    await heading(/Inventory page/);
    expect(toggle).toHaveAttribute('aria-expanded', 'false');
  });

  it('has no axe violations in the shell', async () => {
    const { container } = await renderApp(makeMe(ADMIN_PERMISSIONS, ['admin']));
    await heading(/Dashboard page/);
    expect(await axe(container)).toHaveNoViolations();
  });
});
