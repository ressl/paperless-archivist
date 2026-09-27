import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { api, type Me, type Permissions } from './api/client';
import { App } from './App';
import { I18nProvider } from './i18n/I18nProvider';

vi.mock('./buildInfo', () => ({
  buildInfo: { version: '1.0.0', commitSha: '', buildNumber: '' },
  buildInfoLabel: 'Version 1.0.0'
}));

// Replace every lazily loaded page with a stub so these tests exercise only the
// shell (navigation gates, routing, landmarks).
vi.mock('./dashboard/Dashboard', () => ({
  Dashboard: ({ canManageSettings }: { canManageSettings: boolean }) => (
    <h2>Dashboard page{canManageSettings ? ' (manage)' : ''}</h2>
  )
}));
vi.mock('./statistics/Statistics', () => ({ Statistics: () => <h2>Statistics page</h2> }));
vi.mock('./inventory/Inventory', () => ({ Inventory: () => <h2>Inventory page</h2> }));
vi.mock('./reviews/Reviews', () => ({ Reviews: () => <h2>Reviews page</h2> }));
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
      settings: vi.fn(async () => ({ ui: { debug_console_enabled: false } })),
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

function makeMe(permissions: Partial<Permissions>, roles: Me['roles'] = ['viewer']): Me {
  return { username: 'alice', roles, permissions: { ...NO_PERMISSIONS, ...permissions } };
}

async function renderApp(me: Me) {
  vi.mocked(api.me).mockResolvedValue(me);
  render(
    <I18nProvider>
      <App />
    </I18nProvider>
  );
  await screen.findByRole('navigation', {}, { timeout: 2_000 });
}

beforeEach(() => {
  window.localStorage.clear();
  window.history.replaceState(null, '', '/');
});

afterEach(() => cleanup());

describe('<App> navigation gates use permissions, not roles (#433)', () => {
  it('shows Chat for a role that is granted use_chat, even if not a built-in chat role', async () => {
    await renderApp(makeMe({ use_chat: true }, ['viewer']));
    expect(screen.getByRole('button', { name: 'Chat' })).toBeInTheDocument();
  });

  it('hides Chat for an admin whose permissions do not include use_chat', async () => {
    await renderApp(makeMe({ read_settings: true }, ['admin']));
    expect(screen.queryByRole('button', { name: 'Chat' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Settings' })).toBeInTheDocument();
  });

  it('derives canManageSettings from write_settings', async () => {
    await renderApp(makeMe({ write_settings: true }, ['operator']));
    expect(await screen.findByRole('heading', { name: 'Dashboard page (manage)' })).toBeInTheDocument();
    cleanup();
    await renderApp(makeMe({ read_settings: true }, ['admin']));
    expect(await screen.findByRole('heading', { name: 'Dashboard page' })).toBeInTheDocument();
  });
});
