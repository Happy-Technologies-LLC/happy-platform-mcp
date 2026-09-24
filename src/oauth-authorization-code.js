/**
 * OAuth 2.0 authorization_code grant with PKCE and a loopback redirect.
 *
 * Adds a per-user, browser-based sign-in to the ServiceNow OAuth options so
 * each developer authenticates as themselves (tokens attributed to the real
 * user, not a shared service principal). Kept generic — the caller supplies
 * the authorize/token URLs, client id, redirect port and scope — so it can be
 * contributed upstream alongside the SERVICENOW_OAUTH_GRANT_TYPE seam.
 */

import crypto from 'node:crypto';
import http from 'node:http';
import axios from 'axios';

/**
 * Generate a PKCE code_verifier / code_challenge pair (RFC 7636, S256).
 *
 * The verifier is 32 random bytes base64url-encoded (43 chars — within the
 * required 43–128 range and using only unreserved characters). The challenge
 * is BASE64URL(SHA256(verifier)).
 *
 * @returns {{ codeVerifier: string, codeChallenge: string, codeChallengeMethod: 'S256' }}
 */
export function generatePkcePair() {
  const codeVerifier = crypto.randomBytes(32).toString('base64url');
  const codeChallenge = crypto.createHash('sha256').update(codeVerifier).digest('base64url');
  return { codeVerifier, codeChallenge, codeChallengeMethod: 'S256' };
}

/**
 * Build the OAuth authorization-request URL the user's browser is sent to.
 *
 * @param {object} params
 * @param {string} params.authorizeUrl - Instance authorize endpoint (e.g. .../oauth_auth.do)
 * @param {string} params.clientId
 * @param {string} params.redirectUri - Loopback redirect (e.g. http://127.0.0.1:<port>/callback)
 * @param {string} params.codeChallenge - PKCE S256 challenge
 * @param {string} params.state - Opaque CSRF/binding value echoed back on the redirect
 * @param {string} [params.scope] - Optional OAuth scope
 * @returns {string} Fully-encoded authorization URL
 */
export function buildAuthorizationUrl({ authorizeUrl, clientId, redirectUri, codeChallenge, state, scope }) {
  const url = new URL(authorizeUrl);
  url.searchParams.set('response_type', 'code');
  url.searchParams.set('client_id', clientId);
  url.searchParams.set('redirect_uri', redirectUri);
  url.searchParams.set('code_challenge', codeChallenge);
  url.searchParams.set('code_challenge_method', 'S256');
  url.searchParams.set('state', state);
  if (scope) {
    url.searchParams.set('scope', scope);
  }
  return url.toString();
}

/**
 * Start a loopback HTTP server on 127.0.0.1 to catch the OAuth redirect.
 *
 * Binds before returning so the caller can read the actual `port` (pass
 * `port: 0` for an ephemeral one) and build the redirect URI. The first
 * request to `callbackPath` resolves the `waitForCode()` promise with the
 * authorization code, after validating the returned `state` against
 * `expectedState`. Provider errors (e.g. `?error=access_denied`) and state
 * mismatches reject.
 *
 * @param {object} params
 * @param {number} [params.port=0] - Port to bind (0 = OS-assigned)
 * @param {string} [params.callbackPath='/callback'] - Redirect path to listen on
 * @param {string} params.expectedState - State value the redirect must echo back
 * @returns {Promise<{ port: number, waitForCode: (opts?: {timeoutMs?: number}) => Promise<{code: string}>, close: () => Promise<void> }>}
 */
export function createCallbackServer({ port = 0, callbackPath = '/callback', expectedState }) {
  let resolveCode;
  let rejectCode;
  const codePromise = new Promise((resolve, reject) => {
    resolveCode = resolve;
    rejectCode = reject;
  });

  const server = http.createServer((req, res) => {
    const requestUrl = new URL(req.url, 'http://127.0.0.1');
    if (requestUrl.pathname !== callbackPath) {
      res.writeHead(404).end('Not found');
      return;
    }

    const params = requestUrl.searchParams;
    const error = params.get('error');
    const state = params.get('state');
    const code = params.get('code');

    if (error) {
      respond(res, 400, `Authentication failed: ${escapeHtml(error)}`);
      rejectCode(new Error(`OAuth authorization error: ${error}`));
      return;
    }
    if (state !== expectedState) {
      respond(res, 400, 'Authentication failed: state mismatch.');
      rejectCode(new Error('OAuth state mismatch — possible CSRF; aborting.'));
      return;
    }
    if (!code) {
      respond(res, 400, 'Authentication failed: no authorization code in the redirect.');
      rejectCode(new Error('OAuth callback missing authorization code.'));
      return;
    }
    respond(res, 200, 'Authentication complete. You can close this tab and return to the terminal.');
    resolveCode({ code });
  });

  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => {
      const address = server.address();
      const close = () =>
        new Promise((res) => {
          server.close(() => res());
        });

      const waitForCode = ({ timeoutMs } = {}) => {
        if (timeoutMs == null) return codePromise;
        let timer;
        const timeout = new Promise((_, rej) => {
          timer = setTimeout(() => rej(new Error(`Timed out waiting for OAuth redirect after ${timeoutMs}ms`)), timeoutMs);
        });
        return Promise.race([codePromise, timeout]).finally(() => clearTimeout(timer));
      };

      resolve({ port: address.port, waitForCode, close });
    });
  });
}

/**
 * Exchange an authorization code (with the PKCE verifier) for tokens at the
 * OAuth token endpoint. `client_secret` is sent only when supplied, so the
 * same call serves a public PKCE client and a confidential one.
 *
 * @param {object} params
 * @param {string} params.tokenUrl
 * @param {string} params.clientId
 * @param {string} [params.clientSecret]
 * @param {string} params.code
 * @param {string} params.codeVerifier
 * @param {string} params.redirectUri
 * @param {object} [deps]
 * @param {(url: string, params: Record<string,string>) => Promise<object>} [deps.post] - HTTP POST boundary (injectable for tests)
 * @returns {Promise<object>} The token endpoint response body
 */
export async function exchangeAuthorizationCode(
  { tokenUrl, clientId, clientSecret, code, codeVerifier, redirectUri },
  { post = defaultFormPost } = {}
) {
  const params = {
    grant_type: 'authorization_code',
    client_id: clientId,
    code,
    code_verifier: codeVerifier,
    redirect_uri: redirectUri
  };
  if (clientSecret) {
    params.client_secret = clientSecret;
  }
  return post(tokenUrl, params);
}

/**
 * Run the full per-user authorization_code + PKCE flow: start a loopback
 * server, open the browser to the authorize URL, wait for the redirect, and
 * exchange the code for tokens. Returns the token endpoint response body.
 *
 * @param {object} config
 * @param {string} config.authorizeUrl
 * @param {string} config.tokenUrl
 * @param {string} config.clientId
 * @param {string} [config.clientSecret]
 * @param {string} [config.scope]
 * @param {number} [config.redirectPort=0] - Fixed loopback port (0 = ephemeral). Use a fixed port when the oauth_entity's redirect_url is registered to a specific port.
 * @param {string} [config.callbackPath='/callback']
 * @param {number} [config.timeoutMs=300000] - How long to wait for the user to complete sign-in
 * @param {object} [deps]
 * @param {(authUrl: string) => Promise<void>} [deps.openBrowser]
 * @param {(url: string, params: Record<string,string>) => Promise<object>} [deps.post]
 * @returns {Promise<object>} Token endpoint response body
 */
export async function performAuthorizationCodeFlow(
  { authorizeUrl, tokenUrl, clientId, clientSecret, scope, redirectPort = 0, callbackPath = '/callback', timeoutMs = 300000 },
  { openBrowser = defaultOpenBrowser, post = defaultFormPost } = {}
) {
  const { codeVerifier, codeChallenge } = generatePkcePair();
  const state = crypto.randomBytes(16).toString('base64url');
  const server = await createCallbackServer({ port: redirectPort, callbackPath, expectedState: state });
  try {
    const redirectUri = `http://127.0.0.1:${server.port}${callbackPath}`;
    const authUrl = buildAuthorizationUrl({ authorizeUrl, clientId, redirectUri, codeChallenge, state, scope });
    await openBrowser(authUrl);
    const { code } = await server.waitForCode({ timeoutMs });
    return await exchangeAuthorizationCode(
      { tokenUrl, clientId, clientSecret, code, codeVerifier, redirectUri },
      { post }
    );
  } finally {
    await server.close();
  }
}

/** Default browser opener: print the URL, then best-effort launch the OS browser. */
async function defaultOpenBrowser(authUrl) {
  console.error(`\nOpen this URL to sign in:\n  ${authUrl}\n`);
  const { spawn } = await import('node:child_process');
  // Never launch through a shell. On Windows the previous `shell: true` + `start`
  // ran `cmd.exe /d /s /c "start <authUrl>"` with the URL unquoted, and cmd treats
  // `&` as a command separator — the browser received only the first query
  // parameter and ServiceNow answered "Missing parameters: client_id".
  // `cmd /c start "" "<url>"` is not a fix either: Node auto-quotes an argument
  // only when it contains whitespace or quotes, not `&`. explorer.exe takes the
  // URL as a single argv entry with no shell in the path.
  const opener = process.platform === 'darwin' ? 'open'
    : process.platform === 'win32' ? 'explorer.exe'
    : 'xdg-open';
  try {
    spawn(opener, [authUrl], { stdio: 'ignore', detached: true, shell: false }).unref();
  } catch {
    // Non-fatal: the URL was printed above for manual paste.
  }
}

/** Default HTTP POST: form-encode params to the token endpoint via axios, return the body. */
async function defaultFormPost(url, params) {
  const response = await axios.post(url, new URLSearchParams(params).toString(), {
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' }
  });
  return response.data;
}

function respond(res, status, message) {
  res.writeHead(status, { 'Content-Type': 'text/html; charset=utf-8' });
  res.end(`<!doctype html><html><body style="font-family:system-ui;padding:2rem"><p>${message}</p></body></html>`);
}

/** Escape HTML special characters so provider-supplied values can't inject markup. */
function escapeHtml(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}
