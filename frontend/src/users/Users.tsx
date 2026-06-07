import { useEffect, useState } from 'react';
import { KeyRound, Power, RotateCcw, UserPlus, X } from 'lucide-react';
import { api, ApiToken, Role, SessionItem, UserItem } from '../api/client';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { Button, PageHeader, localizedErrorMessage } from '../lib/ui';

const ALL_ROLES: Role[] = ['viewer', 'reviewer', 'operator', 'auditor', 'admin'];

function splitTags(value: string) {
  return value
    .split(',')
    .map((entry) => entry.trim())
    .filter(Boolean);
}

export function Users({ setError }: { setError: (error: string | null) => void }) {
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
      <PageHeader title={t('users.title')} />
      <form className="compact-form" onSubmit={(event) => {
        event.preventDefault();
        api.createUser({ username, password, roles: [role] }).then(() => {
          setUsername('');
          setPassword('');
          load();
        }).catch((err) => setError(localizedErrorMessage(err, t)));
      }}>
        <input value={username} onChange={(event) => setUsername(event.target.value)} placeholder={t('auth.username')} aria-label={t('auth.username')} />
        <input value={password} onChange={(event) => setPassword(event.target.value)} placeholder={t('auth.password')} type="password" aria-label={t('auth.password')} />
        <select value={role} aria-label={t('users.new_user_role')} onChange={(event) => setRole(event.target.value as Role)}>
          {ALL_ROLES.map((roleOption) => (
            <option key={roleOption} value={roleOption}>{t(`users.role_${roleOption}` as Parameters<TFunction>[0])}</option>
          ))}
        </select>
        <Button variant="primary" icon={<UserPlus size={16} />}>{t('users.create')}</Button>
      </form>
      <div className="table-wrap">
        <table>
          <thead><tr><th>{t('users.col_user')}</th><th>{t('users.col_roles')}</th><th>{t('users.col_status')}</th><th>{t('users.col_password')}</th><th>{t('users.col_actions')}</th></tr></thead>
          <tbody>
            {users.map((user) => (
              <tr key={user.id}>
                <td>{user.username}</td>
                <td>
                  <fieldset className="role-checkboxes" aria-label={t('users.roles_for', { user: user.username })}>
                    {ALL_ROLES.map((roleOption) => {
                      const checked = user.roles.includes(roleOption);
                      return (
                        <label key={roleOption}>
                          <input
                            type="checkbox"
                            checked={checked}
                            aria-label={t('users.role_for', { role: t(`users.role_${roleOption}` as Parameters<TFunction>[0]), user: user.username })}
                            onChange={() => {
                              const nextRoles = checked
                                ? user.roles.filter((existing) => existing !== roleOption)
                                : [...user.roles, roleOption];
                              api.updateUserRoles(user.id, nextRoles).then(load).catch((err) => setError(localizedErrorMessage(err, t)));
                            }}
                          />
                          {t(`users.role_${roleOption}` as Parameters<TFunction>[0])}
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
                    disabled={!resetPasswords[user.id]}
                    onClick={() => api.resetPassword(user.id, resetPasswords[user.id] ?? '').then(() => {
                      setResetPasswords((current) => ({ ...current, [user.id]: '' }));
                      load();
                    }).catch((err) => setError(localizedErrorMessage(err, t)))}
                  />
                </td>
                <td>
                  {user.enabled ? (
                    <Button variant="secondary" icon={<Power size={16} />} title={t('users.disable_user')} aria-label={t('users.disable_user_for', { user: user.username })} onClick={() => api.disableUser(user.id).then(load).catch((err) => setError(localizedErrorMessage(err, t)))}>{t('users.disable')}</Button>
                  ) : (
                    <Button variant="secondary" icon={<Power size={16} />} title={t('users.enable_user')} aria-label={t('users.enable_user_for', { user: user.username })} onClick={() => api.enableUser(user.id).then(load).catch((err) => setError(localizedErrorMessage(err, t)))}>{t('users.enable')}</Button>
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
                  {!session.revoked_at && <Button variant="secondary" icon={<X size={16} />} title={t('users.revoke_session')} aria-label={t('users.revoke_session_for', { user: session.username })} onClick={() => api.revokeSession(session.id).then(load).catch((err) => setError(localizedErrorMessage(err, t)))} />}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <PageHeader title={t('users.api_tokens_title')} />
      <form className="compact-form" onSubmit={(event) => {
        event.preventDefault();
        api.createApiToken({ name: tokenName, scopes: splitTags(tokenScopes), expires_in_days: tokenExpiresInDays }).then((created) => {
          setNewToken(created.token);
          setTokenName('');
          load();
        }).catch((err) => setError(localizedErrorMessage(err, t)));
      }}>
        <input value={tokenName} onChange={(event) => setTokenName(event.target.value)} placeholder={t('users.token_name')} aria-label={t('users.token_name')} />
        <input value={tokenScopes} onChange={(event) => setTokenScopes(event.target.value)} placeholder={t('users.token_scopes_placeholder')} aria-label={t('users.token_scopes')} />
        <input
          type="number"
          min="1"
          max="3650"
          value={tokenExpiresInDays}
          onChange={(event) => setTokenExpiresInDays(Number(event.target.value))}
          aria-label={t('users.token_expiry_days')}
        />
        <Button variant="primary" icon={<KeyRound size={16} />}>{t('users.create_token')}</Button>
      </form>
      {newToken && <pre className="token-once">{newToken}</pre>}
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
                        onClick={() => api.rotateApiToken(token.id, { expires_in_days: tokenExpiresInDays }).then((created) => {
                          setNewToken(created.token);
                          load();
                        }).catch((err) => setError(localizedErrorMessage(err, t)))}
                      />
                      <Button variant="secondary" icon={<X size={16} />} title={t('users.revoke_token')} aria-label={t('users.revoke_token_for', { name: token.name })} onClick={() => api.revokeApiToken(token.id).then(load).catch((err) => setError(localizedErrorMessage(err, t)))} />
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
