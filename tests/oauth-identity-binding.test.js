/**
 * Refresh-token identity binding and OAuth endpoint policy (issue #67: VULN-005, VULN-023).
 *
 * Every IdP here is a real loopback HTTP server that records each request, so
 * the assertions prove which endpoint actually received which refresh token.
 */
import http from 'node:http';
import { inspect } from 'node:util';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import axios from 'axios';
import { afterEach, beforeEach, describe, expect, jest, test } from '@jest/globals';
import { ServiceNowClient } from '../src/servicenow-client.js';
import { FileTokenStore, InMemoryTokenStore, KeychainTokenStore } from '../src/token-store.js';

const TRUSTED_ENV = 'SERVICENOW_OAUTH_TRUSTED_ORIGINS';
const LEGACY_ACCOUNT = name => `${os.userInfo().username}@${name}`;
const servers = [];
const tempDirs = [];
let savedTrusted;

beforeEach(() => {
  savedTrusted = process.env[TRUSTED_ENV];
  delete process.env[TRUSTED_ENV];
});

afterEach(async () => {
  if (savedTrusted === undefined) delete process.env[TRUSTED_ENV];
  else process.env[TRUSTED_ENV] = savedTrusted;
  jest.restoreAllMocks();
  await Promise.all(servers.splice(0).map(server => new Promise(resolve => server.close(resolve))));
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
});

/**
 * Fake OAuth token endpoint. Issues `rt-<label>-<n>` refresh tokens, accepts
 * only refresh tokens it issued itself and answers invalid_grant otherwise.
 */
async function fakeIdp(label, { redirectTo, rejectCode } = {}) {
  const requests = [];
  const issued = new Set();
  let counter = 0;
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', chunk => { body += chunk; });
    req.on('end', () => {
      const params = Object.fromEntries(new URLSearchParams(body));
      requests.push({ method: req.method, path: req.url, params });
      if (redirectTo) {
        res.writeHead(307, { Location: redirectTo });
        res.end();
        return;
      }
      const reply = (status, payload) => {
        res.writeHead(status, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(payload));
      };
      if (rejectCode && params.grant_type === 'authorization_code') {
        reply(400, { error: 'invalid_request' });
        return;
      }
      if (params.grant_type === 'refresh_token' && !issued.has(params.refresh_token)) {
        reply(400, { error: 'invalid_grant' });
        return;
      }
      counter += 1;
      const refreshToken = `rt-${label}-${counter}`;
      issued.add(refreshToken);
      reply(200, { access_token: `at-${label}-${counter}`, refresh_token: refreshToken, expires_in: 1800 });
    });
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  servers.push(server);
  const origin = `http://127.0.0.1:${server.address().port}`;
  return {
    origin,
    requests,
    refreshTokensReceived: () => requests
      .filter(request => request.params.grant_type === 'refresh_token')
      .map(request => request.params.refresh_token)
  };
}

/** Stand-in for the browser sign-in: performs the code exchange via the supplied post seam. */
function fakeBrowserFlow({ beforeExchange } = {}) {
  const calls = [];
  const fallbackPost = async (url, params) => (await axios.post(url, new URLSearchParams(params).toString(), {
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' }
  })).data;
  const flow = async (config, deps) => {
    calls.push(config);
    if (beforeExchange) beforeExchange();
    const post = deps?.post ?? fallbackPost;
    return post(config.tokenUrl, {
      grant_type: 'authorization_code',
      client_id: config.clientId,
      code: `code-${calls.length}`,
      code_verifier: 'verifier',
      redirect_uri: 'http://127.0.0.1/callback'
    });
  };
  return { flow, calls };
}

function authCodeClient({ name = 'dev', url, clientId = 'public-client', store, flow, tokenUrl, authorizeUrl }) {
  const client = new ServiceNowClient(url, null, null, {
    authType: 'oauth',
    grantType: 'authorization_code',
    clientId,
    tokenStore: store,
    performAuthCodeFlow: flow,
    ...(tokenUrl === undefined ? {} : { tokenUrl }),
    ...(authorizeUrl === undefined ? {} : { authorizeUrl })
  });
  client.currentInstanceName = name;
  return client;
}

describe('refresh-token identity binding', () => {
  test('same-named instances on different origins never share or send each other refresh tokens', async () => {
    const store = new InMemoryTokenStore();
    const idpA = await fakeIdp('a');
    const idpB = await fakeIdp('b');
    const flowA = fakeBrowserFlow();
    const flowB = fakeBrowserFlow();

    await authCodeClient({ url: idpA.origin, store, flow: flowA.flow })._getOAuthToken();
    await authCodeClient({ url: idpB.origin, store, flow: flowB.flow })._getOAuthToken();

    expect(idpB.refreshTokensReceived()).toEqual([]);
    expect(flowB.calls).toHaveLength(1);

    // A's own identity still refreshes silently with A's token at A's endpoint.
    const flowA2 = fakeBrowserFlow();
    const token = await authCodeClient({ url: idpA.origin, store, flow: flowA2.flow })._getOAuthToken();
    expect(token).toBe('at-a-2');
    expect(flowA2.calls).toHaveLength(0);
    expect(idpA.refreshTokensReceived()).toEqual(['rt-a-1']);
  });

  test('renaming keeps the identity, and reusing a display name for another client gets nothing', async () => {
    const store = new InMemoryTokenStore();
    const idp = await fakeIdp('a');

    await authCodeClient({ name: 'dev', url: idp.origin, store, flow: fakeBrowserFlow().flow })._getOAuthToken();

    const renamedFlow = fakeBrowserFlow();
    await authCodeClient({ name: 'renamed', url: idp.origin, store, flow: renamedFlow.flow })._getOAuthToken();
    expect(renamedFlow.calls).toHaveLength(0);
    expect(idp.refreshTokensReceived()).toEqual(['rt-a-1']);

    const impostorFlow = fakeBrowserFlow();
    await authCodeClient({ name: 'dev', url: idp.origin, clientId: 'other-client', store, flow: impostorFlow.flow })._getOAuthToken();
    expect(impostorFlow.calls).toHaveLength(1);
    expect(idp.refreshTokensReceived()).toEqual(['rt-a-1']);
    expect(idp.requests.filter(request => request.params.client_id === 'other-client')
      .every(request => request.params.grant_type === 'authorization_code')).toBe(true);
  });

  test.each([
    ['client id', () => ({ clientId: 'rotated-client' })],
    ['token endpoint', origin => ({ tokenUrl: `${origin}/alternate_token.do` })],
    ['authorize endpoint', origin => ({ authorizeUrl: `${origin}/alternate_auth.do` })]
  ])('a changed %s is a new identity that never receives the previous refresh token', async (_label, change) => {
    const store = new InMemoryTokenStore();
    const idp = await fakeIdp('a');
    await authCodeClient({ url: idp.origin, store, flow: fakeBrowserFlow().flow })._getOAuthToken();

    const changedFlow = fakeBrowserFlow();
    await authCodeClient({ url: idp.origin, store, flow: changedFlow.flow, ...change(idp.origin) })._getOAuthToken();

    expect(changedFlow.calls).toHaveLength(1);
    expect(idp.refreshTokensReceived()).toEqual([]);
  });

  test('an approved external token endpoint is a separate identity from the instance-hosted one', async () => {
    const store = new InMemoryTokenStore();
    const instance = await fakeIdp('instance');
    const external = await fakeIdp('external');
    process.env[TRUSTED_ENV] = external.origin;

    await authCodeClient({ url: instance.origin, store, flow: fakeBrowserFlow().flow })._getOAuthToken();
    const externalFlow = fakeBrowserFlow();
    await authCodeClient({
      url: instance.origin,
      store,
      flow: externalFlow.flow,
      tokenUrl: `${external.origin}/token`
    })._getOAuthToken();

    expect(externalFlow.calls).toHaveLength(1);
    expect(external.refreshTokensReceived()).toEqual([]);
    expect(external.requests.map(request => request.params.grant_type)).toEqual(['authorization_code']);
  });

  test('legacy name-only entries are never read or sent, and are cleared after reauthorization', async () => {
    const store = new InMemoryTokenStore();
    await store.setRefreshToken(LEGACY_ACCOUNT('dev'), 'legacy-refresh-token');
    const idp = await fakeIdp('a');
    const flow = fakeBrowserFlow();

    await authCodeClient({ url: idp.origin, store, flow: flow.flow })._getOAuthToken();

    expect(flow.calls).toHaveLength(1);
    expect(idp.refreshTokensReceived()).toEqual([]);
    expect(JSON.stringify(idp.requests)).not.toContain('legacy-refresh-token');
    expect(await store.getRefreshToken(LEGACY_ACCOUNT('dev'))).toBeNull();
  });

  test('persists under a versioned hashed key accepted by the file store', async () => {
    const baseDir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-identity-store-'));
    tempDirs.push(baseDir);
    const store = new FileTokenStore({ baseDir: path.join(baseDir, 'tokens') });
    const spy = jest.spyOn(store, 'setRefreshToken');
    const idp = await fakeIdp('a');

    await authCodeClient({ url: idp.origin, store, flow: fakeBrowserFlow().flow })._getOAuthToken();

    expect(spy).toHaveBeenCalledTimes(1);
    const [account] = spy.mock.calls[0];
    expect(account).toMatch(/^identity-v1-[0-9a-f]{64}$/);
    expect(account).not.toContain('dev');
    expect(account).not.toContain(os.userInfo().username);

    const flow = fakeBrowserFlow();
    const refreshed = await authCodeClient({ name: 'other', url: idp.origin, store, flow: flow.flow })._getOAuthToken();
    expect(refreshed).toBe('at-a-2');
    expect(flow.calls).toHaveLength(0);
  });
});

describe('OAuth endpoint origin policy', () => {
  test('rejects an unapproved external token endpoint before sending any refresh token', async () => {
    const instance = await fakeIdp('instance');
    const attacker = await fakeIdp('attacker');
    const flow = fakeBrowserFlow();
    const client = authCodeClient({
      url: instance.origin,
      store: new InMemoryTokenStore(),
      flow: flow.flow,
      tokenUrl: `${attacker.origin}/token`
    });
    client.oauthRefreshToken = 'in-memory-refresh-token';

    await expect(client._getOAuthToken()).rejects.toMatchObject({ code: 'OAUTH_ENDPOINT_NOT_APPROVED' });
    expect(attacker.requests).toEqual([]);
    expect(flow.calls).toHaveLength(0);
  });

  test('rejects an unapproved external authorize endpoint before opening the sign-in', async () => {
    const instance = await fakeIdp('instance');
    const flow = fakeBrowserFlow();
    const client = authCodeClient({
      url: instance.origin,
      store: new InMemoryTokenStore(),
      flow: flow.flow,
      authorizeUrl: 'https://login.attacker.example/authorize'
    });

    await expect(client._getOAuthToken()).rejects.toMatchObject({ code: 'OAUTH_ENDPOINT_NOT_APPROVED' });
    expect(flow.calls).toHaveLength(0);
    expect(instance.requests).toEqual([]);
  });

  test('never sends a confidential client secret to an unapproved external token endpoint', async () => {
    const instance = await fakeIdp('instance');
    const attacker = await fakeIdp('attacker');
    const client = new ServiceNowClient(instance.origin, null, null, {
      authType: 'oauth',
      grantType: 'client_credentials',
      clientId: 'cid',
      clientSecret: 'client-secret-fixture',
      tokenUrl: `${attacker.origin}/token`
    });

    await expect(client._getOAuthToken()).rejects.toMatchObject({ code: 'OAUTH_ENDPOINT_NOT_APPROVED' });
    expect(attacker.requests).toEqual([]);
  });

  test('allows an operator-approved external IdP and re-checks approval immediately before the exchange', async () => {
    const instance = await fakeIdp('instance');
    const external = await fakeIdp('external');
    process.env[TRUSTED_ENV] = `https://unrelated.example, ${external.origin}`;
    const store = new InMemoryTokenStore();

    const approved = await authCodeClient({
      url: instance.origin,
      store,
      flow: fakeBrowserFlow().flow,
      tokenUrl: `${external.origin}/token`,
      authorizeUrl: `${external.origin}/authorize`
    })._getOAuthToken();
    expect(approved).toBe('at-external-1');

    const revokedFlow = fakeBrowserFlow({ beforeExchange: () => { delete process.env[TRUSTED_ENV]; } });
    const revoked = authCodeClient({
      url: instance.origin,
      store: new InMemoryTokenStore(),
      flow: revokedFlow.flow,
      clientId: 'second-client',
      tokenUrl: `${external.origin}/token`,
      authorizeUrl: `${external.origin}/authorize`
    });
    await expect(revoked._getOAuthToken()).rejects.toMatchObject({ code: 'OAUTH_ENDPOINT_NOT_APPROVED' });
    expect(revokedFlow.calls).toHaveLength(1);
    expect(external.requests.filter(request => request.params.client_id === 'second-client')).toEqual([]);
  });

  test('rejects malformed or wildcard trusted-origin configuration instead of widening trust', async () => {
    const instance = await fakeIdp('instance');
    for (const value of ['*', 'https://idp.example/path', 'http://idp.example', 'https://user:pw@idp.example', 'not a url']) {
      process.env[TRUSTED_ENV] = value;
      const client = authCodeClient({
        url: instance.origin,
        store: new InMemoryTokenStore(),
        flow: fakeBrowserFlow().flow,
        tokenUrl: 'https://idp.example/token'
      });
      await expect(client._getOAuthToken()).rejects.toMatchObject({ code: 'OAUTH_TRUSTED_ORIGINS_INVALID' });
    }
  });

  test.each([
    ['client_credentials token request', async (instanceOrigin) => {
      const client = new ServiceNowClient(instanceOrigin, null, null, {
        authType: 'oauth', grantType: 'client_credentials', clientId: 'cid', clientSecret: 'client-secret-fixture'
      });
      return client._getOAuthToken();
    }],
    ['authorization_code refresh', async (instanceOrigin) => {
      const client = authCodeClient({ url: instanceOrigin, store: new InMemoryTokenStore(), flow: fakeBrowserFlow().flow });
      client.oauthRefreshToken = 'refresh-token-fixture';
      return client._getOAuthToken();
    }],
    ['authorization_code exchange', async (instanceOrigin) => {
      const client = authCodeClient({ url: instanceOrigin, store: new InMemoryTokenStore(), flow: fakeBrowserFlow().flow });
      return client._getOAuthToken();
    }]
  ])('%s never follows a redirect with credentials', async (_label, run) => {
    const sink = await fakeIdp('sink');
    const redirector = await fakeIdp('redirector', { redirectTo: `${sink.origin}/collect` });

    await expect(run(redirector.origin)).rejects.toBeTruthy();
    expect(redirector.requests.length).toBeGreaterThan(0);
    expect(sink.requests).toEqual([]);
  });

  test('a failed code exchange does not expose the authorization code or PKCE verifier', async () => {
    const idp = await fakeIdp('a', { rejectCode: true });
    const client = authCodeClient({
      url: idp.origin,
      store: new InMemoryTokenStore(),
      flow: async (config, { post }) => post(config.tokenUrl, {
        grant_type: 'authorization_code',
        client_id: config.clientId,
        code: 'auth-code-fixture-7f3a',
        code_verifier: 'pkce-verifier-fixture-91bc',
        redirect_uri: 'http://127.0.0.1/callback'
      })
    });

    let thrown;
    try {
      await client._getOAuthToken();
    } catch (error) {
      thrown = error;
    }

    expect(thrown.status).toBe(400);
    expect(idp.requests).toHaveLength(1);
    const rendered = `${JSON.stringify(thrown)}\n${inspect(thrown, { depth: 12, showHidden: true })}`;
    expect(rendered).not.toContain('auth-code-fixture-7f3a');
    expect(rendered).not.toContain('pkce-verifier-fixture-91bc');
  });
});

describe('token store backends', () => {
  test('keychain-backed store acquires and refreshes under the identity key', async () => {
    const entries = new Map();
    const store = new KeychainTokenStore({
      createEntry: (service, account) => ({
        getPassword: async () => entries.get(`${service}/${account}`),
        setPassword: async value => { entries.set(`${service}/${account}`, value); },
        deletePassword: async () => entries.delete(`${service}/${account}`)
      })
    });
    const idp = await fakeIdp('a');

    await authCodeClient({ url: idp.origin, store, flow: fakeBrowserFlow().flow })._getOAuthToken();
    const flow = fakeBrowserFlow();
    expect(await authCodeClient({ url: idp.origin, store, flow: flow.flow })._getOAuthToken()).toBe('at-a-2');

    expect(flow.calls).toHaveLength(0);
    expect([...entries.keys()]).toEqual([expect.stringMatching(/^happy-platform-mcp\/identity-v1-[0-9a-f]{64}$/)]);
    expect([...entries.values()]).toEqual(['rt-a-2']);
  });

  test('keychain and file stores reject account keys outside the shared account grammar', async () => {
    const store = new KeychainTokenStore({ createEntry: () => { throw new Error('must not open an entry'); } });
    for (const account of ['', '../escape', 'with space', 'x'.repeat(201)]) {
      await expect(store.getRefreshToken(account)).rejects.toThrow('unsafe account key');
      await expect(store.setRefreshToken(account, 'token')).rejects.toThrow('unsafe account key');
      await expect(store.clearRefreshToken(account)).rejects.toThrow('unsafe account key');
    }
  });
});
