import { useEffect, useState } from 'react';
import { KeyRound, Power, RotateCcw, UserPlus, X } from 'lucide-react';
import { api, ApiToken, Role, SessionItem, UserItem } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { PageHeader, localizedErrorMessage } from '../lib/ui';

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
