import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError, INVALID_RESPONSE_CODE, api, isAbortError, setUnauthorizedHandler } from './client';
import { localizedErrorMessage } from '../lib/ui';
import type { TFunction } from '../i18n/I18nProvider';

const fetchMock = vi.fn();

function respond(status: number, body: string, statusText = '') {
  return Promise.resolve(new Response(status === 204 ? null : body, { status, statusText }));
}

beforeEach(() => {
  fetchMock.mockReset();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
  setUnauthorizedHandler(null);
});

describe('api request error handling (#432)', () => {
  it('throws an ApiError carrying status, backend message and code', async () => {
    fetchMock.mockImplementation(() => respond(409, JSON.stringify({ error: 'already assigned', code: 'conflict' })));
    const err = await api.users().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err).toBeInstanceOf(Error);
    expect((err as ApiError).status).toBe(409);
    expect((err as ApiError).code).toBe('conflict');
    // Message stays the backend text so `err.message` consumers are unchanged.
    expect((err as ApiError).message).toBe('already assigned');
  });

  it('falls back to the status line for a non-JSON error body', async () => {
    fetchMock.mockImplementation(() => respond(502, '<html>bad gateway</html>', 'Bad Gateway'));
    const err = (await api.users().catch((e: unknown) => e)) as ApiError;
    expect(err.status).toBe(502);
    expect(err.code).toBeUndefined();
    expect(err.message).toBe('502 Bad Gateway');
  });

  it('resolves undefined for 204 No Content and empty 200 bodies', async () => {
    fetchMock.mockReturnValueOnce(respond(204, ''));
    await expect(api.revokeApiToken('t1')).resolves.toBeUndefined();
    fetchMock.mockReturnValueOnce(respond(200, ''));
    await expect(api.logout()).resolves.toBeUndefined();
  });

  it('throws a typed ApiError instead of a SyntaxError for a non-JSON 2xx body', async () => {
    fetchMock.mockImplementation(() => respond(200, '<!doctype html><html></html>'));
    const err = (await api.users().catch((e: unknown) => e)) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(err).not.toBeInstanceOf(SyntaxError);
    expect(err.status).toBe(200);
    expect(err.code).toBe(INVALID_RESPONSE_CODE);
  });

  it('treats a 401 on session endpoints as an expired session', async () => {
    const onUnauthorized = vi.fn();
    setUnauthorizedHandler(onUnauthorized);
    fetchMock.mockImplementation(() => respond(401, JSON.stringify({ error: 'unauthorized' })));
    await expect(api.sessions()).rejects.toBeInstanceOf(ApiError);
    await expect(api.users()).rejects.toBeInstanceOf(ApiError);
    expect(onUnauthorized).toHaveBeenCalledTimes(2);
  });

  it('does not log out on a 401 from a credential check (login, password change)', async () => {
    const onUnauthorized = vi.fn();
    setUnauthorizedHandler(onUnauthorized);
    fetchMock.mockImplementation(() => respond(401, JSON.stringify({ error: 'invalid credentials' })));
    await expect(api.login('a', 'b')).rejects.toMatchObject({ status: 401, message: 'invalid credentials' });
    await expect(api.paperlessLogin('a', 'b')).rejects.toMatchObject({ status: 401 });
    await expect(api.changePassword('old', 'new')).rejects.toMatchObject({ status: 401 });
    expect(onUnauthorized).not.toHaveBeenCalled();
  });

  it('passes an AbortSignal through so requests can be cancelled', async () => {
    fetchMock.mockImplementation((_path: string, init: RequestInit) =>
      new Promise((_resolve, reject) => {
        init.signal?.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
      })
    );
    const controller = new AbortController();
    const pending = api.prompts({ signal: controller.signal });
    expect(fetchMock.mock.calls[0][1].signal).toBe(controller.signal);
    controller.abort();
    const err = await pending.catch((e: unknown) => e);
    expect(isAbortError(err)).toBe(true);
  });
});

describe('localizedErrorMessage classifies by error type (#432)', () => {
  const t = ((key: string) => `[${key}]`) as TFunction;

  it('maps 401/403 ApiErrors to the unauthorized hint', () => {
    expect(localizedErrorMessage(new ApiError('nope', 403), t)).toBe('[generic.unauthorized] nope');
  });

  it('maps gateway timeouts and fetch TypeErrors', () => {
    expect(localizedErrorMessage(new ApiError('slow', 504), t)).toBe('[generic.timeout] slow');
    expect(localizedErrorMessage(new TypeError('Failed to fetch'), t)).toBe('[generic.network_error] Failed to fetch');
  });

  it('does not guess the type from message substrings', () => {
    // A validation error that merely mentions "connect" or "403" is not a
    // network or permission failure.
    expect(localizedErrorMessage(new ApiError('cannot connect provider 403-b', 400), t)).toBe('cannot connect provider 403-b');
    expect(localizedErrorMessage(new ApiError('', 500), t)).toBe('[generic.request_failed]');
  });
});
