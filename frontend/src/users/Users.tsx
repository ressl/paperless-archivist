import { useCallback, useEffect, useRef, useState } from 'react';
import { Copy, KeyRound, Power, RotateCcw, UserPlus, X } from 'lucide-react';
import { api, ApiToken, Role, SessionItem, UserItem } from '../api/client';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { Button, NumberField, PageHeader, localizedErrorMessage } from '../lib/ui';
import { useConfirm } from '../lib/ConfirmDialog';
import { useResource } from '../lib/useResource';

const ALL_ROLES: Role[] = ['viewer', 'reviewer', 'operator', 'auditor', 'admin'];

function splitTags(value: string) {
  return value
    .split(',')
    .map((entry) => entry.trim())
    .filter(Boolean);
}

const roleLabel = (role: Role, t: TFunction) => t(`users.role_${role}` as Parameters<TFunction>[0]);

export function Users({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime } = useI18n();
  const [users, setUsersState] = useState<UserItem[]>([]);
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
  const [currentUsername, setCurrentUsername] = useState<string | null>(null);
  // #422: one busy key per in-flight request ("create-user", "reset:<id>", …).
  // The ref blocks a second submit synchronously; the state drives `disabled`.
  const inFlight = useRef(new Set<string>());
  const [pending, setPending] = useState<ReadonlySet<string>>(() => new Set());
  const [notice, setNotice] = useState<string | null>(null);
  const { confirm, dialog: confirmDialog } = useConfirm();

  // Latest users snapshot for role edits: computing the next role set from the
  // render-time `user.roles` lost one of two quick clicks (#422).
  const usersRef = useRef<UserItem[]>([]);
  const setUsers = useCallback((next: UserItem[]) => {
    usersRef.current = next;
    setUsersState(next);
  }, []);
  // Role updates are serialised per user so each request carries the
  // cumulative role set of all earlier clicks.
  const roleQueues = useRef(new Map<string, Promise<void>>());

  // Shared fetch state (#444): cancellable requests, and a slower earlier
  // reload can never overwrite a newer one.
  const { reload: load } = useResource(
    (signal) => Promise.all([api.users({ signal }), api.sessions({ signal }), api.apiTokens({ signal })]),
    [],
    {
      onError: (err) => setError(localizedErrorMessage(err, t)),
      onSuccess: ([userData, sessionData, tokenData]) => {
        // Keep optimistic roles for users whose role updates are still queued.
        const local = new Map(usersRef.current.map((user) => [user.id, user.roles]));
        setUsers(
          userData.items.map((user) =>
            roleQueues.current.has(user.id) && local.has(user.id) ? { ...user, roles: local.get(user.id)! } : user
          )
        );
        setSessions(sessionData.items);
        setTokens(tokenData.items);
      }
    }
  );

  useEffect(() => {
    api
      .me()
      .then((me) => setCurrentUsername(me.username))
      .catch(() => setCurrentUsername(null));
  }, []);

  const guarded = async (key: string, action: () => Promise<unknown>) => {
    if (inFlight.current.has(key)) return;
    inFlight.current.add(key);
    setPending(new Set(inFlight.current));
    try {
      await action();
    } catch (err) {
      setError(localizedErrorMessage(err, t));
    } finally {
      inFlight.current.delete(key);
      setPending(new Set(inFlight.current));
    }
  };
  const isPending = (key: string) => pending.has(key);

  const isSelf = (user: UserItem) => currentUsername !== null && user.username === currentUsername;

  const toggleRole = async (userId: string, roleOption: Role) => {
    const user = usersRef.current.find((entry) => entry.id === userId);
    if (!user) return;
    const removing = user.roles.includes(roleOption);
    // Self-lockout guard: removing your own admin role needs an explicit
    // confirmation (the backend still enforces "at least one admin").
    if (removing && roleOption === 'admin' && isSelf(user)) {
      const confirmed = await confirm({
        title: t('users.self_admin_confirm.title'),
        description: t('users.self_admin_confirm.description'),
        confirmLabel: t('users.self_admin_confirm.confirm')
      });
      if (!confirmed) return;
    }
    // Re-read after the (possibly async) confirmation.
    const latest = usersRef.current.find((entry) => entry.id === userId);
    if (!latest) return;
    const nextRoles = latest.roles.includes(roleOption)
      ? latest.roles.filter((existing) => existing !== roleOption)
      : [...latest.roles, roleOption];
    if (nextRoles.length === 0) {
      setError(t('users.error_no_roles', { user: latest.username }));
      return;
    }
    setNotice(null);
    setUsers(usersRef.current.map((entry) => (entry.id === userId ? { ...entry, roles: nextRoles } : entry)));
    const previous = roleQueues.current.get(userId) ?? Promise.resolve();
    const queued: Promise<void> = previous
      .then(() => api.updateUserRoles(userId, nextRoles))
      .then(() => undefined)
      .catch((err) => setError(localizedErrorMessage(err, t)))
      .finally(() => {
        if (roleQueues.current.get(userId) === queued) {
          roleQueues.current.delete(userId);
          void load();
        }
      });
    roleQueues.current.set(userId, queued);
  };

  const disableUser = async (user: UserItem) => {
    if (isSelf(user)) return;
    const confirmed = await confirm({
      title: t('users.disable_confirm.title', { user: user.username }),
      description: t('users.disable_confirm.description', { user: user.username }),
      confirmLabel: t('users.disable')
    });
    if (!confirmed) return;
    await guarded(`status:${user.id}`, async () => {
      await api.disableUser(user.id);
      await load();
    });
  };

  const revokeSession = async (session: SessionItem) => {
    const confirmed = await confirm({
      title: t('users.revoke_session_confirm.title', { user: session.username }),
      description: t('users.revoke_session_confirm.description', { user: session.username }),
      confirmLabel: t('users.revoke_session')
    });
    if (!confirmed) return;
    await guarded(`session:${session.id}`, async () => {
      await api.revokeSession(session.id);
      await load();
    });
  };

  const rotateToken = async (token: ApiToken) => {
    const confirmed = await confirm({
      title: t('users.rotate_token_confirm.title', { name: token.name }),
      description: t('users.rotate_token_confirm.description', { name: token.name }),
      confirmLabel: t('users.rotate_token')
    });
    if (!confirmed) return;
    await guarded(`token:${token.id}`, async () => {
      const created = await api.rotateApiToken(token.id, { expires_in_days: tokenExpiresInDays });
      setNewToken(created.token);
      await load();
    });
  };

  const revokeToken = async (token: ApiToken) => {
    const confirmed = await confirm({
      title: t('users.revoke_token_confirm.title', { name: token.name }),
      description: t('users.revoke_token_confirm.description', { name: token.name }),
      confirmLabel: t('users.revoke_token')
    });
    if (!confirmed) return;
    await guarded(`token:${token.id}`, async () => {
      await api.revokeApiToken(token.id);
      await load();
    });
  };

  return (
    <section className="page">
      <PageHeader title={t('users.title')} />
      <p className="inline-notice" role="status" aria-live="polite">{notice}</p>
      <form className="compact-form" onSubmit={(event) => {
        event.preventDefault();
        void guarded('create-user', async () => {
          await api.createUser({ username, password, roles: [role] });
          setNotice(t('users.created_notice', { user: username }));
          setUsername('');
          setPassword('');
          await load();
        });
      }}>
        <input value={username} onChange={(event) => setUsername(event.target.value)} placeholder={t('auth.username')} aria-label={t('auth.username')} />
        <input value={password} onChange={(event) => setPassword(event.target.value)} placeholder={t('auth.password')} type="password" aria-label={t('auth.password')} />
        <select value={role} aria-label={t('users.new_user_role')} onChange={(event) => setRole(event.target.value as Role)}>
          {ALL_ROLES.map((roleOption) => (
            <option key={roleOption} value={roleOption}>{roleLabel(roleOption, t)}</option>
          ))}
        </select>
        <Button variant="primary" icon={<UserPlus size={16} />} disabled={isPending('create-user')} aria-busy={isPending('create-user')}>
          {t('users.create')}
        </Button>
      </form>
      <div className="table-wrap">
        <table>
          <thead><tr><th>{t('users.col_user')}</th><th>{t('users.col_roles')}</th><th>{t('users.col_status')}</th><th>{t('users.col_password')}</th><th>{t('users.col_actions')}</th></tr></thead>
          <tbody>
            {users.map((user) => (
              <tr key={user.id}>
                <td>{user.username}{isSelf(user) && <small className="field-hint"> {t('users.you')}</small>}</td>
                <td>
                  <fieldset className="role-checkboxes" aria-label={t('users.roles_for', { user: user.username })}>
                    {ALL_ROLES.map((roleOption) => {
                      const checked = user.roles.includes(roleOption);
                      // The last remaining role cannot be removed (a user
                      // without roles would be locked out of everything).
                      const lastRole = checked && user.roles.length === 1;
                      return (
                        <label key={roleOption}>
                          <input
                            type="checkbox"
                            checked={checked}
                            disabled={lastRole}
                            title={lastRole ? t('users.last_role_hint') : undefined}
                            aria-label={t('users.role_for', { role: roleLabel(roleOption, t), user: user.username })}
                            onChange={() => void toggleRole(user.id, roleOption)}
                          />
                          {roleLabel(roleOption, t)}
                        </label>
                      );
                    })}
                  </fieldset>
                </td>
                <td>{user.enabled ? t('users.status_enabled') : t('users.status_disabled')}</td>
                <td className="inline-edit">
                  <input
                    value={resetPasswords[user.id] ?? ''}
                    onChange={(event) => setResetPasswords((current) => ({ ...current, [user.id]: event.target.value }))}
                    type="password"
                    placeholder={t('users.new_password')}
                    aria-label={t('users.new_password_for', { user: user.username })}
                  />
                  <Button
                    variant="secondary"
                    icon={<RotateCcw size={16} />}
                    title={t('users.reset_password')}
                    aria-label={t('users.reset_password_for', { user: user.username })}
                    disabled={!resetPasswords[user.id] || isPending(`reset:${user.id}`)}
                    aria-busy={isPending(`reset:${user.id}`)}
                    onClick={() =>
                      void guarded(`reset:${user.id}`, async () => {
                        await api.resetPassword(user.id, resetPasswords[user.id] ?? '');
                        setResetPasswords((current) => ({ ...current, [user.id]: '' }));
                        setNotice(t('users.password_reset_done', { user: user.username }));
                        await load();
                      })
                    }
                  />
                </td>
                <td>
                  {user.enabled ? (
                    <Button
                      variant="secondary"
                      icon={<Power size={16} />}
                      title={isSelf(user) ? t('users.cannot_disable_self') : t('users.disable_user')}
                      aria-label={t('users.disable_user_for', { user: user.username })}
                      disabled={isSelf(user) || isPending(`status:${user.id}`)}
                      onClick={() => void disableUser(user)}
                    >
                      {t('users.disable')}
                    </Button>
                  ) : (
                    <Button
                      variant="secondary"
                      icon={<Power size={16} />}
                      title={t('users.enable_user')}
                      aria-label={t('users.enable_user_for', { user: user.username })}
                      disabled={isPending(`status:${user.id}`)}
                      onClick={() =>
                        void guarded(`status:${user.id}`, async () => {
                          await api.enableUser(user.id);
                          await load();
                        })
                      }
                    >
                      {t('users.enable')}
                    </Button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <PageHeader title={t('users.sessions_title')} />
      <div className="table-wrap">
        <table>
          <thead><tr><th>{t('users.col_user')}</th><th>{t('users.col_created')}</th><th>{t('users.col_last_seen')}</th><th>{t('users.col_expires')}</th><th>{t('users.col_status')}</th><th>{t('users.col_action')}</th></tr></thead>
          <tbody>
            {sessions.map((session) => (
              <tr key={session.id}>
                <td>{session.username}</td>
                <td>{formatDateTime(session.created_at)}</td>
                <td>{session.last_seen_at ? formatDateTime(session.last_seen_at) : '-'}</td>
                <td>{formatDateTime(session.expires_at)}</td>
                <td>{session.revoked_at ? t('users.status_revoked') : t('users.status_active')}</td>
                <td>
                  {!session.revoked_at && (
                    <Button
                      variant="secondary"
                      icon={<X size={16} />}
                      title={t('users.revoke_session')}
                      aria-label={t('users.revoke_session_for', { user: session.username })}
                      disabled={isPending(`session:${session.id}`)}
                      onClick={() => void revokeSession(session)}
                    />
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <PageHeader title={t('users.api_tokens_title')} />
      <form className="compact-form" onSubmit={(event) => {
        event.preventDefault();
        void guarded('create-token', async () => {
          const created = await api.createApiToken({ name: tokenName, scopes: splitTags(tokenScopes), expires_in_days: tokenExpiresInDays });
          setNewToken(created.token);
          setTokenName('');
          await load();
        });
      }}>
        <input value={tokenName} onChange={(event) => setTokenName(event.target.value)} placeholder={t('users.token_name')} aria-label={t('users.token_name')} />
        <input value={tokenScopes} onChange={(event) => setTokenScopes(event.target.value)} placeholder={t('users.token_scopes_placeholder')} aria-label={t('users.token_scopes')} />
        <NumberField
          min={1}
          max={3650}
          value={tokenExpiresInDays}
          onCommit={setTokenExpiresInDays}
          ariaLabel={t('users.token_expiry_days')}
        />
        <Button variant="primary" icon={<KeyRound size={16} />} disabled={isPending('create-token')} aria-busy={isPending('create-token')}>
          {t('users.create_token')}
        </Button>
      </form>
      {newToken && (
        <div className="token-once">
          <small className="field-hint">{t('users.token_once_hint')}</small>
          <pre>{newToken}</pre>
          <div className="token-once-actions">
            <Button
              variant="primary"
              icon={<Copy size={16} />}
              onClick={async () => {
                try {
                  await navigator.clipboard.writeText(newToken);
                  setNewToken(null);
                } catch {
                  setError(t('users.token_copy_failed'));
                }
              }}
            >
              {t('users.token_copy')}
            </Button>
            <Button
              variant="secondary"
              icon={<X size={16} />}
              title={t('generic.dismiss')}
              aria-label={t('generic.dismiss')}
              onClick={() => setNewToken(null)}
            />
          </div>
        </div>
      )}
      <div className="table-wrap">
        <table>
          <thead><tr><th>{t('users.col_name')}</th><th>{t('users.col_scopes')}</th><th>{t('users.col_expires')}</th><th>{t('users.col_last_used')}</th><th>{t('users.col_status')}</th><th>{t('users.col_action')}</th></tr></thead>
          <tbody>
            {tokens.map((token) => (
              <tr key={token.id}>
                <td>{token.name}</td>
                <td>{token.scopes.join(', ')}</td>
                <td>{token.expires_at ? formatDateTime(token.expires_at) : '-'}</td>
                <td>{token.last_used_at ? formatDateTime(token.last_used_at) : '-'}</td>
                <td>{token.revoked_at ? t('users.status_revoked') : t('users.status_active')}</td>
                <td>
                  {!token.revoked_at && (
                    <>
                      <Button
                        variant="secondary"
                        icon={<RotateCcw size={16} />}
                        title={t('users.rotate_token')}
                        aria-label={t('users.rotate_token_for', { name: token.name })}
                        disabled={isPending(`token:${token.id}`)}
                        onClick={() => void rotateToken(token)}
                      />
                      <Button
                        variant="secondary"
                        icon={<X size={16} />}
                        title={t('users.revoke_token')}
                        aria-label={t('users.revoke_token_for', { name: token.name })}
                        disabled={isPending(`token:${token.id}`)}
                        onClick={() => void revokeToken(token)}
                      />
                    </>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {confirmDialog}
    </section>
  );
}
