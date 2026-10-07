/**
 * Tests for the authorization_code grant path in ServiceNowClient._getOAuthToken().
 *
 * Per-user sign-in with a persisted refresh token. Critically, when the stored
 * refresh token is rejected the client must fail loud and re-prompt the browser
 * sign-in — it must NEVER silently fall back to a password/client_credentials
 * grant (that would re-introduce shared-principal attribution). The auth-code
 * flow and the refresh HTTP POST are injected so the path is testable offline.
 */
import axios from 'axios';
import { afterAll, beforeAll, jest } from '@jest/globals';
import { userInfo } from 'node:os';
import { oauthRefreshTokenAccount, ServiceNowClient, StaleInstanceError } from '../src/servicenow-client.js';
import { InMemoryTokenStore } from '../src/token-store.js';

const DEFAULT_ACCOUNT = oauthRefreshTokenAccount({ url: 'https://ex.service-now.com', clientId: 'cid' });
const NEW_ACCOUNT = oauthRefreshTokenAccount({ url: 'https://new.service-now.com', clientId: 'new-cid' });

// Several grant tests use an external IdP; approve it the way an operator would.
let savedTrustedOrigins;
beforeAll(() => {
  savedTrustedOrigins = process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS;
  process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS = 'https://oauth.example.com';
});
afterAll(() => {
  if (savedTrustedOrigins === undefined) delete process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS;
  else process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS = savedTrustedOrigins;
});

function makeClient({ store, flow, postToken, clientSecret } = {}) {
  return new ServiceNowClient('https://ex.service-now.com', null, null, {
    authType: 'oauth',
    grantType: 'authorization_code',
    clientId: 'cid',
    clientSecret,
    scope: 'useraccount',
    tokenStore: store,
    performAuthCodeFlow: flow,
    postToken
  });
}

function serializedError(error) {
  return JSON.stringify({
    ...error,
    message: error.message,
    stack: error.stack
  });
}

function makeGrantClient({
  username = null,
  password = null,
  clientSecret,
  credentialRef,
  credentialStore,
  grantType = 'client_credentials',
  tokenUrl
} = {}) {
  return new ServiceNowClient('https://ex.service-now.com', username, password, {
    authType: 'oauth',
    grantType,
    clientId: 'cid',
    clientSecret,
    credentialRef,
    credentialStore,
    scope: 'useraccount',
    tokenUrl
  });
}
test('exports a secret-safe stale-instance error with a stable dedicated code', () => {
  const error = new StaleInstanceError();

  expect(error).toBeInstanceOf(Error);
  expect(error.name).toBe('StaleInstanceError');
  expect(error.code).toBe('INSTANCE_STALE_DURING_REQUEST');
  expect(JSON.stringify(error)).not.toContain('refresh-secret');
});


afterEach(() => {
  jest.restoreAllMocks();
});

describe('ServiceNowClient authorization_code grant', () => {
  it('runs the interactive flow when no refresh token is stored, and persists the new refresh token', async () => {
    const store = new InMemoryTokenStore();
    let calls = 0;
    const flow = async () => { calls++; return { access_token: 'at1', refresh_token: 'rt1', expires_in: 1800 }; };
    const client = makeClient({ store, flow });

    const token = await client._getOAuthToken();

    expect(token).toBe('at1');
    expect(calls).toBe(1);
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe('rt1');
  });

  it('binds persisted refresh tokens to the OAuth identity, not the instance name', async () => {
    const store = new InMemoryTokenStore();
    const client = makeClient({
      store,
      flow: async () => ({ access_token: 'at1', refresh_token: 'rt1', expires_in: 1800 })
    });
    client.currentInstanceName = 'dev';

    await client._getOAuthToken();

    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe('rt1');
    expect(await store.getRefreshToken(`${userInfo().username}@dev`)).toBeNull();
    expect(await store.getRefreshToken('dev')).toBeNull();
  });

  it('does not re-run the flow while the cached access token is still valid', async () => {
    const store = new InMemoryTokenStore();
    let calls = 0;
    const flow = async () => { calls++; return { access_token: 'at1', refresh_token: 'rt1', expires_in: 1800 }; };
    const client = makeClient({ store, flow });

    await client._getOAuthToken();
    await client._getOAuthToken();

    expect(calls).toBe(1);
  });

  it('refreshes from a stored refresh token without prompting the browser', async () => {
    const store = new InMemoryTokenStore();
    await store.setRefreshToken(DEFAULT_ACCOUNT, 'rt-stored');
    let flowCalls = 0;
    const flow = async () => { flowCalls++; return { access_token: 'fromflow' }; };
    const postToken = async (_url, params) => {
      expect(params.grant_type).toBe('refresh_token');
      expect(params.refresh_token).toBe('rt-stored');
      return { access_token: 'at-refreshed', refresh_token: 'rt-new', expires_in: 1800 };
    };
    const client = makeClient({ store, flow, postToken });

    const token = await client._getOAuthToken();

    expect(token).toBe('at-refreshed');
    expect(flowCalls).toBe(0);
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe('rt-new');
  });

  it('redacts OAuth and basic credential material from interactive auth errors', async () => {
    const accessToken = 'access-token-secret';
    const refreshToken = 'refresh-token-secret';
    const password = 'basic-password-secret';
    const clientSecret = 'client-secret-secret';
    const basicAuth = Buffer.from(`user:${password}`).toString('base64');
    const flow = async () => {
      const error = new Error(
        `token endpoint failed ${accessToken} ${refreshToken} ${password} ${clientSecret} `
        + `Bearer ${accessToken} Basic ${basicAuth}`
      );
      error.response = {
        data: {
          access_token: accessToken,
          refresh_token: refreshToken,
          authorization: `Bearer ${accessToken}`,
          basic: `Basic ${basicAuth}`,
          client_secret: clientSecret,
          password
        }
      };
      throw error;
    };
    const client = makeClient({ flow, clientSecret });
    client.oauthToken = accessToken;
    client.oauthRefreshToken = refreshToken;
    client.password = password;
    client.auth = basicAuth;
    client.username = 'user';

    let thrown;
    try {
      await client._getOAuthToken();
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(Error);
    const serialized = serializedError(thrown);
    for (const secret of [accessToken, refreshToken, password, clientSecret, basicAuth]) {
      expect(serialized).not.toContain(secret);
    }
    expect(serialized).not.toContain(`Bearer ${accessToken}`);
    expect(serialized).not.toContain(`Basic ${basicAuth}`);
  });

  it('redacts newly issued token response credentials before authorization-code caching', async () => {
    const secrets = {
      accessToken: 'new-access-token-fixture',
      refreshToken: 'new-refresh-token-fixture',
      password: 'response-password-fixture',
      clientSecret: 'response-client-secret-fixture',
      authorization: 'Bearer response-authorization-fixture'
    };
    const flow = async () => {
      const error = new Error(Object.values(secrets).join(' '));
      error.stack = `AuthCodeFailure: ${Object.values(secrets).join(' ')}`;
      error.details = { password: secrets.password, note: secrets.accessToken };
      error.logs = [`authorization: ${secrets.authorization}`, secrets.refreshToken];
      error.config = {
        data: {
          client_secret: secrets.clientSecret,
          password: secrets.password,
          Authorization: secrets.authorization
        }
      };
      error.response = {
        status: 500,
        data: {
          access_token: secrets.accessToken,
          refresh_token: secrets.refreshToken
        }
      };
      throw error;
    };
    const store = new InMemoryTokenStore();
    const client = makeClient({ store, flow });

    const error = await client._getOAuthToken().catch(thrown => thrown);
    const serialized = serializedError(error);

    for (const secret of Object.values(secrets)) {
      expect(serialized).not.toContain(secret);
    }
    expect(client.oauthToken).toBeNull();
    expect(client.oauthRefreshToken).toBeNull();
    expect(client.oauthTokenExpiry).toBeNull();
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBeNull();
  });

  it('redacts OAuth and basic credential material from refresh endpoint errors', async () => {
    const accessToken = 'refresh-access-secret';
    const refreshToken = 'refresh-token-secret';
    const password = 'refresh-basic-password';
    const basicAuth = Buffer.from(`user:${password}`).toString('base64');
    const store = new InMemoryTokenStore();
    await store.setRefreshToken(DEFAULT_ACCOUNT, refreshToken);
    const postToken = async () => {
      const error = new Error(
        `refresh endpoint failed ${accessToken} ${refreshToken} ${password} `
        + `Bearer ${accessToken} Basic ${basicAuth} `
        + `authorization: bearer ${accessToken} Authorization: basic ${basicAuth}`
      );
      error.response = {
        status: 500,
        data: {
          access_token: accessToken,
          refresh_token: refreshToken,
          authorization: `Bearer ${accessToken}`,
          basic: `Basic ${basicAuth}`,
          password
        }
      };
      throw error;
    };
    const client = makeClient({ store, flow: async () => ({ access_token: 'unexpected' }), postToken });
    client.oauthToken = accessToken;
    client.oauthRefreshToken = refreshToken;
    client.password = password;
    client.auth = basicAuth;
    client.username = 'user';

    let thrown;
    try {
      await client._getOAuthToken();
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(Error);
    const serialized = serializedError(thrown);
    for (const secret of [accessToken, refreshToken, password, basicAuth]) {
      expect(serialized).not.toContain(secret);
    }
    expect(serialized).not.toContain(`Bearer ${accessToken}`);
    expect(serialized).not.toContain(`Basic ${basicAuth}`);
    expect(serialized).not.toContain(`authorization: bearer ${accessToken}`);
    expect(serialized).not.toContain(`Authorization: basic ${basicAuth}`);
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe(refreshToken);
  });

  it('re-runs the interactive flow (never password) when the server REJECTS the refresh token (400/invalid_grant)', async () => {
    const store = new InMemoryTokenStore();
    await store.setRefreshToken(DEFAULT_ACCOUNT, 'rt-expired');
    let flowCalls = 0;
    const flow = async () => { flowCalls++; return { access_token: 'at-reauth', refresh_token: 'rt-fresh', expires_in: 1800 }; };
    const postToken = async () => {
      const err = new Error('invalid_grant');
      err.response = { status: 400, data: { error: 'invalid_grant' } };
      throw err;
    };
    const client = makeClient({ store, flow, postToken });

    const token = await client._getOAuthToken();

    expect(token).toBe('at-reauth');
    expect(flowCalls).toBe(1);
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe('rt-fresh');
  });
  it('wraps a refresh-token clear failure without exposing backend details', async () => {
    const secret = 'clear-refresh-secret';
    const clearError = new Error(`keychain locked ${secret}`);
    const store = {
      getRefreshToken: jest.fn(async () => 'rt-expired'),
      clearRefreshToken: jest.fn(async () => { throw clearError; }),
      setRefreshToken: jest.fn()
    };
    const flow = jest.fn(async () => ({ access_token: 'unexpected-browser-token' }));
    const postToken = jest.fn(async () => {
      const error = new Error('invalid_grant');
      error.response = { status: 400, data: { error: 'invalid_grant' } };
      throw error;
    });
    const client = makeClient({ store, flow, postToken });

    const error = await client._getOAuthToken().catch(thrown => thrown);
    expect(error).toMatchObject({ code: 'KEYCHAIN_OPERATION_FAILED' });
    expect(JSON.stringify(error)).not.toContain(secret);
    expect(error.message).toBe('Credential store operation failed');

    expect(store.clearRefreshToken).toHaveBeenCalledWith(DEFAULT_ACCOUNT);
    expect(flow).not.toHaveBeenCalled();
    expect(client.oauthRefreshToken).toBeNull();
  });

  it('wraps a refresh-token read failure without exposing backend details or populating cache', async () => {
    const secret = 'read-refresh-secret';
    const store = {
      getRefreshToken: jest.fn(async () => { throw new Error(`keychain read failed ${secret}`); }),
      clearRefreshToken: jest.fn(),
      setRefreshToken: jest.fn()
    };
    const flow = jest.fn(async () => ({ access_token: 'unexpected-browser-token' }));
    const client = makeClient({ store, flow });

    const error = await client._getOAuthToken().catch(thrown => thrown);
    expect(error).toMatchObject({ code: 'KEYCHAIN_OPERATION_FAILED' });
    expect(JSON.stringify(error)).not.toContain(secret);
    expect(error.message).toBe('Credential store operation failed');
    expect(flow).not.toHaveBeenCalled();
    expect(client.oauthToken).toBeNull();
    expect(client.oauthRefreshToken).toBeNull();
    expect(client.oauthTokenExpiry).toBeNull();
  });


  it.each([
    [400, 'invalid_client'],
    [401, 'invalid_request']
  ])('preserves the refresh token and does not re-authenticate for %s %s', async (status, oauthError) => {
    const store = new InMemoryTokenStore();
    const storedRefreshToken = `stored-${oauthError}`;
    await store.setRefreshToken(DEFAULT_ACCOUNT, storedRefreshToken);
    let flowCalls = 0;
    const flow = async () => {
      flowCalls++;
      return { access_token: 'unexpected' };
    };
    const postToken = async () => {
      const error = new Error(oauthError);
      error.response = { status, data: { error: oauthError } };
      throw error;
    };
    const client = makeClient({ store, flow, postToken });

    await expect(client._getOAuthToken()).rejects.toThrow(oauthError);
    expect(flowCalls).toBe(0);
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe(storedRefreshToken);
    expect(client.oauthRefreshToken).toBe(storedRefreshToken);
  });

  it('does NOT discard the refresh token or re-auth on a transient error (network / 5xx)', async () => {
    const store = new InMemoryTokenStore();
    await store.setRefreshToken(DEFAULT_ACCOUNT, 'rt-good');
    let flowCalls = 0;
    const flow = async () => { flowCalls++; return { access_token: 'should-not-happen' }; };
    const postToken = async () => { throw new Error('ECONNREFUSED'); }; // no .response → transient
    const client = makeClient({ store, flow, postToken });

    await expect(client._getOAuthToken()).rejects.toThrow(/ECONNREFUSED/);
    expect(flowCalls).toBe(0);
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe('rt-good'); // token preserved
  });
  it.each([
    ['network', Object.assign(new Error('socket timeout refresh-secret'), { code: 'ECONNRESET' })],
    ['timeout', Object.assign(new Error('request timed out refresh-secret'), { code: 'ETIMEDOUT' })],
    ['5xx', Object.assign(new Error('upstream failure refresh-secret'), {
      response: { status: 503, data: { error: 'temporarily_unavailable', refresh_token: 'refresh-secret' } }
    })],
    ['malformed OAuth error', Object.assign(new Error('malformed response refresh-secret'), {
      response: { status: 400, data: { refresh_token: 'refresh-secret' } }
    })],
    ['invalid_client', Object.assign(new Error('invalid client refresh-secret'), {
      response: { status: 400, data: { error: 'invalid_client', refresh_token: 'refresh-secret' } }
    })],
    ['invalid_request', Object.assign(new Error('invalid request refresh-secret'), {
      response: { status: 401, data: { error: 'invalid_request', refresh_token: 'refresh-secret' } }
    })]
  ])('preserves a non-authorization-code refresh token and does not fall back after %s', async (_kind, refreshError) => {
    const tokenPost = jest.spyOn(axios, 'post')
      .mockRejectedValueOnce(refreshError)
      .mockResolvedValueOnce({ data: { access_token: 'unexpected-fallback' } });
    const client = makeGrantClient({ clientSecret: 'client-secret' });
    client.oauthRefreshToken = 'refresh-secret';

    let thrown;
    try {
      await client._getOAuthToken();
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(Error);
    expect(tokenPost).toHaveBeenCalledTimes(1);
    expect(client.oauthRefreshToken).toBe('refresh-secret');
    const serialized = JSON.stringify({
      message: thrown.message,
      code: thrown.code,
      status: thrown.status
    });
    expect(serialized).not.toContain('refresh-secret');
  });

  it('clears a non-authorization-code invalid_grant refresh and uses the configured primary grant', async () => {
    const tokenPost = jest.spyOn(axios, 'post')
      .mockRejectedValueOnce(Object.assign(new Error('invalid_grant refresh-secret'), {
        response: { status: 400, data: { error: 'invalid_grant', refresh_token: 'refresh-secret' } }
      }))
      .mockResolvedValueOnce({ data: { access_token: 'primary-token', expires_in: 1800 } });
    const client = makeGrantClient({ clientSecret: 'client-secret', grantType: 'client_credentials' });
    client.oauthRefreshToken = 'refresh-secret';

    await expect(client._getOAuthToken()).resolves.toBe('primary-token');

    expect(tokenPost).toHaveBeenCalledTimes(2);
    expect(client.oauthRefreshToken).toBeNull();
    const primaryParams = Object.fromEntries(new URLSearchParams(tokenPost.mock.calls[1][1]));
    expect(primaryParams.grant_type).toBe('client_credentials');
  });


  it('does not re-persist a stale refresh token when a refresh response omits refresh_token (rotation)', async () => {
    let setCalls = 0;
    const store = new InMemoryTokenStore();
    const countingStore = {
      getRefreshToken: (a) => store.getRefreshToken(a),
      setRefreshToken: (a, t) => { setCalls++; return store.setRefreshToken(a, t); },
      clearRefreshToken: (a) => store.clearRefreshToken(a)
    };
    await store.setRefreshToken(DEFAULT_ACCOUNT, 'rt-old');
    const postToken = async () => ({ access_token: 'at-refreshed', expires_in: 1800 }); // no refresh_token
    const client = makeClient({ store: countingStore, flow: async () => ({}), postToken });

    const token = await client._getOAuthToken();

    expect(token).toBe('at-refreshed');
    expect(setCalls).toBe(0); // nothing new to persist; old token left untouched
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe('rt-old');
  });

  it('does not re-persist when a refresh response echoes the SAME refresh token', async () => {
    // ServiceNow commonly returns the existing (unrotated) refresh token on every
    // refresh. Re-writing an identical value is a logical no-op, but some OS keychain
    // backends recreate the item on write (macOS resets its ACL), so an unchanged
    // token must NOT be written.
    let setCalls = 0;
    const store = new InMemoryTokenStore();
    const countingStore = {
      getRefreshToken: (a) => store.getRefreshToken(a),
      setRefreshToken: (a, t) => { setCalls++; return store.setRefreshToken(a, t); },
      clearRefreshToken: (a) => store.clearRefreshToken(a)
    };
    await store.setRefreshToken(DEFAULT_ACCOUNT, 'rt-same');
    const postToken = async () => ({ access_token: 'at-refreshed', refresh_token: 'rt-same', expires_in: 1800 });
    const client = makeClient({ store: countingStore, flow: async () => ({}), postToken });

    const token = await client._getOAuthToken();

    expect(token).toBe('at-refreshed');
    expect(setCalls).toBe(0); // identical value → no keychain rewrite
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe('rt-same');
  });

  it('DOES persist when a refresh response returns a genuinely rotated refresh token', async () => {
    let setCalls = 0;
    const store = new InMemoryTokenStore();
    const countingStore = {
      getRefreshToken: (a) => store.getRefreshToken(a),
      setRefreshToken: (a, t) => { setCalls++; return store.setRefreshToken(a, t); },
      clearRefreshToken: (a) => store.clearRefreshToken(a)
    };
    await store.setRefreshToken(DEFAULT_ACCOUNT, 'rt-old');
    const postToken = async () => ({ access_token: 'at-refreshed', refresh_token: 'rt-rotated', expires_in: 1800 });
    const client = makeClient({ store: countingStore, flow: async () => ({}), postToken });

    await client._getOAuthToken();

    expect(setCalls).toBe(1); // changed value → exactly one write
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe('rt-rotated');
  });
  it('does not cache an access token when refresh-token persistence fails, then retries the flow', async () => {
    let rejectPersistence = true;
    let storedRefreshToken = null;
    const persistenceError = new Error('keychain rejected refresh-token-secret');
    const store = {
      getRefreshToken: jest.fn(async () => storedRefreshToken),
      setRefreshToken: jest.fn(async (_account, token) => {
        if (rejectPersistence) {
          rejectPersistence = false;
          throw persistenceError;
        }
        storedRefreshToken = token;
      }),
      clearRefreshToken: jest.fn()
    };
    const flow = jest.fn()
      .mockResolvedValueOnce({
        access_token: 'unpersisted-access-secret',
        refresh_token: 'refresh-token-secret',
        expires_in: 1800
      })
      .mockResolvedValueOnce({
        access_token: 'retry-access-token',
        refresh_token: 'retry-refresh-token',
        expires_in: 1800
      });
    const client = makeClient({ store, flow });

    const firstError = await client._getOAuthToken().catch(error => error);

    expect(firstError).toMatchObject({ code: 'KEYCHAIN_OPERATION_FAILED' });
    expect(JSON.stringify(firstError)).not.toContain('refresh-token-secret');
    expect(client.oauthToken).toBeNull();
    expect(client.oauthRefreshToken).toBeNull();
    expect(client.oauthTokenExpiry).toBeNull();

    await expect(client._getOAuthToken()).resolves.toBe('retry-access-token');
    expect(flow).toHaveBeenCalledTimes(2);
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBe('retry-refresh-token');
  });

  it('rejects malformed authorization-code token responses without mutating the cache', async () => {
    const store = new InMemoryTokenStore();
    const client = makeClient({
      store,
      flow: async () => ({
        error: 'invalid_grant',
        error_description: 'response contains auth-response-secret'
      })
    });

    const error = await client._getOAuthToken().catch(thrown => thrown);

    expect(error).toMatchObject({
      name: 'AuthenticationError',
      code: 'OAUTH_TOKEN_RESPONSE_INVALID'
    });
    expect(error.message).not.toContain('auth-response-secret');
    expect(JSON.stringify(error)).not.toContain('auth-response-secret');
    expect(client.oauthToken).toBeNull();
    expect(client.oauthRefreshToken).toBeNull();
    expect(client.oauthTokenExpiry).toBeNull();
    expect(await store.getRefreshToken(DEFAULT_ACCOUNT)).toBeNull();
  });

  it('rejects malformed normal token responses without creating an undefined bearer token', async () => {
    const tokenPost = jest.spyOn(axios, 'post').mockResolvedValue({
      data: {
        error: 'invalid_client',
        error_description: 'response contains token-response-secret'
      }
    });
    const client = makeGrantClient({ clientSecret: 'client-secret-fixture' });

    const error = await client.getAuthHeader().catch(thrown => thrown);

    expect(error).toMatchObject({
      name: 'AuthenticationError',
      code: 'OAUTH_TOKEN_RESPONSE_INVALID'
    });
    expect(error.message).not.toContain('token-response-secret');
    expect(JSON.stringify(error)).not.toContain('token-response-secret');
    expect(client.oauthToken).toBeNull();
    expect(client.oauthRefreshToken).toBeNull();
    expect(client.oauthTokenExpiry).toBeNull();
    expect(tokenPost).toHaveBeenCalledTimes(1);
  });

  it.each([
    ['empty access token', { access_token: '   ' }],
    ['non-string refresh token', { access_token: 'access', refresh_token: 42 }],
    ['non-positive expiry', { access_token: 'access', expires_in: 0 }],
    ['non-finite expiry', { access_token: 'access', expires_in: Infinity }]
  ])('rejects token responses with %s before cache mutation', async (_caseName, response) => {
    const client = makeGrantClient({ clientSecret: 'client-secret-fixture' });
    const tokenPost = jest.spyOn(axios, 'post').mockResolvedValue({ data: response });

    await expect(client._getOAuthToken()).rejects.toMatchObject({
      code: 'OAUTH_TOKEN_RESPONSE_INVALID'
    });
    expect(client.oauthToken).toBeNull();
    expect(client.oauthRefreshToken).toBeNull();
    expect(client.oauthTokenExpiry).toBeNull();
    expect(tokenPost).toHaveBeenCalledTimes(1);
  });
});
  it('rejects a stale authorization flow without populating the switched instance', async () => {
    let releaseFlow;
    const flowPending = new Promise(resolve => { releaseFlow = resolve; });
    const oldStore = new InMemoryTokenStore();
    const client = makeClient({
      store: oldStore,
      flow: () => flowPending
    });
    const oldToken = client._getOAuthToken();
    const newStore = new InMemoryTokenStore();

    client.setInstance('https://new.service-now.com', null, null, 'new', {
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'new-cid',
      tokenStore: newStore,
      performAuthCodeFlow: async () => ({ access_token: 'new-token' })
    });
    releaseFlow({ access_token: 'old-token', refresh_token: 'old-refresh', expires_in: 1800 });

    await expect(oldToken).rejects.toBeInstanceOf(StaleInstanceError);
    expect(client.oauthToken).toBeNull();
    expect(client.oauthRefreshToken).toBeNull();
    expect(client.oauthTokenExpiry).toBeNull();
    expect(await oldStore.getRefreshToken(NEW_ACCOUNT)).toBeNull();
    expect(await newStore.getRefreshToken(NEW_ACCOUNT)).toBeNull();
  });

  it('rejects when a stale refresh-token read resolves after instance switch', async () => {
    let releaseRead;
    const readPending = new Promise(resolve => { releaseRead = resolve; });
    const oldStore = {
      getRefreshToken: jest.fn(() => readPending),
      setRefreshToken: jest.fn(),
      clearRefreshToken: jest.fn()
    };
    const client = makeClient({
      store: oldStore,
      flow: async () => ({ access_token: 'old-token' })
    });
    const oldToken = client._getOAuthToken();
    const newStore = new InMemoryTokenStore();

    client.setInstance('https://new.service-now.com', null, null, 'new', {
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'new-cid',
      tokenStore: newStore,
      performAuthCodeFlow: async () => ({ access_token: 'new-token' })
    });
    releaseRead('old-refresh');

    await expect(oldToken).rejects.toBeInstanceOf(StaleInstanceError);
    expect(client.oauthToken).toBeNull();
    expect(client.oauthRefreshToken).toBeNull();
    expect(oldStore.setRefreshToken).not.toHaveBeenCalled();
    expect(await newStore.getRefreshToken(NEW_ACCOUNT)).toBeNull();
  });

  it('rejects when a stale refresh-token write settles after instance switch', async () => {
    let releaseWrite;
    let writeStarted;
    const writeStartedPromise = new Promise(resolve => { writeStarted = resolve; });
    const writePending = new Promise(resolve => { releaseWrite = resolve; });
    const oldStore = {
      getRefreshToken: jest.fn(async () => null),
      setRefreshToken: jest.fn(async () => {
        writeStarted();
        await writePending;
      }),
      clearRefreshToken: jest.fn()
    };
    const client = makeClient({
      store: oldStore,
      flow: async () => ({ access_token: 'old-token', refresh_token: 'old-refresh', expires_in: 1800 })
    });
    const oldToken = client._getOAuthToken();
    await writeStartedPromise;
    const newStore = {
      getRefreshToken: jest.fn(async () => null),
      setRefreshToken: jest.fn(),
      clearRefreshToken: jest.fn()
    };

    client.setInstance('https://new.service-now.com', null, null, 'new', {
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'new-cid',
      tokenStore: newStore,
      performAuthCodeFlow: async () => ({ access_token: 'new-token' })
    });
    releaseWrite();

    await expect(oldToken).rejects.toBeInstanceOf(StaleInstanceError);
    expect(client.oauthToken).toBeNull();
    expect(client.oauthRefreshToken).toBeNull();
    expect(newStore.setRefreshToken).not.toHaveBeenCalled();
  });


  it('rejects a stale refresh POST before inspecting invalid_grant or clearing the new instance', async () => {
    let rejectRefresh;
    let markPostStarted;
    const postStarted = new Promise(resolve => { markPostStarted = resolve; });
    const refreshPending = new Promise((resolve, reject) => { rejectRefresh = reject; });
    const oldStore = {
      getRefreshToken: jest.fn(async () => 'old-refresh-secret'),
      setRefreshToken: jest.fn(),
      clearRefreshToken: jest.fn()
    };
    const postToken = jest.fn(() => {
      markPostStarted();
      return refreshPending;
    });
    const client = makeClient({
      store: oldStore,
      clientSecret: 'old-client-secret',
      flow: async () => ({ access_token: 'unexpected' }),
      postToken
    });
    const consoleError = jest.spyOn(console, 'error').mockImplementation(() => {});

    const oldRequest = client._getOAuthToken();
    await postStarted;

    const newStore = {
      getRefreshToken: jest.fn(async () => null),
      setRefreshToken: jest.fn(),
      clearRefreshToken: jest.fn()
    };
    client.setInstance('https://new.service-now.com', null, null, 'new', {
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'new-cid',
      clientSecret: 'new-client-secret',
      tokenStore: newStore,
      performAuthCodeFlow: async () => ({ access_token: 'unexpected' })
    });
    client.oauthToken = 'new-access-secret';
    client.oauthRefreshToken = 'new-refresh-secret';
    client.oauthTokenExpiry = 1234567890;

    const refreshError = new Error(
      'old refresh failed old-refresh-secret old-client-secret new-refresh-secret new-client-secret'
    );
    refreshError.response = {
      status: 400,
      data: {
        error: 'invalid_grant',
        access_token: 'old-access-secret',
        refresh_token: 'old-refresh-secret',
        client_secret: 'new-client-secret'
      }
    };
    rejectRefresh(refreshError);

    let thrown;
    try {
      await oldRequest;
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toBeInstanceOf(StaleInstanceError);
    const serialized = serializedError(thrown);
    for (const secret of [
      'old-access-secret',
      'old-refresh-secret',
      'old-client-secret',
      'new-access-secret',
      'new-refresh-secret',
      'new-client-secret'
    ]) {
      expect(serialized).not.toContain(secret);
    }
    expect(client.oauthToken).toBe('new-access-secret');
    expect(client.oauthRefreshToken).toBe('new-refresh-secret');
    expect(client.oauthTokenExpiry).toBe(1234567890);
    expect(oldStore.clearRefreshToken).not.toHaveBeenCalled();
    expect(oldStore.setRefreshToken).not.toHaveBeenCalled();
    expect(newStore.clearRefreshToken).not.toHaveBeenCalled();
    expect(newStore.setRefreshToken).not.toHaveBeenCalled();
    expect(consoleError).not.toHaveBeenCalled();
    consoleError.mockRestore();

  });

  it('rejects a stale interactive flow before redacting or touching the switched instance', async () => {
    let rejectFlow;
    let markFlowStarted;
    const flowStarted = new Promise(resolve => { markFlowStarted = resolve; });
    const flowPending = new Promise((resolve, reject) => { rejectFlow = reject; });
    const oldStore = {
      getRefreshToken: jest.fn(async () => null),
      setRefreshToken: jest.fn(),
      clearRefreshToken: jest.fn()
    };
    const flow = jest.fn(() => {
      markFlowStarted();
      return flowPending;
    });
    const client = makeClient({
      store: oldStore,
      clientSecret: 'old-client-secret',
      flow
    });
    const oldRequest = client._getOAuthToken();
    await flowStarted;

    const newStore = {
      getRefreshToken: jest.fn(async () => null),
      setRefreshToken: jest.fn(),
      clearRefreshToken: jest.fn()
    };
    client.setInstance('https://new.service-now.com', null, null, 'new', {
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'new-cid',
      clientSecret: 'new-client-secret',
      tokenStore: newStore,
      performAuthCodeFlow: async () => ({ access_token: 'unexpected' })
    });
    client.oauthToken = 'new-access-secret';
    client.oauthRefreshToken = 'new-refresh-secret';
    client.oauthTokenExpiry = 9876543210;

    const flowError = new Error(
      'old interactive failure old-access-secret old-refresh-secret old-client-secret '
      + 'new-access-secret new-refresh-secret new-client-secret'
    );
    flowError.response = {
      status: 500,
      data: {
        access_token: 'old-access-secret',
        refresh_token: 'old-refresh-secret',
        client_secret: 'new-client-secret'
      }
    };
    rejectFlow(flowError);

    let thrown;
    try {
      await oldRequest;
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toBeInstanceOf(StaleInstanceError);
    const serialized = serializedError(thrown);
    for (const secret of [
      'old-access-secret',
      'old-refresh-secret',
      'old-client-secret',
      'new-access-secret',
      'new-refresh-secret',
      'new-client-secret'
    ]) {
      expect(serialized).not.toContain(secret);
    }
    expect(client.oauthToken).toBe('new-access-secret');
    expect(client.oauthRefreshToken).toBe('new-refresh-secret');
    expect(client.oauthTokenExpiry).toBe(9876543210);
    expect(oldStore.clearRefreshToken).not.toHaveBeenCalled();
    expect(oldStore.setRefreshToken).not.toHaveBeenCalled();
    expect(newStore.clearRefreshToken).not.toHaveBeenCalled();
    expect(newStore.setRefreshToken).not.toHaveBeenCalled();
  });

describe('ServiceNowClient OAuth registered credentials', () => {
  afterEach(() => {
    jest.restoreAllMocks();
  });

  test('resolves a client-credentials secret once and deduplicates simultaneous token/auth requests', async () => {
    let releaseSecret;
    const secretPending = new Promise(resolve => { releaseSecret = resolve; });
    const credentialStore = {
      getSecret: jest.fn(() => secretPending)
    };
    const tokenPost = jest.spyOn(axios, 'post').mockResolvedValue({
      data: { access_token: 'cc-token', expires_in: 1800 }
    });
    const client = makeGrantClient({
      credentialRef: 'keychain:instance/prod/client-secret',
      credentialStore,
      tokenUrl: 'https://oauth.example.com/client-credentials-token'
    });

    const token = client._getOAuthToken();
    const header = client.getAuthHeader();
    releaseSecret('client-secret-fixture');

    await expect(Promise.all([token, header])).resolves.toEqual([
      'cc-token',
      'Bearer cc-token'
    ]);
    expect(credentialStore.getSecret).toHaveBeenCalledTimes(1);
    expect(tokenPost.mock.calls[0][0]).toBe('https://oauth.example.com/client-credentials-token');
    const params = Object.fromEntries(new URLSearchParams(tokenPost.mock.calls[0][1]));
    expect(params).toMatchObject({
      grant_type: 'client_credentials',
      client_id: 'cid',
      client_secret: 'client-secret-fixture'
    });
  });

  test('resolves password-grant password and client secret references before one token request', async () => {
    const values = new Map([
      ['keychain:instance/dev/password', 'password-fixture'],
      ['keychain:instance/dev/client-secret', 'client-secret-fixture']
    ]);
    const credentialStore = {
      getSecret: jest.fn(async ref => values.get(ref))
    };
    const tokenPost = jest.spyOn(axios, 'post').mockResolvedValue({
      data: { access_token: 'password-token', expires_in: 1800 }
    });
    const client = makeGrantClient({
      username: 'dev-user',
      credentialRef: {
        password: 'keychain:instance/dev/password',
        clientSecret: 'keychain:instance/dev/client-secret'
      },
      credentialStore,
      grantType: 'password',
      tokenUrl: 'https://oauth.example.com/password-token'
    });

    const [token, header] = await Promise.all([
      client._getOAuthToken(),
      client.getAuthHeader()
    ]);

    expect(token).toBe('password-token');
    expect(header).toBe('Bearer password-token');
    expect(credentialStore.getSecret).toHaveBeenCalledTimes(2);
    expect(credentialStore.getSecret).toHaveBeenCalledWith('keychain:instance/dev/password');
    expect(credentialStore.getSecret).toHaveBeenCalledWith('keychain:instance/dev/client-secret');
    expect(tokenPost.mock.calls[0][0]).toBe('https://oauth.example.com/password-token');
    const params = Object.fromEntries(new URLSearchParams(tokenPost.mock.calls[0][1]));
    expect(params).toMatchObject({
      grant_type: 'password',
      client_id: 'cid',
      client_secret: 'client-secret-fixture',
      username: 'dev-user',
      password: 'password-fixture'
    });
  });

  test('does not look up a credential for a public authorization-code client', async () => {
    const credentialStore = { getSecret: jest.fn() };
    const client = new ServiceNowClient('https://ex.service-now.com', null, null, {
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'public-client',
      credentialStore,
      tokenStore: new InMemoryTokenStore(),
      performAuthCodeFlow: async () => ({ access_token: 'public-token', expires_in: 1800 })
    });

    await expect(client.getAuthHeader()).resolves.toBe('Bearer public-token');
    expect(credentialStore.getSecret).not.toHaveBeenCalled();
  });

  test('preserves legacy OAuth secrets without consulting the credential store', async () => {
    const credentialStore = { getSecret: jest.fn() };
    const tokenPost = jest.spyOn(axios, 'post').mockResolvedValue({
      data: { access_token: 'legacy-token', expires_in: 1800 }
    });
    const client = makeGrantClient({
      clientSecret: 'legacy-client-secret',
      credentialRef: 'keychain:instance/prod/client-secret',
      credentialStore
    });

    await expect(client.getAuthHeader()).resolves.toBe('Bearer legacy-token');
    expect(credentialStore.getSecret).not.toHaveBeenCalled();
    const params = Object.fromEntries(new URLSearchParams(tokenPost.mock.calls[0][1]));
    expect(params.client_secret).toBe('legacy-client-secret');
  });

  test.each([
    [401, 'AUTHENTICATION_FAILED'],
    [403, 'AUTHORIZATION_FAILED']
  ])('preserves token endpoint %s as a safe %s error', async (status, expectedCode) => {
    const tokenPost = jest.spyOn(axios, 'post').mockRejectedValue(Object.assign(new Error('token endpoint rejected client-secret-fixture'), {
      code: 'ERR_BAD_REQUEST',
      response: { status, data: { error: 'invalid_client', client_secret: 'client-secret-fixture' } }
    }));
    const client = makeGrantClient({ clientSecret: 'client-secret-fixture' });
    await expect(client._getOAuthToken()).rejects.toMatchObject({ status, code: expectedCode });
    expect(tokenPost).toHaveBeenCalledTimes(1);
  });
  test('rejects an old 401 after instance switch without retrying with the new token', async () => {
    const client = makeGrantClient({ clientSecret: 'old-client-secret' });
    client.oauthToken = 'old-access-token';
    client.oauthTokenExpiry = Date.now() + 300000;
    const oldClient = client.client;
    let release401;
    const responsePending = new Promise((resolve, reject) => { release401 = reject; });
    const adapter = jest.fn(config => responsePending.then(() => ({
      status: 200,
      data: { result: [] },
      headers: {},
      config
    })));
    oldClient.defaults.adapter = adapter;
    const request = oldClient.get('/api/now/table/sys_user');
    await new Promise(resolve => setImmediate(resolve));
    expect(adapter).toHaveBeenCalledTimes(1);
    expect(adapter.mock.calls[0][0].headers.Authorization).toBe('Bearer old-access-token');

    client.setInstance('https://prod.service-now.com', null, null, 'prod', {
      authType: 'oauth',
      grantType: 'client_credentials',
      clientId: 'cid',
      clientSecret: 'new-client-secret'
    });
    const oldConfig = adapter.mock.calls[0][0];
    const unauthorized = new Error('old instance unauthorized');
    unauthorized.config = oldConfig;
    unauthorized.response = {
      status: 401,
      data: { error: { authorization: 'Bearer old-access-token' } },
      config: oldConfig
    };
    release401(unauthorized);

    await expect(request).rejects.toMatchObject({ code: 'INSTANCE_STALE_DURING_REQUEST' });
    expect(adapter).toHaveBeenCalledTimes(1);
    expect(client.oauthToken).toBeNull();
    expect(oldConfig.headers.Authorization).toBe('Bearer old-access-token');
  });
  test.each([200, 403, 500])(
    'rejects a stale old-generation %s response before it reaches the caller',
    async (status) => {
      const client = makeGrantClient({ clientSecret: 'old-client-secret' });
      client.oauthToken = 'old-access-token';
      client.oauthTokenExpiry = Date.now() + 300000;
      const oldClient = client.client;
      let releaseResponse;
      const responsePending = new Promise(resolve => { releaseResponse = resolve; });
      const adapter = jest.fn(config => responsePending.then(() => {
        if (status < 400) {
          return { status, data: { result: ['stale'] }, headers: {}, config };
        }
        const error = new Error(`old ${status} response`);
        error.config = config;
        error.response = { status, data: { error: { message: 'stale' } }, config };
        throw error;
      }));
      oldClient.defaults.adapter = adapter;

      const request = oldClient.get('/api/now/table/sys_user');
      await new Promise(resolve => setImmediate(resolve));
      expect(adapter).toHaveBeenCalledTimes(1);

      client.setInstance('https://prod.service-now.com', null, null, 'prod', {
        authType: 'oauth',
        grantType: 'client_credentials',
        clientId: 'new-cid',
        clientSecret: 'new-client-secret'
      });
      releaseResponse();

      await expect(request).rejects.toMatchObject({ code: 'INSTANCE_STALE_DURING_REQUEST' });
    }
  );

  test.each([
    '/api/now/table/sys_user',
    'api/now/table/sys_user'
  ])('accepts relative request URL %s for an instance path prefix', async (url) => {
    const client = new ServiceNowClient('https://example.service-now.com/tenant', 'user', 'password');
    const adapter = jest.fn(async config => ({
      status: 200,
      data: { result: [] },
      headers: {},
      config
    }));
    client.client.defaults.adapter = adapter;

    await expect(client.client.get(url)).resolves.toMatchObject({
      data: { result: [] }
    });
    expect(adapter).toHaveBeenCalledTimes(1);
  });

  test.each([
    'https://evil.example.com/api/now/table/sys_user',
    'https://example.service-now.com/api/now/table/sys_user'
  ])('rejects absolute request URL outside the captured instance base: %s', async (url) => {
    const client = new ServiceNowClient('https://example.service-now.com/tenant', 'user', 'password');
    const adapter = jest.fn(async config => ({
      status: 200,
      data: { result: [] },
      headers: {},
      config
    }));
    client.client.defaults.adapter = adapter;

    await expect(client.client.get(url)).rejects.toMatchObject({ code: 'INSTANCE_STALE_DURING_REQUEST' });
    expect(adapter).not.toHaveBeenCalled();
  });
});

test('returns a successful client-credentials refresh without falling back to another grant', async () => {
  const tokenPost = jest.spyOn(axios, 'post')
    .mockResolvedValueOnce({ data: { access_token: 'refreshed-client-token', refresh_token: 'rotated-client-refresh', expires_in: 1800 } })
    .mockResolvedValueOnce({ data: { access_token: 'fallback-token', expires_in: 1800 } });
  const client = makeGrantClient({
    clientSecret: 'client-secret-fixture',
    tokenUrl: 'https://oauth.example.com/refresh-token'
  });
  client.oauthRefreshToken = 'stored-client-refresh';

  await expect(client._getOAuthToken()).resolves.toBe('refreshed-client-token');

  expect(tokenPost).toHaveBeenCalledTimes(1);
  expect(tokenPost.mock.calls[0][0]).toBe('https://oauth.example.com/refresh-token');
  const params = Object.fromEntries(new URLSearchParams(tokenPost.mock.calls[0][1]));
  expect(params).toMatchObject({
    grant_type: 'refresh_token',
    refresh_token: 'stored-client-refresh',
    client_secret: 'client-secret-fixture'
  });
});

test('returns a successful password-grant refresh without falling back to another grant', async () => {
  const credentialStore = {
    getSecret: jest.fn(async ref => ({
      'keychain:instance/dev/password': 'password-fixture',
      'keychain:instance/dev/client-secret': 'client-secret-fixture'
    }[ref]))
  };
  const tokenPost = jest.spyOn(axios, 'post')
    .mockResolvedValueOnce({ data: { access_token: 'refreshed-password-token', refresh_token: 'rotated-password-refresh', expires_in: 1800 } })
    .mockResolvedValueOnce({ data: { access_token: 'fallback-token', expires_in: 1800 } });
  const client = makeGrantClient({
    username: 'dev-user',
    credentialRef: {
      password: 'keychain:instance/dev/password',
      clientSecret: 'keychain:instance/dev/client-secret'
    },
    credentialStore,
    grantType: 'password',
    tokenUrl: 'https://oauth.example.com/password-refresh-token'
  });
  client.oauthRefreshToken = 'stored-password-refresh';

  await expect(client._getOAuthToken()).resolves.toBe('refreshed-password-token');

  expect(tokenPost).toHaveBeenCalledTimes(1);
  expect(tokenPost.mock.calls[0][0]).toBe('https://oauth.example.com/password-refresh-token');
  const params = Object.fromEntries(new URLSearchParams(tokenPost.mock.calls[0][1]));
  expect(params).toMatchObject({
    grant_type: 'refresh_token',
    refresh_token: 'stored-password-refresh',
    client_secret: 'client-secret-fixture'
  });
});
