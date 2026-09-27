import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type ApiToken, type Me, type UserItem } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

expect.extend(toHaveNoViolations);

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      me: vi.fn(),
      users: vi.fn(),
      sessions: vi.fn(async () => ({ items: [] })),
      apiTokens: vi.fn(),
      createUser: vi.fn(),
      updateUserRoles: vi.fn(),
      resetPassword: vi.fn(async () => ({ ok: true })),
      disableUser: vi.fn(async () => ({ ok: true })),
      revokeApiToken: vi.fn(async () => ({ ok: true })),
      rotateApiToken: vi.fn(async () => ({ id: 'tok-1', token: 'secret-token' }))
    }
  };
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

const alice: UserItem = { id: 'u-alice', username: 'alice', roles: ['admin'], enabled: true, created_at: '2026-01-01T00:00:00Z' };
const bob: UserItem = { id: 'u-bob', username: 'bob', roles: ['viewer'], enabled: true, created_at: '2026-01-01T00:00:00Z' };
const token: ApiToken = { id: 'tok-1', name: 'ci', scopes: ['runs:read'], created_at: '2026-01-01T00:00:00Z' };

async function renderUsers(users: UserItem[] = [alice, bob]) {
  vi.mocked(api.users).mockResolvedValue({ items: users.map((user) => ({ ...user, roles: [...user.roles] })) });
  const { Users } = await import('./Users');
  const setError = vi.fn();
  render(
    <I18nProvider>
      <Users setError={setError} />
    </I18nProvider>
  );
  await screen.findByRole('group', { name: 'Roles for bob' });
  // Wait until the current user is known (self-guards depend on it).
  await waitFor(() => expect(screen.getByText('(you)')).toBeInTheDocument());
  return { setError };
}

describe('<Users> admin safety (#422, #417)', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
    vi.mocked(api.me).mockResolvedValue({ username: 'alice', roles: ['admin'], permissions: {} } as unknown as Me);
    vi.mocked(api.apiTokens).mockResolvedValue({ items: [token] });
  });

  it('keeps both role changes when two checkboxes are clicked quickly', async () => {
    const first = deferred<{ ok: boolean }>();
    const second = deferred<{ ok: boolean }>();
    vi.mocked(api.updateUserRoles).mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
    await renderUsers();
    const roles = screen.getByRole('group', { name: 'Roles for bob' });

    fireEvent.click(within(roles).getByRole('checkbox', { name: 'reviewer role for bob' }));
    fireEvent.click(within(roles).getByRole('checkbox', { name: 'operator role for bob' }));

    expect(within(roles).getByRole('checkbox', { name: 'reviewer role for bob' })).toBeChecked();
    expect(within(roles).getByRole('checkbox', { name: 'operator role for bob' })).toBeChecked();
    await waitFor(() => expect(api.updateUserRoles).toHaveBeenCalledTimes(1));
    expect(api.updateUserRoles).toHaveBeenNthCalledWith(1, 'u-bob', ['viewer', 'reviewer']);

    await act(async () => first.resolve({ ok: true }));
    await waitFor(() => expect(api.updateUserRoles).toHaveBeenCalledTimes(2));
    expect(api.updateUserRoles).toHaveBeenNthCalledWith(2, 'u-bob', ['viewer', 'reviewer', 'operator']);
    await act(async () => second.resolve({ ok: true }));
  });

  it('submits a new user only once on a double submit', async () => {
    const create = deferred<{ id: string }>();
    vi.mocked(api.createUser).mockReturnValue(create.promise);
    await renderUsers();
    fireEvent.change(screen.getByRole('textbox', { name: 'Username' }), { target: { value: 'carol' } });
    fireEvent.change(screen.getByLabelText('Password'), { target: { value: 'pw-123456' } });
    const submit = screen.getByRole('button', { name: 'Create' });

    fireEvent.click(submit);
    fireEvent.click(submit);
    expect(submit).toBeDisabled();
    expect(api.createUser).toHaveBeenCalledTimes(1);

    await act(async () => create.resolve({ id: 'u-carol' }));
    expect(await screen.findByText('User carol created.')).toBeInTheDocument();
  });

  it('prevents self-lockout: own disable is blocked, own admin removal asks', async () => {
    await renderUsers([{ ...alice, roles: ['admin', 'viewer'] }, bob]);

    expect(screen.getByRole('button', { name: 'Disable alice' })).toBeDisabled();
    // A user's last remaining role cannot be removed.
    expect(screen.getByRole('checkbox', { name: 'viewer role for bob' })).toBeDisabled();

    fireEvent.click(screen.getByRole('checkbox', { name: 'admin role for alice' }));
    const dialog = await screen.findByRole('alertdialog', { name: 'Remove your own admin role?' });
    expect(await axe(dialog)).toHaveNoViolations();
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());

    expect(api.updateUserRoles).not.toHaveBeenCalled();
    expect(screen.getByRole('checkbox', { name: 'admin role for alice' })).toBeChecked();
  });

  it('confirms disabling another user and revoking a token', async () => {
    await renderUsers();

    fireEvent.click(screen.getByRole('button', { name: 'Disable bob' }));
    const disableDialog = await screen.findByRole('alertdialog', { name: 'Disable bob?' });
    fireEvent.click(within(disableDialog).getByRole('button', { name: 'Disable' }));
    await waitFor(() => expect(api.disableUser).toHaveBeenCalledWith('u-bob'));

    fireEvent.click(screen.getByRole('button', { name: 'Revoke token ci' }));
    const revokeDialog = await screen.findByRole('alertdialog', { name: 'Revoke token ci?' });
    fireEvent.click(within(revokeDialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(api.revokeApiToken).not.toHaveBeenCalled();
  });

  it('announces a successful password reset', async () => {
    await renderUsers();
    fireEvent.change(screen.getByLabelText('New password for bob'), { target: { value: 'new-secret-1' } });
    fireEvent.click(screen.getByRole('button', { name: 'Reset password for bob' }));

    await waitFor(() => expect(api.resetPassword).toHaveBeenCalledWith('u-bob', 'new-secret-1'));
    expect(await screen.findByText('Password for bob was reset.')).toBeInTheDocument();
    expect(screen.getByLabelText('New password for bob')).toHaveValue('');
  });
});
