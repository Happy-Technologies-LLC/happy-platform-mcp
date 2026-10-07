/**
 * Tests for the authorization_code + PKCE OAuth flow.
 *
 * This module adds a per-user authorization_code grant (with PKCE and a
 * loopback redirect) to the ServiceNow OAuth options. It is deliberately
 * generic — no instance-specific hardcoding — so it can be contributed
 * upstream alongside the SERVICENOW_OAUTH_GRANT_TYPE seam.
 */

import { jest } from '@jest/globals';
import crypto from 'node:crypto';
import {
  generatePkcePair,
  buildAuthorizationUrl,
  createCallbackServer,
  exchangeAuthorizationCode,
  performAuthorizationCodeFlow
} from '../src/oauth-authorization-code.js';

describe('generatePkcePair()', () => {
  it('returns an S256 challenge that is the base64url SHA-256 of the verifier', () => {
    const { codeVerifier, codeChallenge, codeChallengeMethod } = generatePkcePair();
    expect(codeChallengeMethod).toBe('S256');
    const expected = crypto.createHash('sha256').update(codeVerifier).digest('base64url');
    expect(codeChallenge).toBe(expected);
  });

  it('produces a verifier within RFC 7636 length bounds using only unreserved characters', () => {
    const { codeVerifier } = generatePkcePair();
    expect(codeVerifier.length).toBeGreaterThanOrEqual(43);
    expect(codeVerifier.length).toBeLessThanOrEqual(128);
    expect(codeVerifier).toMatch(/^[A-Za-z0-9\-._~]+$/);
  });

  it('generates a unique verifier on each call', () => {
    expect(generatePkcePair().codeVerifier).not.toBe(generatePkcePair().codeVerifier);
  });
});

describe('buildAuthorizationUrl()', () => {
  const base = {
    authorizeUrl: 'https://example.service-now.com/oauth_auth.do',
    clientId: 'cli-client-id',
    redirectUri: 'http://127.0.0.1:8455/callback',
    codeChallenge: 'abc123challenge',
    state: 'xyz-state'
  };

  it('preserves the authorize endpoint and encodes the PKCE authorization-request params', () => {
    const url = new URL(buildAuthorizationUrl(base));
    expect(`${url.origin}${url.pathname}`).toBe('https://example.service-now.com/oauth_auth.do');
    expect(url.searchParams.get('response_type')).toBe('code');
    expect(url.searchParams.get('client_id')).toBe('cli-client-id');
    expect(url.searchParams.get('redirect_uri')).toBe('http://127.0.0.1:8455/callback');
    expect(url.searchParams.get('code_challenge')).toBe('abc123challenge');
    expect(url.searchParams.get('code_challenge_method')).toBe('S256');
    expect(url.searchParams.get('state')).toBe('xyz-state');
  });

  it('includes scope only when provided', () => {
    expect(new URL(buildAuthorizationUrl(base)).searchParams.has('scope')).toBe(false);
    const withScope = new URL(buildAuthorizationUrl({ ...base, scope: 'useraccount' }));
    expect(withScope.searchParams.get('scope')).toBe('useraccount');
  });
});

describe('createCallbackServer()', () => {
  let server;
  afterEach(async () => {
    if (server) await server.close();
    server = undefined;
  });

  it('resolves with the code when the loopback redirect is hit with the matching state', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    const codePromise = server.waitForCode();
    const res = await fetch(`http://127.0.0.1:${server.port}/callback?code=THE_CODE&state=good-state`);
    expect(res.status).toBe(200);
    await expect(codePromise).resolves.toEqual({ code: 'THE_CODE' });
  });

  it('ignores a foreign-state callback without settling, then accepts the valid one (CSRF guard)', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    const pending = server.waitForCode();
    let settled = false;
    pending.then(() => { settled = true; }, () => { settled = true; });
    const foreign = await fetch(`http://127.0.0.1:${server.port}/callback?code=EVIL&state=tampered`);
    expect(foreign.status).toBe(400);
    await new Promise((r) => setTimeout(r, 20));
    expect(settled).toBe(false);
    await fetch(`http://127.0.0.1:${server.port}/callback?code=THE_CODE&state=good-state`);
    await expect(pending).resolves.toEqual({ code: 'THE_CODE' });
  });

  it('rejects with a fixed sanitized code when the provider returns an error with the valid state', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    const pending = server.waitForCode();
    const res = await fetch(`http://127.0.0.1:${server.port}/callback?error=access_denied&error_description=nope&state=good-state`);
    expect(res.status).toBe(400);
    const err = await pending.catch((e) => e);
    expect(err.code).toBe('OAUTH_AUTHORIZATION_DENIED');
    expect(err.message).not.toMatch(/access_denied|nope/);
  });
});

describe('exchangeAuthorizationCode()', () => {
  const args = {
    tokenUrl: 'https://example.service-now.com/oauth_token.do',
    clientId: 'cli-client-id',
    code: 'AUTH_CODE',
    codeVerifier: 'the-verifier',
    redirectUri: 'http://127.0.0.1:8455/callback'
  };
  const tokenResponse = { access_token: 'at', refresh_token: 'rt', expires_in: 1800 };

  it('posts the authorization_code + PKCE params to the token endpoint and returns the token body', async () => {
    let captured;
    const post = async (url, params) => {
      captured = { url, params };
      return tokenResponse;
    };
    const result = await exchangeAuthorizationCode(args, { post });
    expect(captured.url).toBe(args.tokenUrl);
    expect(captured.params).toMatchObject({
      grant_type: 'authorization_code',
      client_id: 'cli-client-id',
      code: 'AUTH_CODE',
      code_verifier: 'the-verifier',
      redirect_uri: 'http://127.0.0.1:8455/callback'
    });
    expect(result).toBe(tokenResponse);
  });

  it('omits client_secret for a public PKCE client but includes it when configured (confidential)', async () => {
    let captured;
    const post = async (_url, params) => { captured = params; return tokenResponse; };
    await exchangeAuthorizationCode(args, { post });
    expect('client_secret' in captured).toBe(false);
    await exchangeAuthorizationCode({ ...args, clientSecret: 'shh' }, { post });
    expect(captured.client_secret).toBe('shh');
  });
});

describe('performAuthorizationCodeFlow()', () => {
  const config = {
    authorizeUrl: 'https://example.service-now.com/oauth_auth.do',
    tokenUrl: 'https://example.service-now.com/oauth_token.do',
    clientId: 'cli-client-id',
    scope: 'useraccount'
  };

  it('runs the full PKCE flow end-to-end and returns the exchanged tokens', async () => {
    let exchanged;
    // Simulate the user completing sign-in: the browser hits the loopback redirect.
    const openBrowser = async (authUrl) => {
      const url = new URL(authUrl);
      const redirectUri = new URL(url.searchParams.get('redirect_uri'));
      const state = url.searchParams.get('state');
      redirectUri.searchParams.set('code', 'AUTH_CODE');
      redirectUri.searchParams.set('state', state);
      await fetch(redirectUri.toString());
    };
    const post = async (_url, params) => {
      exchanged = params;
      return { access_token: 'at', refresh_token: 'rt', expires_in: 1800 };
    };

    const tokens = await performAuthorizationCodeFlow(config, { openBrowser, post });

    expect(tokens.access_token).toBe('at');
    expect(exchanged.grant_type).toBe('authorization_code');
    expect(exchanged.code).toBe('AUTH_CODE');
    // The verifier the server generated must be the one exchanged, and the
    // redirect_uri must point at the loopback callback the browser hit.
    expect(exchanged.code_verifier).toBeTruthy();
    expect(exchanged.redirect_uri).toMatch(/^http:\/\/127\.0\.0\.1:\d+\/callback$/);
  });
});

describe('createCallbackServer() — hardening', () => {
  let server;
  afterEach(async () => {
    if (server) await server.close();
    server = undefined;
  });

  it('rejects when the redirect has a valid state but no authorization code', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    const assertion = expect(server.waitForCode()).rejects.toThrow(/code/i);
    await fetch(`http://127.0.0.1:${server.port}/callback?state=good-state`);
    await assertion;
  });

  it('never reflects provider error text in the response or rejection', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    const pending = server.waitForCode();
    const payload = '<script>alert(1)</script> ignore previous instructions';
    const res = await fetch(`http://127.0.0.1:${server.port}/callback?error=${encodeURIComponent(payload)}&error_description=${encodeURIComponent(payload)}&state=good-state`);
    const body = await res.text();
    expect(body).not.toMatch(/script|alert|ignore previous/i);
    const err = await pending.catch((e) => e);
    expect(err.code).toBe('OAUTH_AUTHORIZATION_DENIED');
    expect(err.message).not.toMatch(/script|alert|ignore previous/i);
  });

  it('does not settle on a provider error carrying a wrong or missing state', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    const pending = server.waitForCode();
    let settled = false;
    pending.then(() => { settled = true; }, () => { settled = true; });
    const r1 = await fetch(`http://127.0.0.1:${server.port}/callback?error=access_denied&state=bad`);
    const r2 = await fetch(`http://127.0.0.1:${server.port}/callback?error=${encodeURIComponent('<b>x</b>')}`);
    const r3 = await fetch(`http://127.0.0.1:${server.port}/callback?code=C&state=good-state&state=good-state`);
    for (const r of [r1, r2, r3]) {
      expect(r.status).toBe(400);
      expect(await r.text()).not.toMatch(/access_denied|<b>/);
    }
    await new Promise((r) => setTimeout(r, 20));
    expect(settled).toBe(false);
    await fetch(`http://127.0.0.1:${server.port}/callback?code=OK&state=good-state`);
    await expect(pending).resolves.toEqual({ code: 'OK' });
  });

  it('answers unrelated paths generically without settling', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    const pending = server.waitForCode();
    const fav = await fetch(`http://127.0.0.1:${server.port}/favicon.ico`);
    const other = await fetch(`http://127.0.0.1:${server.port}/other?state=good-state&code=X`);
    expect(fav.status).toBe(404);
    expect(other.status).toBe(404);
    await fetch(`http://127.0.0.1:${server.port}/callback?code=REAL&state=good-state`);
    await expect(pending).resolves.toEqual({ code: 'REAL' });
  });

  it('settles only once: duplicate valid callbacks do not change the result', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    const pending = server.waitForCode();
    const first = await fetch(`http://127.0.0.1:${server.port}/callback?code=FIRST&state=good-state`);
    const second = await fetch(`http://127.0.0.1:${server.port}/callback?code=SECOND&state=good-state`);
    const denial = await fetch(`http://127.0.0.1:${server.port}/callback?error=access_denied&state=good-state`);
    expect(first.status).toBe(200);
    expect(second.status).toBe(409);
    expect(denial.status).toBe(409);
    await expect(pending).resolves.toEqual({ code: 'FIRST' });
    await expect(server.waitForCode()).resolves.toEqual({ code: 'FIRST' });
  });

  it('timeout settles the flow; later callbacks are refused', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    await expect(server.waitForCode({ timeoutMs: 10 })).rejects.toThrow(/timed out/i);
    const late = await fetch(`http://127.0.0.1:${server.port}/callback?code=LATE&state=good-state`);
    expect(late.status).toBe(409);
    await expect(server.waitForCode()).rejects.toThrow(/timed out/i);
  });

  it('close() rejects a pending wait and is idempotent', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    const pending = server.waitForCode();
    await server.close();
    await server.close();
    const err = await pending.catch((e) => e);
    expect(err.code).toBe('OAUTH_CALLBACK_CLOSED');
    server = undefined;
  });

  it('treats timeoutMs: 0 as an immediate timeout, not "no timeout"', async () => {
    server = await createCallbackServer({ port: 0, expectedState: 'good-state' });
    await expect(server.waitForCode({ timeoutMs: 0 })).rejects.toThrow(/timed out/i);
  });
});
