#!/usr/bin/env node
// Native OAuth sign-in smoke (issue #67, VULN-004).
//
// Runs the real authorization-code flow with the DEFAULT browser opener
// (explorer.exe on Windows, open on macOS, xdg-open on Linux) against a
// loopback fake IdP. The fake authorize endpoint verifies that every query
// parameter survived the hand-off to the OS (the old Windows `start` opener
// truncated the URL at the first `&`) and redirects to the loopback callback,
// so the system browser itself completes the flow. The token endpoint checks
// PKCE. No real IdP, credentials or ServiceNow instance are involved.
import crypto from 'node:crypto';
import http from 'node:http';
import { pathToFileURL } from 'node:url';
import { performAuthorizationCodeFlow } from '../src/oauth-authorization-code.js';

const CLIENT_ID = 'native-signin-smoke';
const REQUIRED_PARAMS = ['response_type', 'client_id', 'redirect_uri', 'code_challenge', 'code_challenge_method', 'state', 'scope'];

export async function runSignInSmoke({ openBrowser, timeoutMs = 120000 } = {}) {
  const evidence = { authorizeRequests: 0, userAgent: null, tokenExchanged: false };
  const code = crypto.randomBytes(16).toString('base64url');
  let challenge = null;

  const idp = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1');
    if (req.method === 'GET' && url.pathname === '/oauth_auth.do') {
      evidence.authorizeRequests += 1;
      evidence.userAgent = req.headers['user-agent'] || null;
      const missing = REQUIRED_PARAMS.filter((name) => !url.searchParams.get(name));
      if (missing.length || url.searchParams.get('client_id') !== CLIENT_ID ||
          url.searchParams.get('code_challenge_method') !== 'S256') {
        evidence.authorizeError = `missing or invalid params: ${missing.join(',') || 'client_id/method'}`;
        res.writeHead(400, { 'content-type': 'text/plain' }).end(evidence.authorizeError);
        return;
      }
      challenge = url.searchParams.get('code_challenge');
      const redirect = new URL(url.searchParams.get('redirect_uri'));
      redirect.searchParams.set('code', code);
      redirect.searchParams.set('state', url.searchParams.get('state'));
      res.writeHead(302, { location: redirect.href }).end();
      return;
    }
    if (req.method === 'POST' && url.pathname === '/oauth_token.do') {
      let body = '';
      req.on('data', (chunk) => { body += chunk; });
      req.on('end', () => {
        const form = new URLSearchParams(body);
        const verifier = form.get('code_verifier') || '';
        const derived = crypto.createHash('sha256').update(verifier).digest('base64url');
        if (form.get('grant_type') !== 'authorization_code' || form.get('code') !== code || derived !== challenge) {
          res.writeHead(400, { 'content-type': 'application/json' }).end(JSON.stringify({ error: 'invalid_grant' }));
          return;
        }
        evidence.tokenExchanged = true;
        res.writeHead(200, { 'content-type': 'application/json' })
          .end(JSON.stringify({ access_token: 'smoke-access', refresh_token: 'smoke-refresh', token_type: 'Bearer', expires_in: 600 }));
      });
      return;
    }
    res.writeHead(404).end();
  });
  await new Promise((resolve) => idp.listen(0, '127.0.0.1', resolve));
  const base = `http://127.0.0.1:${idp.address().port}`;

  try {
    const tokens = await performAuthorizationCodeFlow(
      { authorizeUrl: `${base}/oauth_auth.do`, tokenUrl: `${base}/oauth_token.do`, clientId: CLIENT_ID, scope: 'useraccount', timeoutMs },
      openBrowser ? { openBrowser } : {}
    );
    return { ok: tokens?.access_token === 'smoke-access' && evidence.tokenExchanged, tokens: Boolean(tokens?.access_token), ...evidence };
  } finally {
    idp.closeAllConnections?.();
    await new Promise((resolve) => idp.close(resolve));
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const requireBrowser = process.argv.includes('--require-browser');
  try {
    const result = await runSignInSmoke();
    const browser = /Mozilla\//.test(result.userAgent || '');
    console.log(JSON.stringify({ platform: process.platform, ...result, realBrowser: browser }, null, 2));
    process.exit(result.ok && (!requireBrowser || browser) ? 0 : 1);
  } catch (error) {
    console.error(`sign-in smoke failed: ${error.code || ''} ${error.message}`);
    process.exit(1);
  }
}
